use portboard::{
    launch_target_config::parse_launch_target_config,
    launch_target_processes::find_launch_target_processes,
    launch_target_readiness::wait_for_launch_target_ready, launch_target_stop::stop_launch_target,
    runtime_metadata::load_launch_target_runtime_metadata,
};
use std::{
    fs,
    io::Write,
    net::TcpListener,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture(Child, portboard::process_identity::ProcessIdentity);
impl Fixture {
    fn new(child: Child) -> Self {
        let identity = portboard::process_identity::ProcessIdentity::read(child.id()).unwrap();
        Self(child, identity)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(tree) = portboard::process_identity::process_trees(&[self.1]) {
            let _ = portboard::launch_target_stop::stop_process_identities(&tree);
        }
        let _ = self.0.wait();
    }
}

#[test]
fn identity_wrapper_preserves_coordinator_metadata_url_readiness_and_stop() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let manifest = "version = 1\n[[launch_targets]]\nid = \"probe\"\nlabel = \"Probe\"\nargv = [\"sh\", \"wrapper.sh\"]\nprocess_match = [\"coordinator.sh\"]\nruntime_file = \"runtime.json\"\n";
    fs::write(root.path().join("portboard.toml"), manifest).unwrap();
    fs::write(root.path().join("wrapper.sh"), "sleep 30 &\nhelper=$!\necho $helper > helper.pid\nsh coordinator.sh &\nchild=$!\ntrap 'kill $child $helper 2>/dev/null; wait; exit' TERM\nwait\n").unwrap();
    fs::write(root.path().join("coordinator.sh"), format!("printf '%s' '{{\"version\":1,\"targetId\":\"probe\",\"pid\":'\"$$\"',\"startedAt\":\"now\",\"endpoints\":[{{\"id\":\"web\",\"url\":\"{url}\",\"primary\":true}},{{\"id\":\"api\",\"url\":\"{url}/api\"}}]}}' > runtime.json\nread line < gate\n")).unwrap();
    let gate =
        std::ffi::CString::new(root.path().join("gate").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(gate.as_ptr(), 0o600) }, 0);
    let mut wrapper = Fixture::new(
        Command::new("sh")
            .arg("wrapper.sh")
            .env("PORTBOARD_TARGET_ID", "probe")
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let duplicate = Fixture::new(
        Command::new("sleep")
            .arg("30")
            .env("PORTBOARD_TARGET_ID", "probe")
            .current_dir(root.path())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let published: serde_json::Value = loop {
        if let Ok(bytes) = fs::read(root.path().join("runtime.json")) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                break value;
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    };
    let coordinator = published["pid"].as_u64().unwrap() as u32;
    let helper: u32 = fs::read_to_string(root.path().join("helper.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let config = parse_launch_target_config(manifest).unwrap();
    let target = &config.launch_targets[0];
    let processes = find_launch_target_processes(root.path(), target).unwrap();
    assert_eq!(processes.len(), 2, "wrapper tree and independent duplicate");
    assert!(processes.iter().any(|p| p.pid == duplicate.0.id()));
    let metadata = load_launch_target_runtime_metadata(root.path(), target, &processes)
        .unwrap()
        .expect("signature coordinator metadata must survive its identity wrapper");
    assert_eq!(metadata.pid, coordinator);
    assert_eq!(metadata.endpoints.len(), 2);
    let mut stale = processes.clone();
    for process in &mut stale {
        for member in &mut process.metadata_members {
            member.start_time = member.start_time.wrapping_add(1);
        }
    }
    assert!(
        load_launch_target_runtime_metadata(root.path(), target, &stale)
            .unwrap()
            .is_none(),
        "a reused coordinator PID must not authorize metadata"
    );
    let mut stale_root = processes.clone();
    for process in &mut stale_root {
        process.start_time = process.start_time.wrapping_add(1);
    }
    assert!(
        load_launch_target_runtime_metadata(root.path(), target, &stale_root)
            .unwrap()
            .is_none(),
        "a stale root must not authorize its captured members"
    );
    let mut unrelated = published.clone();
    unrelated["pid"] = serde_json::json!(std::process::id());
    fs::write(root.path().join("runtime.json"), unrelated.to_string()).unwrap();
    assert!(
        load_launch_target_runtime_metadata(root.path(), target, &processes)
            .unwrap()
            .is_none(),
        "an unrelated live PID must not authorize metadata"
    );
    unrelated["pid"] = serde_json::json!(helper);
    fs::write(root.path().join("runtime.json"), unrelated.to_string()).unwrap();
    assert!(
        load_launch_target_runtime_metadata(root.path(), target, &processes)
            .unwrap()
            .is_none(),
        "an identity-only descendant is not a validated signature coordinator"
    );
    fs::write(root.path().join("runtime.json"), published.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["url", "probe", "--cwd"])
        .arg(root.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), url);
    let named = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["url", "probe", "api", "--cwd"])
        .arg(root.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .output()
        .unwrap();
    assert!(named.status.success());
    assert_eq!(
        String::from_utf8_lossy(&named.stdout).trim(),
        format!("{url}/api")
    );
    listener.set_nonblocking(true).unwrap();
    let responder = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let ready = wait_for_launch_target_ready(root.path(), target, Duration::from_secs(2));
    responder.join().unwrap();
    assert_eq!(ready.unwrap().unwrap().id, "web");
    let coordinator_identity =
        portboard::process_identity::ProcessIdentity::read(coordinator).unwrap();
    let helper_identity = portboard::process_identity::ProcessIdentity::read(helper).unwrap();
    stop_launch_target(root.path(), target).unwrap();
    // A wrapper's TERM trap may stop/reap a child before Portboard signals it;
    // assert actual exit rather than requiring every PID in the signaled list.
    assert!(!wrapper.1.is_live());
    assert!(!duplicate.1.is_live());
    assert!(!coordinator_identity.is_live());
    assert!(!helper_identity.is_live());
    wrapper.0.wait().unwrap();
    assert!(!std::path::Path::new(&format!("/proc/{coordinator}")).exists());
    assert!(!std::path::Path::new(&format!("/proc/{helper}")).exists());
    assert!(find_launch_target_processes(root.path(), target)
        .unwrap()
        .is_empty());
}
