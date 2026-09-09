use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::launch_target_config::LaunchTarget;

/// Live operating-system process matching one launch target in its worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaunchTargetProcess {
    /// Exact signature-matching descendants captured with this run's root.
    /// Kept separately so metadata can name the coordinator without changing
    /// duplicate counting or the root used for process-tree shutdown.
    #[serde(skip)]
    pub metadata_members: Vec<crate::process_identity::ProcessIdentity>,
    pub pid: u32,
    #[serde(skip)]
    pub start_time: u64,
    pub argv: Vec<String>,
}

/// Live operating-system process whose cwd is inside one worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorktreeProcess {
    pub pid: u32,
    #[serde(skip)]
    pub start_time: u64,
    pub argv: Vec<String>,
}

/// Finds live Linux processes belonging to a worktree launch target. A process
/// matches when its cwd and argv match the configured pattern, or when it
/// inherited the target's `PORTBOARD_TARGET_ID` identity from a Portboard-run
/// host pane. The identity pass keeps detection working across re-exec and
/// wrapper scripts that would otherwise hide the real server from argv
/// matching; interactive host shells carrying the marker are ignored.
pub fn find_launch_target_processes(
    worktree_root: &Path,
    target: &LaunchTarget,
) -> Result<Vec<LaunchTargetProcess>> {
    Ok(DiscoverySnapshot::capture(worktree_root)?.target_processes(target))
}

/// One refresh's proc/git inventory. Reuse across target status and inventory;
/// signals always revalidate the captured identities independently.
pub struct DiscoverySnapshot {
    pub processes: Vec<WorktreeProcess>,
    parents: std::collections::HashMap<u32, u32>,
    target_ids: std::collections::HashMap<u32, String>,
    endpoints: std::sync::OnceLock<
        std::collections::HashMap<u32, Vec<crate::process_endpoints::ProcessEndpoint>>,
    >,
}

impl DiscoverySnapshot {
    pub fn capture(worktree_root: &Path) -> Result<Self> {
        let processes = find_worktree_processes(worktree_root)?;
        let mut parents = std::collections::HashMap::new();
        let mut target_ids = std::collections::HashMap::new();
        for process in &processes {
            let mut current = process.pid;
            while current > 1 && !parents.contains_key(&current) {
                let Some(parent) =
                    read_process_parent_pid(&PathBuf::from(format!("/proc/{current}")))
                else {
                    break;
                };
                parents.insert(current, parent);
                current = parent;
            }
            if let Ok(environment) = fs::read(format!("/proc/{}/environ", process.pid)) {
                if let Some(id) = environment
                    .split(|byte| *byte == 0)
                    .find_map(|entry| entry.strip_prefix(b"PORTBOARD_TARGET_ID="))
                {
                    target_ids.insert(process.pid, String::from_utf8_lossy(id).into_owned());
                }
            }
        }
        Ok(Self {
            processes,
            parents,
            target_ids,
            endpoints: std::sync::OnceLock::new(),
        })
    }

    /// Lazily performs one grouped socket/parent scan for the whole refresh.
    pub fn endpoints_by_process(
        &self,
    ) -> Result<&std::collections::HashMap<u32, Vec<crate::process_endpoints::ProcessEndpoint>>>
    {
        if self.endpoints.get().is_none() {
            let processes = self
                .processes
                .iter()
                .map(as_launch_target_process)
                .collect::<Vec<_>>();
            let grouped = crate::process_endpoints::find_process_endpoints_by_process(&processes)?;
            let _ = self.endpoints.set(grouped);
        }
        Ok(self.endpoints.get().expect("initialized endpoint snapshot"))
    }

    pub fn target_endpoints(
        &self,
        processes: &[LaunchTargetProcess],
    ) -> Result<Vec<crate::process_endpoints::ProcessEndpoint>> {
        let roots = processes
            .iter()
            .map(|p| p.pid)
            .collect::<std::collections::HashSet<_>>();
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let mut endpoints = Vec::new();
        for (pid, owned) in self.endpoints_by_process()? {
            let mut current = *pid;
            let mut visited = std::collections::HashSet::new();
            loop {
                if roots.contains(&current) {
                    endpoints.extend(owned.iter().cloned());
                    break;
                }
                if !visited.insert(current) {
                    break;
                }
                let Some(parent) = self.parents.get(&current) else {
                    break;
                };
                current = *parent;
            }
        }
        endpoints.sort();
        endpoints.dedup();
        Ok(endpoints)
    }

