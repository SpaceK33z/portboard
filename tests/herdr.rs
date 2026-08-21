use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn plugin_popup_uses_herdrs_active_pane_targeting() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let fake = temporary.path().join("fake-herdr");
    let log = temporary.path().join("herdr.log");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"result\":{{\"type\":\"ok\"}}}}'\n",
            log.display()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .arg("plugin-open")
        .env("HERDR_BIN_PATH", &fake)
        .env("HERDR_PLUGIN_ID", "portboard")
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"workspace_id":"w1","workspace_cwd":"/tmp/project","focused_pane_id":"w1:p1"}"#,
        )
        .output()
        .expect("plugin open");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let command = fs::read_to_string(log).expect("Herdr command");
    assert!(!command.contains("--target-pane"), "{command}");
    assert!(!command.contains("--workspace"), "{command}");
    assert!(
        command.contains("--env PORTBOARD_WORKSPACE_ID=w1"),
        "{command}"
    );
}

#[test]
fn herdr_ensure_starts_a_dedicated_tab_without_focusing_it() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    fs::write(
        repository.path().join("portboard.toml"),
        r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["herdr-process-that-is-starting"]
"#,
    )
    .expect("manifest");
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    let log = state.path().join("herdr.log");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1 $2" in
  "tab create") printf '%s\n' '{{"result":{{"tab":{{"tab_id":"w1:t2"}},"root_pane":{{"pane_id":"w1:p2"}}}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":1,"foreground_processes":[{{"pid":{}}}]}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            log.display(),
            std::process::id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("PORTBOARD_APPROVE", "1")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("Portboard ensure");

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let commands = fs::read_to_string(log).expect("Herdr commands");
    assert!(commands.contains("tab create --workspace w1"), "{commands}");
    assert!(commands.contains("pane run w1:p2 sleep 30"), "{commands}");
    assert!(!commands.contains("tab focus w1:t2"), "{commands}");
}

#[test]
fn herdr_launch_without_process_identity_fails_and_closes_the_tab() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    fs::write(
        repository.path().join("portboard.toml"),
        r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["herdr-process-that-never-appears"]
"#,
    )
    .expect("manifest");
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    let log = state.path().join("herdr.log");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1 $2" in
  "tab create") printf '%s\n' '{{"result":{{"tab":{{"tab_id":"w1:t2"}},"root_pane":{{"pane_id":"w1:p2"}}}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":1}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            log.display()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["open", "probe", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("PORTBOARD_APPROVE", "1")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("Portboard open");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("could not identify the target process"));
    let commands = fs::read_to_string(log).expect("Herdr commands");
    assert_eq!(commands.matches("pane run w1:p2 sleep 30").count(), 1);
    assert!(commands.contains("tab close w1:t2"));
    assert!(!commands.contains("tab focus w1:t2"));
}
