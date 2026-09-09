use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral listener")
        .local_addr()
        .expect("listener address")
        .port()
}

fn start_server(
    repository: &tempfile::TempDir,
    state: &tempfile::TempDir,
    port: u16,
) -> ServerProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["serve", "--cwd"])
        .arg(repository.path())
        .args(["--bind", &format!("127.0.0.1:{port}")])
        .env("PORTBOARD_STATE_DIR", state.path())
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dashboard server");
    ServerProcess(child)
}

fn request(
    port: u16,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("dashboard connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("bounded test response");
    let token_header = token
        .map(|token| format!("X-Portboard-Token: {token}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n{token_header}\r\n{body}",
        body.len()
    )
    .expect("HTTP request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("HTTP response");
    response
}

fn wait_until_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "dashboard did not start");
        thread::sleep(Duration::from_millis(20));
    }
}

fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("HTTP response body")
}

fn dashboard_token(html: &str) -> &str {
    let token = html
        .split_once("const token = \"")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once('"').map(|(token, _)| token))
        .expect("dashboard token");
    assert_eq!(token.len(), 64, "token must contain 256 random bits");
    token
}

#[test]
fn dashboard_rejects_rebound_hosts_and_serializes_cross_server_starts() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    let marker = format!("portboard-dashboard-race-{}", std::process::id());
    let manifest = format!(
        r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["bash", "launcher.sh"]
process_match = ["{marker}"]
"#
    );
    fs::write(repository.path().join("portboard.toml"), &manifest).expect("manifest");
    fs::write(
        repository.path().join("launcher.sh"),
        format!("printf x >> starts\nsleep 2.2\nexec -a {marker} sleep 0.3\n"),
    )
    .expect("launcher");
    let state = tempfile::tempdir().expect("temporary state");
    let first_port = free_port();
    let second_port = free_port();
    let _first = start_server(&repository, &state, first_port);
    let _second = start_server(&repository, &state, second_port);
    wait_until_ready(first_port);
    wait_until_ready(second_port);

    let rebound = request(first_port, "attacker.example", "GET", "/", None, "");
    assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");
    assert!(!rebound.contains("const token"));

    let first_html = request(
        first_port,
        &format!("127.0.0.1:{first_port}"),
        "GET",
        "/",
        None,
        "",
    );
    let second_html = request(
        second_port,
        &format!("127.0.0.1:{second_port}"),
        "GET",
        "/",
        None,
        "",
    );
    let first_token = dashboard_token(response_body(&first_html)).to_string();
    let second_token = dashboard_token(response_body(&second_html)).to_string();
    assert_ne!(first_token, second_token);

    let status = request(
        first_port,
        &format!("127.0.0.1:{first_port}"),
        "GET",
        "/api/status",
        None,
        "",
    );
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let status: serde_json::Value =
        serde_json::from_str(response_body(&status)).expect("status JSON");
    assert_eq!(status["targets"][0]["id"], "probe");
    assert_eq!(status["manifest"], serde_json::Value::Null);

    let first_host = format!("127.0.0.1:{first_port}");
    let second_host = format!("127.0.0.1:{second_port}");
    let first_open = thread::spawn(move || {
        request(
            first_port,
            &first_host,
            "POST",
            "/api/open/probe",
            Some(&first_token),
            "",
        )
    });
    let second_open = thread::spawn(move || {
        request(
            second_port,
            &second_host,
            "POST",
            "/api/open/probe",
            Some(&second_token),
            "",
        )
    });
    let responses = [
        first_open.join().expect("first open"),
        second_open.join().expect("second open"),
    ];
    assert!(responses
        .iter()
        .all(|response| response.starts_with("HTTP/1.1 200")));
    assert!(responses
        .iter()
        .any(|response| response.contains("started")));
    assert!(responses
        .iter()
        .any(|response| { response.contains("already_running") || response.contains("starting") }));

    thread::sleep(Duration::from_millis(2_700));
    assert_eq!(
        fs::read_to_string(repository.path().join("starts")).expect("start marker"),
        "x"
    );
}

#[test]
fn unfinished_bodies_and_slow_connections_do_not_block_status() {
    let repository = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("portboard.toml"), "version = 1\n[[launch_targets]]\nid = \"probe\"\nlabel = \"Probe\"\nargv = [\"missing-command\"]\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let port = free_port();
    let _server = start_server(&repository, &state, port);
    wait_until_ready(port);
    let mut attacker = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        attacker,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1025\r\n\r\n"
    )
    .unwrap();
    thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    let mut healthy = TcpStream::connect(("127.0.0.1", port)).unwrap();
    healthy
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        healthy,
        "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    healthy
        .read_to_string(&mut response)
        .expect("status must not wait for attacker body");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut idle = Vec::new();
    for _ in 0..48 {
        idle.push(TcpStream::connect(("127.0.0.1", port)).unwrap());
    }
    thread::sleep(Duration::from_millis(700));
    for mut stream in idle {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0];
        assert!(
            matches!(stream.read(&mut byte), Ok(0)),
            "idle connections must close"
        );
    }
}

