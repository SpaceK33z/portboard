use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::launch_target_config::{LaunchTargetId, PortboardConfig};
use crate::launch_target_processes::{find_launch_target_processes, LaunchTargetProcess};
use crate::process_endpoints::{find_process_endpoints, ProcessEndpoint};
use crate::runtime_metadata::{load_launch_target_runtime_metadata, RuntimeEndpoint};

/// Observed process state for one launch target in the current worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchTargetRuntimeState {
    Stopped,
    Running { processes: Vec<LaunchTargetProcess> },
}

/// Display-ready status for one configured current-worktree launch target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CurrentLaunchTargetStatus {
    pub id: LaunchTargetId,
    pub label: String,
    pub argv: Vec<String>,
    pub endpoints: Vec<ProcessEndpoint>,
    pub named_endpoints: Vec<RuntimeEndpoint>,
    /// Whether more than one live process currently matches this target. A
    /// duplicate run is exactly what Portboard exists to prevent, so every
    /// presentation surfaces it instead of hiding it behind `running`.
    pub duplicate: bool,
    #[serde(flatten)]
    pub state: LaunchTargetRuntimeState,
}

/// Observed runtime state for every launch target plus cross-target findings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorktreeLaunchTargetInspection {
    pub statuses: Vec<CurrentLaunchTargetStatus>,
    /// Problems that span launch targets, such as one process matching two
    /// targets. Empty when every match is unambiguous.
    pub findings: Vec<String>,
}

/// Inspects every configured launch target without looking outside the current worktree.
pub fn inspect_worktree_launch_targets(
    worktree_root: &Path,
    config: &PortboardConfig,
) -> Result<WorktreeLaunchTargetInspection> {
    let statuses = config
        .launch_targets
        .iter()
        .map(|target| {
            let processes = find_launch_target_processes(worktree_root, target)?;
            let endpoints = find_process_endpoints(&processes)?;
            let named_endpoints =
                load_launch_target_runtime_metadata(worktree_root, target, &processes)?
                    .map(|metadata| metadata.endpoints)
                    .unwrap_or_default();
            let state = if processes.is_empty() {
                LaunchTargetRuntimeState::Stopped
            } else {
                LaunchTargetRuntimeState::Running { processes }
            };
            let duplicate = matches!(
                &state,
                LaunchTargetRuntimeState::Running { processes } if processes.len() > 1
            );
            Ok(CurrentLaunchTargetStatus {
                id: target.id.clone(),
                label: target.label.clone(),
                argv: target.argv.clone(),
                endpoints,
                named_endpoints,
                duplicate,
                state,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let findings = cross_target_findings(&statuses);
    Ok(WorktreeLaunchTargetInspection { statuses, findings })
}

/// Reports processes whose command line matches more than one launch target.
fn cross_target_findings(statuses: &[CurrentLaunchTargetStatus]) -> Vec<String> {
    let mut owners = statuses
        .iter()
        .filter_map(|status| match &status.state {
            LaunchTargetRuntimeState::Running { processes } => Some(
                processes
                    .iter()
                    .map(|process| (process.pid, status.id.as_str())),
            ),
            LaunchTargetRuntimeState::Stopped => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    owners.sort();
    let mut findings = Vec::new();
    let mut index = 0;
    while index < owners.len() {
        let pid = owners[index].0;
        let mut target_ids = Vec::new();
        while index < owners.len() && owners[index].0 == pid {
            let target_id = owners[index].1;
            if !target_ids.contains(&target_id) {
                target_ids.push(target_id);
            }
            index += 1;
        }
        if target_ids.len() > 1 {
            findings.push(format!(
                "pid {pid} matches launch targets {}; choose signatures that identify exactly one target",
                target_ids
                    .iter()
                    .map(|target_id| format!("`{target_id}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use crate::launch_target_config::parse_launch_target_config;

    use super::{inspect_worktree_launch_targets, LaunchTargetRuntimeState};

    #[test]
    fn reports_stopped_launch_target_when_no_process_matches() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "missing"
label = "Missing server"
argv = ["missing-command"]
process_match = ["portboard-process-that-does-not-exist"]
"#,
        )
        .expect("launch target config");

        let inspection = inspect_worktree_launch_targets(temporary.path(), &config)
            .expect("launch target status");

        assert_eq!(inspection.statuses.len(), 1);
        assert!(!inspection.statuses[0].duplicate);
        assert!(inspection.findings.is_empty());
        assert_eq!(
            inspection.statuses[0].state,
            LaunchTargetRuntimeState::Stopped
        );
    }

    #[test]
    fn flags_duplicate_instances_of_one_launch_target() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-duplicate-test-{}", std::process::id());
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
        let mut children = (0..2)
            .map(|_| {
                Command::new("bash")
                    .args(["-c", &format!("exec -a {marker} sleep 30")])
                    .current_dir(temporary.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("duplicate test process")
            })
            .collect::<Vec<_>>();

        let inspection = inspect_worktree_launch_targets(temporary.path(), &config)
            .expect("launch target status");

        assert_eq!(inspection.statuses.len(), 1);
        assert!(inspection.statuses[0].duplicate);
        match &inspection.statuses[0].state {
            LaunchTargetRuntimeState::Running { processes } => {
                assert_eq!(processes.len(), 2);
            }
            other => panic!("expected running state, got {other:?}"),
        }
        for mut child in children.drain(..) {
            child.kill().expect("stop test process");
            child.wait().expect("reap test process");
        }
    }

    #[test]
    fn reports_a_finding_when_two_targets_claim_one_process() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-overlap-test-{}", std::process::id());
        let config = parse_launch_target_config(&format!(
            r#"
version = 1

[[launch_targets]]
id = "first"
label = "First"
argv = ["sleep", "30"]
process_match = ["{marker}"]

[[launch_targets]]
id = "second"
label = "Second"
argv = ["sleep", "30"]
process_match = ["{marker}"]
"#
        ))
        .expect("launch target config");
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("overlap test process");

        let inspection = inspect_worktree_launch_targets(temporary.path(), &config)
            .expect("launch target status");

        assert_eq!(inspection.findings.len(), 1);
        assert!(inspection.findings[0].contains("pid"));
        assert!(
            inspection.findings[0].contains("`first`")
                && inspection.findings[0].contains("`second`")
        );
        assert!(inspection.statuses.iter().all(|status| !status.duplicate));
        child.kill().expect("stop test process");
        child.wait().expect("reap test process");
    }
}
