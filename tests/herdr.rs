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
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("Portboard ensure");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(log).expect("Herdr commands");
    assert!(commands.contains("tab create --workspace w1"), "{commands}");
    assert!(
        commands.contains("--env PORTBOARD_TARGET_ID=probe"),
        "{commands}"
    );
    assert!(
        commands.contains("pane run w1:p2 PORTBOARD_TARGET_ID=probe 'sleep' '30'"),
        "{commands}"
    );
    assert!(!commands.contains("tab focus w1:t2"), "{commands}");
}

#[test]
fn herdr_ensure_rejects_a_matching_process_without_portboard_identity() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    let marker = format!("portboard-unowned-herdr-test-{}", std::process::id());
    fs::write(
        repository.path().join("portboard.toml"),
        format!(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ),
    )
    .expect("manifest");
    let mut target = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("unowned target");
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
case "$1 $2" in
  "pane list") printf '%s\n' '{{"result":{{"panes":[{{"pane_id":"w1:p2","tab_id":"w1:t2"}}]}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[]}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            target.id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard ensure");

    target.kill().expect("stop target");
    target.wait().expect("reap target");
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 stderr")
        .contains("outside its Portboard-owned Herdr tab"));
}

#[test]
fn herdr_ensure_requires_the_owned_process_to_be_in_the_herdr_tab() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    let marker = format!("portboard-mixed-ownership-test-{}", std::process::id());
    fs::write(
        repository.path().join("portboard.toml"),
        format!(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ),
    )
    .expect("manifest");
    let mut owned = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .env("PORTBOARD_TARGET_ID", "probe")
        .spawn()
        .expect("owned target");
    let mut unowned = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("unowned target");
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
case "$1 $2" in
  "pane list") printf '%s\n' '{{"result":{{"panes":[{{"pane_id":"w1:p2","tab_id":"w1:t2"}}]}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[]}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            unowned.id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard ensure");

    owned.kill().expect("stop owned target");
    unowned.kill().expect("stop unowned target");
    owned.wait().expect("reap owned target");
    unowned.wait().expect("reap unowned target");
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 stderr")
        .contains("outside its Portboard-owned Herdr tab"));
}

#[test]
fn stop_closes_the_portboard_owned_herdr_tab() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    let marker = format!("portboard-owned-stop-test-{}", std::process::id());
    fs::write(
        repository.path().join("portboard.toml"),
        format!(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ),
    )
    .expect("manifest");
    let mut target = OwnedPane::spawn(
        repository.path(),
        &format!("bash -c 'exec -a {marker} sleep 30'"),
    );
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    let log = state.path().join("herdr.log");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1 $2" in
  "pane list") printf '%s\n' '{{"result":{{"panes":[{{"pane_id":"w1:p2","tab_id":"w1:t2"}}]}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[]}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            log.display(),
            target.shell.id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["stop", "probe", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard stop");

    target.wait().expect("reap target");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(log).expect("Herdr commands");
    assert!(commands.contains("tab close w1:t2"), "{commands}");
}

#[test]
fn stop_cancels_a_reserved_launcher_before_its_process_signature_appears() {
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
process_match = ["signature-that-has-not-appeared"]
"#,
    )
    .expect("manifest");
    let mut launcher = OwnedPane::spawn(repository.path(), "env -u PORTBOARD_TARGET_ID sleep 30");
    let shell_pid = launcher.shell.id();
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
  "pane list") printf '%s\n' '{{"result":{{"panes":[{{"pane_id":"w1:p2","tab_id":"w1:t2"}}]}}}}' ;;
  "pane process-info") if kill -0 {child} 2>/dev/null; then foreground='{{"pid":{child}}}'; else foreground=''; fi; printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":{shell},"foreground_processes":['"$foreground"']}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            log.display(),
            shell = shell_pid,
            child = launcher.id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let ensured = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard ensure");
    assert!(ensured.status.success());

    let stopped = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["stop", "probe", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard stop");

    launcher.wait().expect("reap launcher");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(String::from_utf8(stopped.stdout)
        .expect("UTF-8 stop output")
        .contains(&format!("stopping pid {}", launcher.id())));
    let commands = fs::read_to_string(log).expect("Herdr commands");
    assert!(commands.contains("tab close w1:t2"), "{commands}");
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
    assert_eq!(
        commands
            .matches("pane run w1:p2 PORTBOARD_TARGET_ID=probe 'sleep' '30'")
            .count(),
        1
    );
    assert!(commands.contains("tab close w1:t2"));
    assert!(!commands.contains("tab focus w1:t2"));
}

