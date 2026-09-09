use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use url::Host;

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::{find_launch_target_processes, LaunchTargetProcess};
use crate::runtime_metadata::{load_launch_target_runtime_metadata, RuntimeEndpoint};

/// Waits for a target process and its primary HTTP endpoint to accept a request.
pub fn wait_for_launch_target_ready(
    worktree_root: &Path,
    target: &LaunchTarget,
    timeout: Duration,
) -> Result<Option<RuntimeEndpoint>> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` readiness deadline expired",
                target.id.as_str()
            );
        }
        let processes = find_launch_target_processes(worktree_root, target)?;
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` readiness deadline expired",
                target.id.as_str()
            );
        }
        if !processes.is_empty() {
            if target.runtime_file.is_none() {
                return Ok(None);
            }
            if let Some(metadata) =
                load_launch_target_runtime_metadata(worktree_root, target, &processes)?
            {
                let primary_endpoint = metadata.primary_endpoint().with_context(|| {
                    format!(
                        "Portboard launch target `{}` runtime metadata has no primary endpoint",
                        target.id.as_str()
                    )
                })?;
                if http_endpoint_is_ready_until(&primary_endpoint.url, deadline)? {
                    return Ok(Some(primary_endpoint.clone()));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` did not become ready within {} seconds",
                target.id.as_str(),
                timeout.as_secs()
            );
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Waits until a target's final configured process signature becomes visible.
pub fn wait_for_launch_target_process(
    worktree_root: &Path,
    target: &LaunchTarget,
    timeout: Duration,
) -> Result<Vec<LaunchTargetProcess>> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` readiness deadline expired",
                target.id.as_str()
            );
        }
        let processes = find_launch_target_processes(worktree_root, target)?;
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` readiness deadline expired",
                target.id.as_str()
            );
        }
        if !processes.is_empty() {
            return Ok(processes);
        }
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` process did not appear within {} seconds",
                target.id.as_str(),
                timeout.as_secs()
            );
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
fn http_endpoint_is_ready(endpoint_url: &str) -> Result<bool> {
    http_endpoint_is_ready_until(endpoint_url, Instant::now() + Duration::from_millis(500))
}

