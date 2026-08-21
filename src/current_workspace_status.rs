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
    #[serde(flatten)]
    pub state: LaunchTargetRuntimeState,
}

/// Inspects every configured launch target without looking outside the current worktree.
pub fn inspect_worktree_launch_targets(
    worktree_root: &Path,
    config: &PortboardConfig,
) -> Result<Vec<CurrentLaunchTargetStatus>> {
    config
        .launch_targets
        .iter()
        .map(|target| {
            let processes = find_launch_target_processes(worktree_root, target)?;
            let endpoints = find_process_endpoints(&processes)?;
            let named_endpoints = load_launch_target_runtime_metadata(
                worktree_root,
                target,
                &processes,
            )?
            .map(|metadata| metadata.endpoints)
            .unwrap_or_default();
            let state = if processes.is_empty() {
                LaunchTargetRuntimeState::Stopped
            } else {
                LaunchTargetRuntimeState::Running { processes }
            };
            Ok(CurrentLaunchTargetStatus {
                id: target.id.clone(),
                label: target.label.clone(),
                argv: target.argv.clone(),
                endpoints,
                named_endpoints,
                state,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
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

        let statuses = inspect_worktree_launch_targets(temporary.path(), &config)
            .expect("launch target status");

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, LaunchTargetRuntimeState::Stopped);
    }
}
