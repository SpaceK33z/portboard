use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

/// Mirrors `state_paths::worktree_state_directory` for an explicit state root
/// so tests never depend on ambient environment variables.
fn state_runs_directory(
    state_root: &std::path::Path,
    worktree_root: &std::path::Path,
) -> std::path::PathBuf {
    let digest = Sha256::digest(worktree_root.as_os_str().as_bytes());
    let identity = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    state_root.join("worktrees").join(identity).join("runs")
}

fn initialize_repository(manifest: &str) -> tempfile::TempDir {
    let repository = initialize_empty_repository();
    fs::write(repository.path().join("portboard.toml"), manifest).expect("manifest");
    repository
}

fn initialize_empty_repository() -> tempfile::TempDir {
    let repository = tempfile::tempdir().expect("temporary repository");
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init");
    assert!(status.success());
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
fn approve_records_trust_without_starting_the_target() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sh", "-c", "printf executed > marker"]
process_match = ["portboard-never-running-approve-command-test"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state");

    let approved = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["approve", "probe", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("PORTBOARD_APPROVE", "1")
        .output()
        .expect("Portboard approve");

    assert!(approved.status.success());
    assert!(!repository.path().join("marker").exists());
    let started = portboard(&repository, &state)
        .status()
        .expect("approved start");
    assert!(started.success());
    assert!(repository.path().join("marker").exists());
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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 URL"),
        "http://localhost:4123\n"
    );
}

#[test]
fn stop_terminates_only_the_matching_target() {
    let marker = format!("portboard-cli-stop-test-{}", std::process::id());
    let repository = initialize_repository(&format!(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
    ));
    let mut child = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("running target");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["stop", "probe", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("Portboard stop");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stop output"),
        format!("probe: stopping pid {}\n", child.id())
    );
    assert_eq!(child.wait().expect("stopped target").signal(), Some(15));

    let second = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["stop", "probe", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("idempotent Portboard stop");
    assert!(second.status.success());
    assert_eq!(
        String::from_utf8(second.stdout).expect("UTF-8 second stop output"),
        "probe: already stopped\n"
    );
}

#[test]
fn logs_prints_a_bounded_tail_from_the_project_log() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["probe"]
log_file = "logs/probe.log"
"#,
    );
    fs::create_dir(repository.path().join("logs")).expect("logs directory");
    fs::write(
        repository.path().join("logs/probe.log"),
        "first\nsecond\nthird\n",
    )
    .expect("probe log");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["logs", "probe", "--lines", "2", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("Portboard logs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 log output"),
        "second\nthird\n"
    );
}

#[test]
fn logs_rejects_a_symlink_outside_the_worktree() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
log_file = "logs/probe.log"
"#,
    );
    let outside = tempfile::NamedTempFile::new().expect("outside log");
    fs::create_dir(repository.path().join("logs")).expect("logs directory");
    symlink(outside.path(), repository.path().join("logs/probe.log")).expect("log symlink");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["logs", "probe", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("Portboard logs");

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 logs error")
        .contains("resolves outside the worktree"));
}

#[test]
fn ps_inventories_processes_with_and_without_a_manifest() {
    let marker = format!("portboard-ps-cli-test-{}", std::process::id());
    let repository = initialize_empty_repository();
    let mut child = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("inventory test process");

    // The child needs a moment to appear in /proc under the worktree cwd;
    // poll generously so slow machines keep this test reliable.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let json_output = loop {
        let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
            .args(["ps", "--json", "--cwd"])
            .arg(repository.path())
            .output()
            .expect("Portboard ps --json");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let current = String::from_utf8(output.stdout).expect("UTF-8 ps JSON");
        if current.contains(&child.id().to_string()) {
            break current;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "inventory test process was never reported"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(json_output.contains(&format!("\"pid\": {}", child.id())));
    assert!(json_output.contains(&marker));
    assert!(json_output.contains("\"target_ids\": []"));
    assert!(!json_output.contains("test-server"));

    let text = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ps", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("Portboard ps");
    assert!(text.status.success());
    let text_output = String::from_utf8(text.stdout).expect("UTF-8 ps output");
    assert!(
        text_output.contains("PID")
            && text_output.contains("TARGET")
            && text_output.contains("ARGV")
    );
    assert!(text_output.contains(&child.id().to_string()));
    assert!(text_output.contains('—'));

    fs::write(
        repository.path().join("portboard.toml"),
        format!(
            r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ),
    )
    .expect("manifest");
    let claimed = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ps", "--cwd"])
        .arg(repository.path())
        .output()
        .expect("claimed Portboard ps");
    assert!(claimed.status.success());
    let claimed_output = String::from_utf8(claimed.stdout).expect("UTF-8 claimed ps output");
    assert!(claimed_output.contains(&child.id().to_string()));
    assert!(claimed_output.contains("probe"));

    child.kill().expect("stop test process");
    child.wait().expect("reap test process");
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

#[test]
fn logs_tails_the_newest_dashboard_run_log() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["probe"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state directory");
    let canonical_root = repository.path().canonicalize().expect("canonical root");
    let runs = state_runs_directory(state.path(), &canonical_root);
    fs::create_dir_all(&runs).expect("runs directory");
    fs::write(runs.join("probe.1000.log"), "older run\n").expect("old log");
    fs::write(runs.join("probe.2000.log"), "newest run\n").expect("new log");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["logs", "probe", "--cwd"])
        .arg(repository.path())
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .env("PORTBOARD_STATE_DIR", state.path())
        .output()
        .expect("Portboard logs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 log output");
    assert!(stdout.contains("probe.2000.log"), "{stdout}");
    assert!(stdout.contains("newest run"), "{stdout}");
    assert!(!stdout.contains("older run"), "{stdout}");
}

#[test]
fn logs_explains_a_manual_run_without_portboard_logs() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["portboard-logs-manual-marker"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state directory");
    let mut child = Command::new("bash")
        .args(["-c", "exec -a portboard-logs-manual-marker sleep 30"])
        .current_dir(repository.path())
        .spawn()
        .expect("manual test process");

    // Poll generously so slow machines still observe the process in /proc.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let stdout = loop {
        let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
            .args(["logs", "probe", "--cwd"])
            .arg(repository.path())
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("PORTBOARD_WORKSPACE_ID")
            .env("PORTBOARD_STATE_DIR", state.path())
            .output()
            .expect("Portboard logs");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 log output");
        if stdout.contains("attached to the terminal") {
            break stdout;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "manual run was never reported: {stdout}"
        );
    };
    assert!(stdout.contains(&child.id().to_string()), "{stdout}");
    child.kill().expect("stop manual test process");
    child.wait().expect("reap manual test process");
}

#[test]
fn logs_fails_for_a_stopped_target_without_any_log() {
    let repository = initialize_repository(
        r#"
version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["probe"]
"#,
    );
    let state = tempfile::tempdir().expect("temporary state directory");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["logs", "probe", "--cwd"])
        .arg(repository.path())
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .env("PORTBOARD_STATE_DIR", state.path())
        .output()
        .expect("Portboard logs");

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 logs error")
        .contains("is not running"));
}
