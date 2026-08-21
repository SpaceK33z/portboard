use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use url::Url;

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::find_launch_target_processes;
use crate::runtime_metadata::{load_launch_target_runtime_metadata, RuntimeEndpoint};

/// Waits for a target process and its primary HTTP endpoint to accept a request.
pub fn wait_for_launch_target_ready(
    worktree_root: &Path,
    target: &LaunchTarget,
    timeout: Duration,
) -> Result<Option<RuntimeEndpoint>> {
    let deadline = Instant::now() + timeout;
    loop {
        let processes = find_launch_target_processes(worktree_root, target)?;
        if !processes.is_empty() {
            if target.runtime_file.is_none() {
                return Ok(None);
            }
            if let Some(metadata) =
                load_launch_target_runtime_metadata(worktree_root, target, &processes)?
            {
                let Some(primary_endpoint) = metadata.primary_endpoint() else {
                    return Ok(None);
                };
                if http_endpoint_is_ready(&primary_endpoint.url)? {
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
        thread::sleep(Duration::from_millis(100));
    }
}

fn http_endpoint_is_ready(endpoint_url: &str) -> Result<bool> {
    let url = Url::parse(endpoint_url)
        .with_context(|| format!("Portboard readiness URL is invalid: {endpoint_url}"))?;
    if url.scheme() != "http" {
        bail!("Portboard readiness currently requires an http:// primary endpoint");
    }
    let host = url
        .host_str()
        .context("Portboard readiness URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("Portboard readiness URL has no port")?;
    let addresses = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("Portboard readiness could not resolve {host}:{port}"))?;
    let mut stream = None;
    for address in addresses {
        if let Ok(connection) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
            stream = Some(connection);
            break;
        }
    }
    let Some(mut stream) = stream else {
        return Ok(false);
    };
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;
    let mut status_line = String::new();
    if BufReader::new(stream).read_line(&mut status_line).is_err() {
        return Ok(false);
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok());
    Ok(matches!(status, Some(200..=399)))
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
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
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