    pub fn target_processes(&self, target: &LaunchTarget) -> Vec<LaunchTargetProcess> {
        let signature = if target.process_match.is_empty() {
            &target.argv
        } else {
            &target.process_match
        };
        let candidates = self
            .processes
            .iter()
            .filter(|process| {
                signature.iter().all(|expected| {
                    process
                        .argv
                        .iter()
                        .any(|arg| process_argument_matches(arg, expected))
                }) || (!looks_like_interactive_shell(&process.argv)
                    && self
                        .target_ids
                        .get(&process.pid)
                        .is_some_and(|id| id == target.id.as_str()))
            })
            .collect::<Vec<_>>();
        let pids = candidates
            .iter()
            .map(|process| process.pid)
            .collect::<std::collections::HashSet<_>>();
        candidates
            .iter()
            .copied()
            .filter(|process| {
                let mut current = process.pid;
                let mut visited = std::collections::HashSet::new();
                while let Some(parent) = self.parents.get(&current) {
                    if !visited.insert(*parent) {
                        break;
                    }
                    if pids.contains(parent) {
                        return false;
                    }
                    current = *parent;
                }
                crate::process_identity::ProcessIdentity {
                    pid: process.pid,
                    start_time: process.start_time,
                }
                .is_live()
            })
            .map(|root| {
                let mut process = as_launch_target_process(root);
                process.metadata_members = candidates
                    .iter()
                    .filter(|member| {
                        member.pid != root.pid
                            && signature.iter().all(|expected| {
                                member
                                    .argv
                                    .iter()
                                    .any(|arg| process_argument_matches(arg, expected))
                            })
                            && {
                                let mut current = member.pid;
                                let mut visited = std::collections::HashSet::new();
                                while visited.insert(current) {
                                    let Some(parent) = self.parents.get(&current) else {
                                        return false;
                                    };
                                    if *parent == root.pid {
                                        return true;
                                    }
                                    current = *parent;
                                }
                                false
                            }
                    })
                    .map(|member| crate::process_identity::ProcessIdentity {
                        pid: member.pid,
                        start_time: member.start_time,
                    })
                    .filter(|identity| identity.is_live())
                    .collect();
                process
            })
            .collect()
    }
}

