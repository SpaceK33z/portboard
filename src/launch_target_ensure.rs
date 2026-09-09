use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::herdr_workspace::{
    current_herdr_workspace_id, launch_target_in_herdr, launch_target_runs_in_herdr,
    reap_stale_launch_target_tabs,
};
use crate::launch_target_config::{LaunchTarget, PortboardConfig};
use crate::launch_target_lock::LaunchTargetLock;
use crate::launch_target_processes::{
    find_launch_target_processes, process_has_portboard_target_identity, LaunchTargetProcess,
};
use crate::launch_target_readiness::{
    wait_for_launch_target_process, wait_for_launch_target_ready,
};
use crate::runtime_metadata::RuntimeEndpoint;

const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Observable result of idempotently ensuring one launch target in Herdr.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnsureLaunchTargetOutcome {
    Started,
    AlreadyRunning,
    Starting,
}

/// Result of ensuring a launch target, including its primary URL after readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnsureLaunchTargetResult {
    pub outcome: EnsureLaunchTargetOutcome,
    pub primary_endpoint: Option<RuntimeEndpoint>,
}

/// Ensures a target runs in a Portboard-owned Herdr tab without changing focus.
pub fn ensure_launch_target_in_herdr(
    worktree_root: &Path,
    config: &PortboardConfig,
    target_id: &str,
    wait_until_ready: bool,
) -> Result<EnsureLaunchTargetResult> {
    let workspace_id = current_herdr_workspace_id().context(
        "Portboard ensure --herdr requires a Herdr workspace; run it inside the target workspace",
    )?;
    let target = find_launch_target(config, target_id)?;
    let mut launch_lock = LaunchTargetLock::acquire(worktree_root, target)?;
    if let Some(workspace_id) = current_herdr_workspace_id() {
        // Leftover tabs from a crashed run must go before any new tab is
        // created. Holding the launch lock prevents racing a concurrent start;
        // failures are non-fatal for the ensure flow.
        if let Err(error) = reap_stale_launch_target_tabs(&workspace_id, target) {
            eprintln!("Portboard could not reap stale tabs: {error:#}");
        }
    }
    let processes = find_launch_target_processes(worktree_root, target)?;
    if !processes.is_empty() {
        require_portboard_herdr_run(&workspace_id, target, &processes)?;
        let primary_endpoint = wait_if_requested(worktree_root, target, wait_until_ready)?;
        return Ok(EnsureLaunchTargetResult {
            outcome: EnsureLaunchTargetOutcome::AlreadyRunning,
            primary_endpoint,
        });
    }

    if launch_lock.has_active_reservation()? {
        drop(launch_lock);
        let processes =
            wait_for_launch_target_process(worktree_root, target, DEFAULT_READINESS_TIMEOUT)?;
        require_portboard_herdr_run(&workspace_id, target, &processes)?;
        let primary_endpoint = wait_if_requested(worktree_root, target, wait_until_ready)?;
        return Ok(EnsureLaunchTargetResult {
            outcome: EnsureLaunchTargetOutcome::Starting,
            primary_endpoint,
        });
    }

    let launcher_pid = launch_target_in_herdr(worktree_root, &workspace_id, target, false)?;
    launch_lock.reserve_process(launcher_pid)?;
    drop(launch_lock);
    let primary_endpoint = wait_if_requested(worktree_root, target, wait_until_ready)?;
    Ok(EnsureLaunchTargetResult {
        outcome: EnsureLaunchTargetOutcome::Started,
        primary_endpoint,
    })
}

fn require_portboard_herdr_run(
    workspace_id: &str,
    target: &LaunchTarget,
    processes: &[LaunchTargetProcess],
) -> Result<()> {
    if processes.len() > 1 {
        bail!("Portboard launch target `{}` has {} duplicate instances, including runs outside its Portboard-owned Herdr tab", target.id.as_str(), processes.len());
    }
    let owned_processes = processes
        .iter()
        .filter(|process| process_has_portboard_target_identity(process, target.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let runs_in_herdr =
        !owned_processes.is_empty() && launch_target_runs_in_herdr(workspace_id, &owned_processes)?;
    if !runs_in_herdr {
        bail!(
            "Portboard launch target `{}` is running outside its Portboard-owned Herdr tab; stop it, then retry `portboard ensure {} --herdr --wait`",
            target.id.as_str(),
            target.id.as_str()
        );
    }
    Ok(())
}

fn wait_if_requested(
    worktree_root: &Path,
    target: &LaunchTarget,
    wait_until_ready: bool,
) -> Result<Option<RuntimeEndpoint>> {
    if !wait_until_ready {
        return Ok(None);
    }
    wait_for_launch_target_ready(worktree_root, target, DEFAULT_READINESS_TIMEOUT)
}

fn find_launch_target<'a>(
    config: &'a PortboardConfig,
    target_id: &str,
) -> Result<&'a LaunchTarget> {
    config
        .launch_targets
        .iter()
        .find(|target| target.id.as_str() == target_id)
        .with_context(|| format!("Portboard launch target `{target_id}` is not configured"))
}