#[test]
fn oversized_headers_and_body_attacks_preserve_authorization_and_reaping() {
    let repository = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("portboard.toml"), "version = 1\n[[launch_targets]]\nid = \"probe\"\nlabel = \"Probe\"\nargv = [\"sh\", \"fixture.sh\"]\nprocess_match = [\"portboard-bounded-http-never-match\"]\n").unwrap();
    fs::write(
        repository.path().join("fixture.sh"),
        "echo $$ > fixture.pid\nread line < gate\n",
    )
    .unwrap();
    let gate_path = repository.path().join("gate");
    let gate_name = std::ffi::CString::new(gate_path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(gate_name.as_ptr(), 0o600) }, 0);
    struct Release(fs::File);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.write_all(b"exit\n");
        }
    }
    let release = Release(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(gate_path)
            .unwrap(),
    );
    let state = tempfile::tempdir().unwrap();
    let port = free_port();
    let server = start_server(&repository, &state, port);
    wait_until_ready(port);
    let host = format!("127.0.0.1:{port}");
    let html = request(port, &host, "GET", "/", None, "");
    let token = dashboard_token(response_body(&html));
    // On assertion failure (including the reaping mutation), ask the fixture
    // server to stop/wait its child before ServerProcess kills the server.
    struct CleanupFixture(u16, String, String);
    impl Drop for CleanupFixture {
        fn drop(&mut self) {
            let _ = std::panic::catch_unwind(|| {
                request(
                    self.0,
                    &self.1,
                    "POST",
                    "/api/stop/probe",
                    Some(&self.2),
                    "",
                )
            });
        }
    }
    let _cleanup = CleanupFixture(port, host.clone(), token.to_string());
    assert!(request(port, &host, "POST", "/api/open/probe", Some(token), "").contains("started"));
    let deadline = Instant::now() + Duration::from_secs(3);
    let fixture: u32 = loop {
        if let Ok(pid) = fs::read_to_string(repository.path().join("fixture.pid")) {
            if let Ok(pid) = pid.trim().parse() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "fixture never started");
        thread::sleep(Duration::from_millis(10));
    };
    let identity =
        portboard::process_identity::ProcessIdentity::read(fixture).expect("live fixture");
    assert!(identity.is_live());
    let owner = fs::read_dir(format!("/proc/{}/task", server.0.id()))
        .unwrap()
        .filter_map(Result::ok)
        .find(|task| {
            fs::read_to_string(task.path().join("children"))
                .unwrap_or_default()
                .split_whitespace()
                .any(|pid| pid == fixture.to_string())
        })
        .expect("fixture must be a dashboard-owned child");
    assert_ne!(
        owner.file_name().to_str().unwrap(),
        server.0.id().to_string(),
        "fixture must belong to the mutation worker, not the main task"
    );
    for (host, expected) in [
        ("attacker.example".to_string(), "421"),
        (host.clone(), "403"),
    ] {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        write!(
            stream,
            "POST /api/open/probe HTTP/1.1\r\nHost: {host}\r\nContent-Length: 999999999\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {expected}")));
    }
    let mut attacker = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        attacker,
        "GET / HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\r\n1000\r\n"
    )
    .unwrap();
    let mut oversized = TcpStream::connect(("127.0.0.1", port)).unwrap();
    oversized
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let _ =
        oversized.write_all(format!("GET / HTTP/1.1\r\nX-Large: {}", "x".repeat(20000)).as_bytes());
    let mut byte = [0];
    match oversized.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("oversized headers must close, not time out: {other:?}"),
    }
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(3);
    while fs::metadata(format!("/proc/{fixture}")).is_ok() {
        assert!(
            Instant::now() < deadline,
            "dashboard must reap actual worker-owned fixture {fixture}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!identity.is_live());
    assert!(request(port, &host, "GET", "/api/status", None, "").starts_with("HTTP/1.1 200"));
}

#[test]
fn blocked_mutation_does_not_block_status_requests() {
    use sha2::{Digest, Sha256};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let repository = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    fs::write(repository.path().join("portboard.toml"), "version = 1\n[[launch_targets]]\nid = \"probe\"\nlabel = \"Probe\"\nargv = [\"missing-command\"]\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let hash = format!(
        "{:x}",
        Sha256::digest(
            repository
                .path()
                .canonicalize()
                .unwrap()
                .as_os_str()
                .as_bytes()
        )
    );
    let locks = state.path().join("worktrees").join(hash).join("locks");
    fs::create_dir_all(&locks).unwrap();
    let lock = fs::File::create(locks.join("probe.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let port = free_port();
    let _server = start_server(&repository, &state, port);
    wait_until_ready(port);
    let host = format!("127.0.0.1:{port}");
    let html = request(port, &host, "GET", "/", None, "");
    let token = dashboard_token(response_body(&html)).to_string();
    let stop_host = host.clone();
    let queue_token = token.clone();
    let mutation = thread::spawn(move || {
        request(
            port,
            &stop_host,
            "POST",
            "/api/stop/probe",
            Some(&token),
            "",
        )
    });
    thread::sleep(Duration::from_millis(100));
    let mut queued = Vec::new();
    for _ in 0..8 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "POST /api/stop/probe HTTP/1.1\r\nHost: {host}\r\nX-Portboard-Token: {queue_token}\r\n\r\n").unwrap();
        queued.push(stream);
        thread::sleep(Duration::from_millis(10));
    }
    let overflow = request(
        port,
        &host,
        "POST",
        "/api/stop/probe",
        Some(&queue_token),
        "",
    );
    assert!(
        overflow.starts_with("HTTP/1.1 503"),
        "bounded queue overflow: {overflow}"
    );
    let mut healthy = TcpStream::connect(("127.0.0.1", port)).unwrap();
    healthy
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    write!(healthy, "GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
    let mut response = String::new();
    let result = healthy.read_to_string(&mut response);
    drop(lock);
    assert!(mutation.join().unwrap().starts_with("HTTP/1.1 200"));
    result.expect("mutation lock must not block HTTP status");
    assert!(response.starts_with("HTTP/1.1 200"));
}