impl LaunchTargetProcess {
    pub fn identity(&self) -> crate::process_identity::ProcessIdentity {
        crate::process_identity::ProcessIdentity {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// Shell basenames whose bare invocation identifies an interactive shell.
const INTERACTIVE_SHELL_NAMES: &[&str] = &[
    "bash", "sh", "zsh", "fish", "dash", "ksh", "csh", "tcsh", "nu", "elvish", "xonsh",
];

/// Returns whether a command line looks like a bare interactive shell.
///
/// Portboard's Herdr host panes carry `PORTBOARD_TARGET_ID` in their shell
/// environment, so every pane shell inherits the target identity. Those shells
/// outlive the target command and must never count as a live run; wrappers
/// such as `bash -c ...` keep their arguments and stay detectable.
fn looks_like_interactive_shell(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let name = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    // Login shells render with a leading dash, for example `-bash`.
    let name = name.strip_prefix('-').unwrap_or(name);
    INTERACTIVE_SHELL_NAMES.contains(&name) && argv.len() <= 1
}

/// Scans `/proc` for every live process whose cwd is inside the canonical
/// worktree. Processes running in *other* Git worktrees of the same repository
/// are excluded even when their paths sit below the root (for example
/// `<root>/.worktrees/<name>`), so one worktree's stack never claims another's.
pub fn find_worktree_processes(worktree_root: &Path) -> Result<Vec<WorktreeProcess>> {
    let canonical_root = worktree_root.canonicalize().with_context(|| {
        format!(
            "Portboard process scan could not resolve worktree {}",
            worktree_root.display()
        )
    })?;
    let other_worktrees = sibling_git_worktrees(&canonical_root);
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
        // Linux appends " (deleted)" to /proc/<pid>/cwd after a worktree is
        // removed. Such stale processes are not members of the parent checkout
        // merely because their former path was nested below it.
        if !cwd.exists() {
            continue;
        }
        if !cwd.starts_with(&canonical_root)
            || other_worktrees
                .iter()
                .any(|worktree| cwd.starts_with(worktree))
        {
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
        matches.push(WorktreeProcess {
            pid,
            start_time,
            argv,
        });
    }

    matches.sort_by_key(|process| process.pid);
    Ok(matches)
}

/// Checks whether a process inherited the identity assigned by Portboard's run host.
pub fn process_has_portboard_target_identity(
    process: &LaunchTargetProcess,
    target_id: &str,
) -> bool {
    if !process.identity().is_live() {
        return false;
    }
    let Ok(environment) = fs::read(format!("/proc/{}/environ", process.pid)) else {
        return false;
    };
    let expected = format!("PORTBOARD_TARGET_ID={target_id}");
    environment
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected.as_bytes())
}

/// Lists the repository's other Git worktrees whose paths sit below the root.
/// Returns an empty list outside a Git repository or when `git` is unavailable,
/// which keeps the scan working for plain directories.
fn sibling_git_worktrees(canonical_root: &Path) -> Vec<PathBuf> {
    let Ok(output) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(canonical_root)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|path| path != canonical_root && path.starts_with(canonical_root))
        .collect()
}

fn as_launch_target_process(process: &WorktreeProcess) -> LaunchTargetProcess {
    LaunchTargetProcess {
        metadata_members: Vec::new(),
        pid: process.pid,
        start_time: process.start_time,
        argv: process.argv.clone(),
    }
}

fn process_argument_matches(argument: &str, expected: &str) -> bool {
    argument == expected || Path::new(argument).ends_with(Path::new(expected))
}

fn read_process_parent_pid(process_path: &Path) -> Option<u32> {
    let stat = fs::read_to_string(process_path.join("stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
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
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::process_has_portboard_target_identity;
    use super::{find_launch_target_processes, looks_like_interactive_shell};

    /// Creates the directory so the git fixtures below have a stable parent.
    fn fs_err_create(path: &Path) {
        fs::create_dir_all(path).expect("repository directory");
    }

    use crate::launch_target_config::parse_launch_target_config;

    #[test]
    fn snapshot_reuses_the_grouped_listener_scan() {
        let identity = crate::process_identity::ProcessIdentity::read(std::process::id()).unwrap();
        let snapshot = super::DiscoverySnapshot {
            processes: vec![super::WorktreeProcess {
                pid: identity.pid,
                start_time: identity.start_time,
                argv: vec!["listener".into()],
            }],
            parents: Default::default(),
            target_ids: Default::default(),
            endpoints: Default::default(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let initial = snapshot.endpoints_by_process().unwrap().clone();
        drop(listener);
        assert!(initial[&identity.pid]
            .iter()
            .any(|endpoint| endpoint.port == port));
        assert_eq!(
            snapshot.endpoints_by_process().unwrap(),
            &initial,
            "one refresh must not rescan listeners per target"
        );
        let roots = snapshot
            .processes
            .iter()
            .map(super::as_launch_target_process)
            .collect::<Vec<_>>();
        assert_eq!(
            snapshot.target_endpoints(&roots).unwrap(),
            initial[&identity.pid]
        );
    }

    #[test]
    fn refresh_snapshot_is_shared_by_status_and_inventory_without_rescanning() {
        let root = tempfile::tempdir().unwrap();
        let config = parse_launch_target_config(
            r#"version = 1
[[launch_targets]]
id = "snapshot"
label = "Snapshot"
argv = ["snapshot-marker"]
"#,
        )
        .unwrap();
        let snapshot = super::DiscoverySnapshot::capture(root.path()).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .env("PORTBOARD_TARGET_ID", "snapshot")
            .current_dir(root.path())
            .spawn()
            .unwrap();
        let inspection =
            crate::current_workspace_status::inspect_worktree_launch_targets_with_snapshot(
                root.path(),
                &config,
                &snapshot,
            )
            .unwrap();
        let inventory = crate::worktree_processes::inspect_worktree_processes_with_snapshot(
            &config.launch_targets,
            &snapshot,
        )
        .unwrap();
        let next = super::DiscoverySnapshot::capture(root.path()).unwrap();
        let found = next.target_processes(&config.launch_targets[0]);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            inventory.is_empty(),
            "inventory must reuse its refresh's capture"
        );
        assert!(matches!(
            inspection.statuses[0].state,
            crate::current_workspace_status::LaunchTargetRuntimeState::Stopped
        ));
        assert_eq!(found.len(), 1);
        assert!(
            next.target_processes(&config.launch_targets[0]).is_empty(),
            "cached identities must still be revalidated"
        );
    }

    #[test]
    fn independent_identity_tree_is_not_hidden_by_signature() {
        let temporary = tempfile::tempdir().unwrap();
        let config = parse_launch_target_config(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["missing"]
process_match = ["signature-probe"]
"#,
        )
        .unwrap();
        let mut manual = Command::new("bash")
            .args(["-c", "exec -a signature-probe sleep 30"])
            .current_dir(temporary.path())
            .spawn()
            .unwrap();
        let mut owned = Command::new("sleep")
            .arg("30")
            .env("PORTBOARD_TARGET_ID", "probe")
            .current_dir(temporary.path())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let found =
            find_launch_target_processes(temporary.path(), &config.launch_targets[0]).unwrap();
        manual.kill().unwrap();
        manual.wait().unwrap();
        owned.kill().unwrap();
        owned.wait().unwrap();
        assert_eq!(found.len(), 2, "independent runs must remain visible");
    }

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
    fn uses_the_signature_as_the_instance_boundary_for_a_process_tree() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let script = temporary.path().join("instance-marker.sh");
        fs::write(
            &script,
            "#!/bin/sh\nsleep 30 &\nchild=$!\ntrap 'kill $child; wait $child; exit' TERM\nwait\n",
        )
        .expect("target script");
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "tree-server"
label = "Tree server"
argv = ["sh", "instance-marker.sh"]
process_match = ["instance-marker.sh"]
"#,
        )
        .expect("launch target config");
        let mut child = Command::new("sh")
            .arg("instance-marker.sh")
            .env("PORTBOARD_TARGET_ID", "tree-server")
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("process tree");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let processes = find_launch_target_processes(temporary.path(), &config.launch_targets[0])
            .expect("process scan");

        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        child.wait().expect("reap process tree");
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].pid, child.id());
    }