fn http_endpoint_is_ready_until(endpoint_url: &str, deadline: Instant) -> Result<bool> {
    let url = crate::browser_url::validate_browser_url(endpoint_url)?;
    if url.scheme() != "http" {
        bail!("Portboard readiness does not support HTTPS; use an http:// primary endpoint (HTTPS remains supported for browser links)");
    }
    let port = url
        .port_or_known_default()
        .context("Portboard readiness URL has no port")?;
    // Never call the system resolver: it has no cancellable deadline. Local
    // development endpoints must use an IP literal or the exact localhost name.
    let ips = match url.host().context("Portboard readiness URL has no host")? {
        Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
        Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
        Host::Domain("localhost") => vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ],
        Host::Domain(_) => {
            bail!("Portboard readiness does not resolve DNS names; use an IP literal or localhost")
        }
    };
    let deadline = deadline.min(Instant::now() + Duration::from_millis(500));
    let mut stream = None;
    for ip in ips {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        if let Ok(connection) = TcpStream::connect_timeout(
            &SocketAddr::new(ip, port),
            remaining.min(Duration::from_millis(250)),
        ) {
            stream = Some(connection);
            break;
        }
    }
    let Some(mut stream) = stream else {
        return Ok(false);
    };
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let host = url.host_str().context("missing host")?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    let mut bytes = request.as_bytes();
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(bytes) {
            Ok(0) | Err(_) => return Ok(false),
            Ok(count) => bytes = &bytes[count..],
        }
    }
    let mut line = Vec::new();
    while line.len() < 4096 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        stream.set_read_timeout(Some(remaining))?;
        let mut byte = [0];
        if !matches!(stream.read(&mut byte), Ok(1)) {
            return Ok(false);
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            let Ok(line) = std::str::from_utf8(&line) else {
                return Ok(false);
            };
            let mut parts = line.split_whitespace();
            if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
                return Ok(false);
            }
            return Ok(matches!(
                parts.next().and_then(|value| value.parse::<u16>().ok()),
                Some(200..=399)
            ));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    use crate::launch_target_config::parse_launch_target_config;

    use super::wait_for_launch_target_ready;

    #[test]
    fn probes_honor_short_absolute_deadlines_and_reject_dns_and_https() {
        for url in [
            "http://unresolvable.invalid/",
            "https://localhost/",
            "http://localhost/\x07",
        ] {
            let start = std::time::Instant::now();
            assert!(
                super::http_endpoint_is_ready_until(url, start + Duration::from_millis(50))
                    .is_err()
            );
            assert!(start.elapsed() < Duration::from_millis(100));
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let start = std::time::Instant::now();
        assert!(!super::http_endpoint_is_ready_until(
            &format!("http://{address}"),
            start + Duration::from_millis(60)
        )
        .unwrap());
        let elapsed = start.elapsed();
        server.join().unwrap();
        assert!(elapsed < Duration::from_millis(180));
    }

    #[test]
    fn trickling_and_oversized_status_lines_are_bounded() {
        for trickle in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                for _ in 0..30 {
                    if stream
                        .write_all(if trickle { b"H" } else { &[b'H'; 1024] })
                        .is_err()
                    {
                        break;
                    }
                    if trickle {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            });
            let start = std::time::Instant::now();
            assert!(!super::http_endpoint_is_ready(&format!("http://{address}")).unwrap());
            let elapsed = start.elapsed();
            server.join().unwrap();
            assert!(
                elapsed < Duration::from_millis(800),
                "probe took {elapsed:?}"
            );
        }
    }

    #[test]
    fn ipv6_probe_uses_correct_host_authority() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            };
            let mut request = [0; 1024];
            let count = stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            Some(String::from_utf8_lossy(&request[..count]).into_owned())
        });
        let result = super::http_endpoint_is_ready(&format!("http://{address}"));
        let request = server.join().unwrap();
        assert!(result.unwrap());
        assert!(request.unwrap().contains(&format!("Host: {address}\r\n")));
    }

    #[test]
    fn rejects_runtime_metadata_without_a_primary_endpoint() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-no-primary-test-{}", std::process::id());
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(worktree.path())
            .spawn()
            .expect("target process");
        fs::write(
            worktree.path().join("runtime.json"),
            format!(
                r#"{{"version":1,"targetId":"probe","pid":{},"startedAt":"2026-08-21T12:37:22.695Z","endpoints":[{{"id":"web","url":"http://127.0.0.1:1"}}]}}"#,
                child.id()
            ),
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(&format!(
            r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
runtime_file = "runtime.json"
"#
        ))
        .expect("manifest");

        let error = wait_for_launch_target_ready(
            worktree.path(),
            &config.launch_targets[0],
            Duration::from_secs(1),
        )
        .expect_err("primary endpoint is required");

        child.kill().expect("stop target");
        child.wait().expect("reap target");
        assert!(error.to_string().contains("no primary endpoint"));
    }

    #[test]
    fn waits_for_the_primary_http_endpoint() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-ready-test-{}", std::process::id());
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(worktree.path())
            .spawn()
            .expect("target process");
        let listener = TcpListener::bind("127.0.0.1:0").expect("HTTP listener");
        let port = listener.local_addr().expect("listener address").port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("HTTP connection");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).expect("HTTP request");
                request.extend_from_slice(&chunk[..read]);
                if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .expect("HTTP response");
        });
        fs::write(
            worktree.path().join("runtime.json"),
            format!(
                r#"{{"version":1,"targetId":"probe","pid":{},"startedAt":"2026-08-21T12:37:22.695Z","endpoints":[{{"id":"web","url":"http://127.0.0.1:{port}","primary":true}}]}}"#,
                child.id()
            ),
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(&format!(
            r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
runtime_file = "runtime.json"
"#
        ))
        .expect("manifest");

        let endpoint = wait_for_launch_target_ready(
            worktree.path(),
            &config.launch_targets[0],
            Duration::from_secs(2),
        )
        .expect("ready target")
        .expect("primary endpoint");

        child.kill().expect("stop target");
        child.wait().expect("reap target");
        server.join().expect("HTTP server");
        assert_eq!(endpoint.id, "web");
    }
}
