use std::fs;
use std::process::{Command, Stdio};

fn initialize_repository(manifest: &str) -> tempfile::TempDir {
    let repository = tempfile::tempdir().expect("temporary repository");
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init");
    assert!(status.success());
    fs::write(repository.path().join("portboard.toml"), manifest).expect("manifest");
    repository
}

fn portboard(repository: &tempfile::TempDir, state: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_portboard"));
    command
        .args(["open", "probe", "--cwd"])
        .arg(repository.path())
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .env("PORTBOARD_STATE_DIR", state.path())
        .stdin(Stdio::null());
    command
}

#[test]
fn refuses_an_unapproved_noninteractive_command() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sh", "-c", "printf executed > marker"]
process_match = ["portboard-never-running-approval-test"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state");

    let output = portboard(&repository, &state)
        .output()
        .expect("portboard open");

    assert!(!output.status.success());
    assert!(!repository.path().join("marker").exists());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("requires approval"));
    assert!(stderr.contains(repository.path().to_str().expect("repository path")));
    assert!(stderr.contains(r#"["sh","-c","printf executed > marker"]"#));

    let almost_approved = portboard(&repository, &state)
        .env("PORTBOARD_APPROVE", "true")
        .output()
        .expect("strict noninteractive approval");
    assert!(!almost_approved.status.success());
    assert!(!repository.path().join("marker").exists());
}

#[test]
fn explicit_approval_is_persisted_and_manifest_changes_revoke_it() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sh", "-c", "printf first >> marker"]
process_match = ["portboard-never-running-persist-test"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state");

    let first = portboard(&repository, &state)
        .env("PORTBOARD_APPROVE", "1")
        .status()
        .expect("approved open");
    assert!(first.success());
    let second = portboard(&repository, &state)
        .status()
        .expect("persisted approval open");
    assert!(second.success());
    assert_eq!(
        fs::read_to_string(repository.path().join("marker")).expect("marker"),
        "firstfirst"
    );

    fs::write(
        repository.path().join("portboard.toml"),
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sh", "-c", "printf changed >> changed-marker"]
process_match = ["portboard-never-running-persist-test"]
"#,
    )
    .expect("changed manifest");
    let changed = portboard(&repository, &state)
        .output()
        .expect("changed open");
    assert!(!changed.status.success());
    assert!(!repository.path().join("changed-marker").exists());
}

#[test]
fn ensure_with_required_herdr_refuses_to_start_without_a_workspace() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sh", "-c", "printf executed > marker"]
process_match = ["portboard-never-running-required-herdr-test"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("PORTBOARD_APPROVE", "1")
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("Portboard ensure");

    assert!(!output.status.success());
    assert!(!repository.path().join("marker").exists());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 stderr")
        .contains("requires a Herdr workspace"));
}

#[test]
fn url_prints_the_primary_named_endpoint_for_a_running_target() {
    let marker = format!("portboard-url-test-{}", std::process::id());
    let repository = initialize_repository(&format!(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
runtime_file = "runtime.json"
"#
    ));
    let mut child = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("running target");
    fs::write(
        repository.path().join("runtime.json"),
        format!(
            r#"{{"version":1,"targetId":"probe","pid":{},"startedAt":"2026-08-21T12:37:22.695Z","endpoints":[{{"id":"web","url":"http://localhost:4123","primary":true}}]}}"#,
            child.id()
        ),
    )
    .expect("runtime metadata");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["url", "probe", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("Portboard url");

    child.kill().expect("stop target");
    child.wait().expect("reap target");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).expect("UTF-8 URL"), "http://localhost:4123\n");
}

#[test]
fn concurrent_open_starts_one_long_lived_process() {
    let marker = format!("portboard-open-race-{}", std::process::id());
    let repository = initialize_repository(&format!(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["bash", "launcher.sh"]
process_match = ["{marker}"]
"#
    ));
    fs::write(
        repository.path().join("launcher.sh"),
        format!("printf x >> starts\nsleep 2.2\nexec -a {marker} sleep 0.3\n"),
    )
    .expect("launcher script");
    let state = tempfile::tempdir().expect("temporary state");

    // Establish approval, then clear the probe run's output before exercising
    // two genuinely concurrent open calls.
    let approved = portboard(&repository, &state)
        .env("PORTBOARD_APPROVE", "1")
        .status()
        .expect("approval run");
    assert!(approved.success());
    fs::remove_file(repository.path().join("starts")).expect("clear starts");

    let mut first = portboard(&repository, &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("first open");
    let mut second = portboard(&repository, &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("second open");
    assert!(first.wait().expect("first status").success());
    assert!(second.wait().expect("second status").success());

    assert_eq!(
        fs::read_to_string(repository.path().join("starts")).expect("starts"),
        "x"
    );
}