    #[test]
    fn ignores_processes_in_a_deleted_nested_directory() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let deleted = temporary.path().join("deleted-worktree");
        fs::create_dir(&deleted).expect("nested directory");
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "deleted-server"
label = "Deleted server"
argv = ["sleep", "30"]
process_match = ["sleep", "30"]
"#,
        )
        .expect("launch target config");
        let mut child = Command::new("sleep")
            .arg("30")
            .env("PORTBOARD_TARGET_ID", "deleted-server")
            .current_dir(&deleted)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("deleted-directory process");
        fs::remove_dir(&deleted).expect("remove nested directory");

        let processes = find_launch_target_processes(temporary.path(), &config.launch_targets[0])
            .expect("process scan");

        child.kill().expect("stop deleted-directory process");
        child.wait().expect("reap deleted-directory process");
        assert!(processes.is_empty());
    }

    #[test]
    fn detects_a_reexeced_process_through_inherited_identity() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "identity-server"
label = "Identity server"
argv = ["never-matches-anything"]
process_match = ["never-matches-anything"]
"#,
        )
        .expect("launch target config");
        let mut child = Command::new("sleep")
            .args(["30"])
            .env("PORTBOARD_TARGET_ID", "identity-server")
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("identity test process");

        let processes = find_launch_target_processes(temporary.path(), &config.launch_targets[0])
            .expect("process scan");
        child.kill().expect("kill identity test process");
        child.wait().expect("reap identity test process");

        assert!(processes.iter().any(|process| process.pid == child.id()));
    }

    #[test]
    fn ignores_interactive_host_shells_carrying_the_identity() {
        // A bare shell inherits PORTBOARD_TARGET_ID from the host pane but is
        // never a live run, while `sh -c ...` wrappers stay detectable.
        assert!(looks_like_interactive_shell(&["bash".to_string()]));
        assert!(looks_like_interactive_shell(&["-bash".to_string()]));
        assert!(looks_like_interactive_shell(&["/usr/bin/zsh".to_string()]));
        assert!(!looks_like_interactive_shell(&[
            "bash".to_string(),
            "-c".to_string(),
            "exec server".to_string()
        ]));
        assert!(!looks_like_interactive_shell(&[
            "node".to_string(),
            "server.js".to_string()
        ]));
        assert!(!looks_like_interactive_shell(&[]));
    }

    #[test]
    fn does_not_match_a_shell_command_that_only_mentions_the_signature() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-mentioned-signature-{}", std::process::id());
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
            .args([
                "-c",
                &format!("while true; do sleep 1; done # mentions {marker}"),
            ])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("shell process");

        let processes = find_launch_target_processes(temporary.path(), &config.launch_targets[0])
            .expect("process scan");

        child.kill().expect("stop shell process");
        child.wait().expect("reap shell process");
        assert!(!processes.iter().any(|process| process.pid == child.id()));
    }

    #[test]
    fn ignores_processes_running_in_a_nested_git_worktree() {
        let repository = tempfile::tempdir().expect("temporary repository");
        fs_err_create(repository.path());
        let run_git = |arguments: &[&str]| {
            Command::new("git")
                .args(["-c", "user.email=t@l", "-c", "user.name=t"])
                .arg("-C")
                .arg(repository.path())
                .args(arguments)
                .output()
                .expect("git")
        };
        assert!(run_git(&["init", "-b", "main"]).status.success());
        fs::write(repository.path().join("file"), "content").expect("commit input");
        assert!(run_git(&["add", "file"]).status.success());
        assert!(run_git(&["commit", "-m", "init"]).status.success());
        let nested = repository.path().join(".worktrees/embed-tests");
        assert!(run_git(&[
            "worktree",
            "add",
            nested.to_str().expect("nested path"),
            "-b",
            "embed-tests"
        ])
        .status
        .success());

        let marker = format!("portboard-nested-worktree-{}", std::process::id());
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
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&nested)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nested worktree process");

        // The scanner runs against the *main* checkout while the process sits
        // in the nested worktree below it.
        let processes = find_launch_target_processes(repository.path(), &config.launch_targets[0])
            .expect("process scan");

        child.kill().expect("stop nested process");
        child.wait().expect("reap nested process");
        assert!(!processes.iter().any(|process| process.pid == child.id()));
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
