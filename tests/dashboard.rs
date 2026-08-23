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
