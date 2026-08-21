use std::path::Path;
use std::process::{Child, Command};

use anyhow::{bail, Context, Result};

use crate::herdr_workspace::{
    current_herdr_workspace_id, focus_launch_target_in_herdr, launch_target_in_herdr,
};
use crate::launch_target_config::{LaunchTarget, PortboardConfig};
use crate::launch_target_lock::LaunchTargetLock;
use crate::launch_target_processes::find_launch_target_processes;
use crate::manifest_approval::ensure_launch_target_approved;

/// Opens an existing launch target run or starts it in the best available run host.
pub fn open_or_start_launch_target(
    worktree_root: &Path,
    config: &PortboardConfig,
    requested_target_id: Option<&str>,
) -> Result<()> {
    let target = select_launch_target(config, requested_target_id)?;
    let mut launch_lock = LaunchTargetLock::acquire(worktree_root, target)?;
    let processes = find_launch_target_processes(worktree_root, target)?;
    if !processes.is_empty() {
        launch_lock.clear()?;
        if processes.len() > 1 {
            println!(
                "{} has {} duplicate instances running; refusing to focus or start another",
                target.label,
                processes.len()
            );
            for process in &processes {
                println!(
                    "  pid {}: {}",
                    process.pid,
                    serde_json::to_string(&process.argv)?
                );
            }
            return Ok(());
        }
        let focused_in_herdr = match current_herdr_workspace_id() {
            Some(workspace_id) => focus_launch_target_in_herdr(&workspace_id, &processes)?,
            None => false,
        };
        if focused_in_herdr {
            println!("Focused {}", target.label);
        } else {
            println!(
                "{} is already running outside a Portboard-owned Herdr pane",
                target.label
            );
            println!("  cwd: {}", worktree_root.display());
            for process in &processes {
                println!(
                    "  pid {}: {}",
                    process.pid,
                    serde_json::to_string(&process.argv)?
                );
            }
        }
        return Ok(());
    }

    if launch_lock.has_active_reservation()? {
        println!("{} is starting", target.label);
        return Ok(());
    }

    ensure_launch_target_approved(worktree_root, config, target)?;

    if let Some(workspace_id) = current_herdr_workspace_id() {
        let pid = launch_target_in_herdr(worktree_root, &workspace_id, target, true)?;
        launch_lock.reserve_process(pid)?;
        println!("Started {} in Herdr", target.label);
        return Ok(());
    }

    let child = start_launch_target_attached(worktree_root, target)?;
    launch_lock.reserve_process(child.id())?;
    // The persistent reservation now identifies the launcher across Portboard
    // processes; the advisory lock need not cover the complete run lifetime.
    drop(launch_lock);
    wait_for_attached_launch(child, target)
}

fn select_launch_target<'a>(
    config: &'a PortboardConfig,
    requested_target_id: Option<&str>,
) -> Result<&'a LaunchTarget> {
    if let Some(target_id) = requested_target_id {
        return config
            .launch_targets
            .iter()
            .find(|target| target.id.as_str() == target_id)
            .with_context(|| format!("Portboard launch target `{target_id}` is not configured"));
    }
    match config.launch_targets.as_slice() {
        [target] => Ok(target),
        [] => bail!("Portboard manifest has no launch targets"),
        _ => bail!("Portboard open needs a launch target id when multiple targets are configured"),
    }
}

fn start_launch_target_attached(worktree_root: &Path, target: &LaunchTarget) -> Result<Child> {
    Command::new(&target.argv[0])
        .args(&target.argv[1..])
        .current_dir(worktree_root)
        .spawn()
        .with_context(|| {
            format!(
                "Portboard attached launch could not start `{}`",
                target.argv.join(" ")
            )
        })
}

fn wait_for_attached_launch(mut child: Child, target: &LaunchTarget) -> Result<()> {
    let status = child.wait().with_context(|| {
        format!(
            "Portboard attached launch could not wait for `{}`",
            target.argv.join(" ")
        )
    })?;
    if !status.success() {
        bail!(
            "Portboard attached launch `{}` exited with {}",
            target.argv.join(" "),
            status
        );
    }
    Ok(())
}