#[test]
fn herdr_ensure_rejects_owned_in_herdr_plus_manual_duplicate() {
    let repository = tempfile::tempdir().expect("temporary repository");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .expect("git init")
        .success());
    let marker = format!("portboard-mixed-ownership-test-{}", std::process::id());
    fs::write(
        repository.path().join("portboard.toml"),
        format!(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ),
    )
    .expect("manifest");
    let mut owned = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .env("PORTBOARD_TARGET_ID", "probe")
        .spawn()
        .expect("owned target");
    let mut unowned = Command::new("bash")
        .args(["-c", &format!("exec -a {marker} sleep 30")])
        .current_dir(repository.path())
        .spawn()
        .expect("unowned target");
    let state = tempfile::tempdir().expect("temporary state");
    let fake = state.path().join("fake-herdr");
    fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
case "$1 $2" in
  "pane list") printf '%s\n' '{{"result":{{"panes":[{{"pane_id":"w1:p2","tab_id":"w1:t2"}}]}}}}' ;;
  "pane process-info") printf '%s\n' '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[]}}}}}}' ;;
  *) printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
esac
"#,
            owned.id()
        ),
    )
    .expect("fake Herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("fake permissions");

    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["ensure", "probe", "--herdr", "--cwd"])
        .arg(repository.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .expect("Portboard ensure");

    owned.kill().expect("stop owned target");
    unowned.kill().expect("stop unowned target");
    owned.wait().expect("reap owned target");
    unowned.wait().expect("reap unowned target");
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .expect("UTF-8 stderr")
        .contains("duplicate"));
}

#[test]
fn stop_preserves_tab_with_foreign_split() {
    check_stop_preserves_split(false);
}

#[test]
fn stop_preserves_tab_with_busy_owned_split() {
    check_stop_preserves_split(true);
}

fn check_stop_preserves_split(busy_owned: bool) {
    let root = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .unwrap();
    fs::write(
        root.path().join("portboard.toml"),
        r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["signature-probe"]
"#,
    )
    .unwrap();
    let mut target = OwnedPane::spawn(root.path(), "bash -c 'exec -a signature-probe sleep 30'");
    let state = tempfile::tempdir().unwrap();
    // A split outside the target worktree is never part of the target run.
    let split = OwnedPane::spawn(state.path(), "sleep 30");
    let fake = state.path().join("herdr");
    let log = state.path().join("log");
    let split_shell = if busy_owned {
        split.shell.id()
    } else {
        std::process::id()
    };
    let foreground = if busy_owned {
        format!(r#"{{"pid":{}}}"#, split.id())
    } else {
        String::new()
    };
    fs::write(&fake, format!(r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1 $2" in
 "pane list") echo '{{"result":{{"panes":[{{"pane_id":"main","tab_id":"tab"}},{{"pane_id":"split","tab_id":"tab"}}]}}}}' ;;
 "pane process-info")
   if [ "$4" = main ]; then echo '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[]}}}}}}';
   else echo '{{"result":{{"process_info":{{"shell_pid":{},"foreground_processes":[{}]}}}}}}'; fi ;;
 *) echo '{{"result":{{"type":"ok"}}}}' ;;
esac
"#, log.display(), target.shell.id(), split_shell, foreground)).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_portboard"))
        .args(["stop", "probe", "--cwd"])
        .arg(root.path())
        .env("PORTBOARD_STATE_DIR", state.path())
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", &fake)
        .env_remove("PORTBOARD_WORKSPACE_ID")
        .output()
        .unwrap();
    target.wait().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(portboard::process_identity::ProcessIdentity::read(split.id()).is_some());
    assert!(!fs::read_to_string(log).unwrap().contains("tab close"));
}

/// An isolated real owned host shell, with a child command it reaps itself.
struct OwnedPane {
    shell: std::process::Child,
    pid: u32,
}
impl OwnedPane {
    fn spawn(root: &std::path::Path, command: &str) -> Self {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        let pid_file = root.join("pane-child.pid");
        let mut shell = Command::new("bash")
            .current_dir(root)
            .env("PORTBOARD_TARGET_ID", "probe")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        writeln!(
            shell.stdin.as_mut().unwrap(),
            "{command} & child=$!; echo $child > '{}'; wait $child",
            pid_file.display()
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let pid = loop {
            if let Ok(text) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = text.trim().parse() {
                    break pid;
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        Self { shell, pid }
    }
    fn id(&self) -> u32 {
        self.pid
    }
    fn wait(&mut self) -> std::io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::path::Path::new(&format!("/proc/{}", self.pid)).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "host did not reap child"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }
}
impl Drop for OwnedPane {
    fn drop(&mut self) {
        // Reap the command through its host before killing the host itself.
        if portboard::process_identity::ProcessIdentity::read(self.pid).is_some() {
            unsafe {
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::path::Path::new(&format!("/proc/{}", self.pid)).exists()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}
