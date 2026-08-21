use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::launch_target_config::LaunchTarget;

/// Live operating-system process matching one launch target in its worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaunchTargetProcess {
    pub pid: u32,
    pub argv: Vec<String>,
}

/// Live operating-system process whose cwd is inside one worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorktreeProcess {
    pub pid: u32,
    pub argv: Vec<String>,
}

/// Finds live Linux processes whose cwd and argv match a worktree launch target.
pub fn find_launch_target_processes(
    worktree_root: &Path,
    target: &LaunchTarget,
) -> Result<Vec<LaunchTargetProcess>> {
    let process_match = if target.process_match.is_empty() {
        &target.argv
    } else {
        &target.process_match
    };
    let mut matches = Vec::new();
    for process in find_worktree_processes(worktree_root)? {
        let searchable_command = process.argv.join(" ");
        if process_match
            .iter()
            .all(|expected| searchable_command.contains(expected))
        {
            matches.push(LaunchTargetProcess {
                pid: process.pid,
                argv: process.argv,
            });
        }
    }
    Ok(matches)
}

/// Scans `/proc` for every live process whose cwd is inside the canonical worktree.
pub fn find_worktree_processes(worktree_root: &Path) -> Result<Vec<WorktreeProcess>> {
    let canonical_root = worktree_root.canonicalize().with_context(|| {
        format!(
            "Portboard process scan could not resolve worktree {}",
            worktree_root.display()
        )
    })?;
    let mut matches = Vec::new();

    for entry in fs::read_dir("/proc").context("Portboard process scan could not read /proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // The scanner's own command line can contain a target's argv (for
        // example when Portboard itself is configured as a target). It is an
        // observer, never a live run.
        if pid == std::process::id() {
            continue;
        }
        let process_path = entry.path();
        let Some(start_time) = read_process_start_time(&process_path) else {
            continue;
        };
        let Ok(cwd) = fs::read_link(process_path.join("cwd")) else {
            continue;
        };
        if !cwd.starts_with(&canonical_root) {
            continue;
        }
        let Ok(command_line) = fs::read(process_path.join("cmdline")) else {
            continue;
        };
        let argv = command_line
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect::<Vec<_>>();
        if argv.is_empty() {
            continue;
        }
        // Re-reading the start time prevents a PID reused between the two
        // reads from being reported as a live run.
        if read_process_start_time(&process_path) != Some(start_time) {
            continue;
        }
        matches.push(WorktreeProcess { pid, argv });
    }

    matches.sort_by_key(|process| process.pid);
    Ok(matches)
}

/// Checks whether a process inherited the identity assigned by Portboard's run host.
pub fn process_has_portboard_target_identity(
    process: &LaunchTargetProcess,
    target_id: &str,
) -> bool {
    let Ok(environment) = fs::read(format!("/proc/{}/environ", process.pid)) else {
        return false;
    };
    let expected = format!("PORTBOARD_TARGET_ID={target_id}");
    environment
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected.as_bytes())
}

fn read_process_start_time(process_path: &Path) -> Option<u64> {
    let stat = fs::read_to_string(process_path.join("stat")).ok()?;
    // The comm field is parenthesized and may itself contain spaces or `)`.
    // Fields after its final `)` begin at proc(5) field 3; starttime is field
    // 22, or index 19 in this suffix.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use crate::launch_target_config::parse_launch_target_config;

    use super::{find_launch_target_processes, process_has_portboard_target_identity};

    #[test]
    fn does_not_report_the_scanner_as_a_target_process() {
        let worktree = std::env::current_dir().expect("current worktree");
        let executable = std::env::current_exe().expect("test executable");
        let executable_name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test executable name");
        let config = parse_launch_target_config(&format!(
            r#"
version = 1

[[launch_targets]]
id = "portboard-test"
label = "Portboard test"
argv = ["{executable_name}"]
process_match = ["{executable_name}"]
"#
        ))
        .expect("launch target config");

        let processes = find_launch_target_processes(&worktree, &config.launch_targets[0])
            .expect("process scan");

        assert!(!processes
            .iter()
            .any(|process| process.pid == std::process::id()));
    }

    #[test]
    fn finds_matching_process_in_current_worktree() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-process-test-{}", std::process::id());
        let config = parse_launch_target_config(&format!(
            r#"
version = 1

[[launch_targets]]
id = "test-server"
label = "Test server"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ))
        .expect("launch target config");
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .env("PORTBOARD_TARGET_ID", "test-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test process");

        let processes = find_launch_target_processes(temporary.path(), &config.launch_targets[0])
            .expect("process scan");

        let process = processes
            .iter()
            .find(|process| process.pid == child.id())
            .expect("matching process");
        assert!(process_has_portboard_target_identity(
            process,
            "test-server"
        ));
        child.kill().expect("stop test process");
        child.wait().expect("reap test process");
    }
}
