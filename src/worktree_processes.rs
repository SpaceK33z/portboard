use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::DiscoverySnapshot;
use crate::process_endpoints::ProcessEndpoint;

/// One running worktree process together with the launch targets that claim it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorktreeProcessInventoryEntry {
    pub pid: u32,
    pub argv: Vec<String>,
    /// Ids of configured launch targets whose signature matches this process.
    /// Empty when no manifest claims the process or no manifest exists.
    pub target_ids: Vec<String>,
    /// Listening TCP endpoints owned by this process or its descendants.
    pub endpoints: Vec<ProcessEndpoint>,
}

/// Inventories every live process in the worktree and attributes configured
/// launch targets to them. Works without a manifest; unclaimed processes are
/// reported with an empty `target_ids` list.
pub fn inspect_worktree_processes(
    worktree_root: &Path,
    targets: &[LaunchTarget],
) -> Result<Vec<WorktreeProcessInventoryEntry>> {
    let snapshot = DiscoverySnapshot::capture(worktree_root)?;
    inspect_worktree_processes_with_snapshot(targets, &snapshot)
}

pub fn inspect_worktree_processes_with_snapshot(
    targets: &[LaunchTarget],
    snapshot: &DiscoverySnapshot,
) -> Result<Vec<WorktreeProcessInventoryEntry>> {
    let processes = snapshot.processes.clone();
    let mut claims = vec![Vec::new(); processes.len()];
    for target in targets {
        for claimed in snapshot.target_processes(target) {
            if let Some(index) = processes
                .iter()
                .position(|process| process.pid == claimed.pid)
            {
                claims[index].push(target.id.as_str().to_string());
            }
        }
    }
    // One grouped scan attributes every listener in the worktree to its owning
    // process so the inventory can show ports with or without a manifest.
    let endpoint_groups = snapshot.endpoints_by_process()?;
    Ok(processes
        .into_iter()
        .zip(claims)
        .map(|(process, target_ids)| WorktreeProcessInventoryEntry {
            endpoints: endpoint_groups
                .get(&process.pid)
                .cloned()
                .unwrap_or_default(),
            pid: process.pid,
            argv: process.argv,
            target_ids,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use crate::launch_target_config::parse_launch_target_config;

    use super::inspect_worktree_processes;

    #[test]
    fn inventories_unclaimed_processes_without_a_manifest() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-ps-unclaimed-{}", std::process::id());
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("inventory test process");

        let entries =
            inspect_worktree_processes(temporary.path(), &[]).expect("worktree process inventory");

        let entry = entries
            .iter()
            .find(|entry| entry.pid == child.id())
            .expect("inventoried process");
        assert!(entry.argv.iter().any(|argument| argument.contains(&marker)));
        assert!(entry.target_ids.is_empty());
        child.kill().expect("stop test process");
        child.wait().expect("reap test process");
    }

    #[test]
    fn attributes_claimed_processes_to_their_launch_targets() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-ps-claimed-{}", std::process::id());
        let config = parse_launch_target_config(&format!(
            r#"
version = 1

[[launch_targets]]
id = "test-server"
label = "Test server"
argv = ["sleep", "30"]
process_match = ["{marker}"]

[[launch_targets]]
id = "other"
label = "Other"
argv = ["sleep", "30"]
process_match = ["portboard-never-running-other-target"]
"#
        ))
        .expect("launch target config");
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("inventory test process");

        let entries = inspect_worktree_processes(temporary.path(), &config.launch_targets)
            .expect("worktree process inventory");

        let entry = entries
            .iter()
            .find(|entry| entry.pid == child.id())
            .expect("inventoried process");
        assert_eq!(entry.target_ids, ["test-server"]);
        child.kill().expect("stop test process");
        child.wait().expect("reap test process");
    }
}
