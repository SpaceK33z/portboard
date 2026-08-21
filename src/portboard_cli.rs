use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::current_workspace_panel::open_current_workspace_panel;
use crate::current_workspace_status::{inspect_worktree_launch_targets, LaunchTargetRuntimeState};
use crate::current_worktree::resolve_current_worktree;
use crate::dashboard_server::serve_worktree_dashboard;
use crate::herdr_workspace::open_current_workspace_plugin_panel;
use crate::launch_target_config::load_worktree_launch_targets;
use crate::launch_target_ensure::{
    ensure_launch_target_in_herdr, EnsureLaunchTargetOutcome,
};
use crate::launch_target_open::open_or_start_launch_target;
use crate::launch_target_processes::find_launch_target_processes;
use crate::runtime_metadata::load_launch_target_runtime_metadata;

/// Runs the Portboard command line interface for the current workspace.
pub fn run_portboard_cli(arguments: &[String]) -> Result<()> {
    let command = arguments.first().map(String::as_str).unwrap_or("status");
    match command {
        "status" => run_status_command(&arguments[1..]),
        "open" => run_open_command(&arguments[1..]),
        "ensure" => run_ensure_command(&arguments[1..]),
        "url" => run_url_command(&arguments[1..]),
        "serve" => run_serve_command(&arguments[1..]),
        "panel" => {
            let root = resolve_cli_worktree(None)?;
            open_current_workspace_panel(&root)
        }
        "plugin-open" => open_current_workspace_plugin_panel(),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => bail!("Portboard command `{other}` is unknown; run `portboard help`"),
    }
}

fn run_status_command(arguments: &[String]) -> Result<()> {
    let mut json = false;
    let mut cwd = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard status --cwd needs a path")?,
                ));
            }
            other => bail!("Portboard status option `{other}` is unknown"),
        }
        index += 1;
    }

    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let statuses = inspect_worktree_launch_targets(&root, &config)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
    } else {
        println!("Portboard · {}", root.display());
        for status in statuses {
            let endpoint_summary = status
                .endpoints
                .iter()
                .map(|endpoint| format!("{}:{}", endpoint.address, endpoint.port))
                .collect::<Vec<_>>()
                .join(", ");
            let state = match status.state {
                LaunchTargetRuntimeState::Stopped => "stopped".to_string(),
                LaunchTargetRuntimeState::Running { processes } => format!(
                    "running (pid {})",
                    processes
                        .iter()
                        .map(|process| process.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            if endpoint_summary.is_empty() {
                println!("  {:<24} {state}", status.label);
            } else {
                println!("  {:<24} {state} · {endpoint_summary}", status.label);
            }
        }
    }
    Ok(())
}

fn run_ensure_command(arguments: &[String]) -> Result<()> {
    let mut target_id = None;
    let mut cwd = None;
    let mut herdr_required = false;
    let mut wait = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--herdr" => herdr_required = true,
            "--wait" => wait = true,
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard ensure --cwd needs a path")?,
                ));
            }
            value if target_id.is_none() => target_id = Some(value),
            other => bail!("Portboard ensure argument `{other}` is unexpected"),
        }
        index += 1;
    }
    let target_id = target_id.context("Portboard ensure needs a launch target id")?;
    if !herdr_required {
        bail!("Portboard ensure currently requires --herdr so it cannot fall back to a background process");
    }

    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let result = ensure_launch_target_in_herdr(&root, &config, target_id, wait)?;
    let outcome = match result.outcome {
        EnsureLaunchTargetOutcome::Started => "started",
        EnsureLaunchTargetOutcome::AlreadyRunning => "already running",
        EnsureLaunchTargetOutcome::Starting => "starting",
    };
    println!("{target_id}: {outcome}");
    if let Some(endpoint) = result.primary_endpoint {
        println!("{}", endpoint.url);
    }
    Ok(())
}

fn run_url_command(arguments: &[String]) -> Result<()> {
    let mut positional = Vec::new();
    let mut cwd = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard url --cwd needs a path")?,
                ));
            }
            value => positional.push(value),
        }
        index += 1;
    }
    let target_id = positional.first().context("Portboard url needs a launch target id")?;
    if positional.len() > 2 {
        bail!("Portboard url received too many endpoint identifiers");
    }
    let endpoint_id = positional.get(1).copied();
    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let target = config
        .launch_targets
        .iter()
        .find(|target| target.id.as_str() == *target_id)
        .with_context(|| format!("Portboard launch target `{target_id}` is not configured"))?;
    let processes = find_launch_target_processes(&root, target)?;
    if processes.is_empty() {
        bail!("Portboard launch target `{target_id}` is not running");
    }
    let metadata = load_launch_target_runtime_metadata(&root, target, &processes)?
        .with_context(|| format!("Portboard launch target `{target_id}` has no current runtime metadata"))?;
    let endpoint = match endpoint_id {
        Some(endpoint_id) => metadata
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == endpoint_id)
            .with_context(|| {
                format!(
                    "Portboard launch target `{target_id}` has no endpoint `{endpoint_id}`"
                )
            })?,
        None => metadata.primary_endpoint().with_context(|| {
            format!("Portboard launch target `{target_id}` has no primary endpoint")
        })?,
    };
    println!("{}", endpoint.url);
    Ok(())
}

fn run_serve_command(arguments: &[String]) -> Result<()> {
    let mut cwd = None;
    let mut bind = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard serve --cwd needs a path")?,
                ));
            }
            "--bind" => {
                index += 1;
                bind = Some(
                    arguments
                        .get(index)
                        .context("Portboard serve --bind needs an address")?
                        .as_str(),
                );
            }
            other => bail!("Portboard serve option `{other}` is unknown"),
        }
        index += 1;
    }

    let root = resolve_cli_worktree(cwd.as_deref())?;
    serve_worktree_dashboard(&root, bind)
}

fn run_open_command(arguments: &[String]) -> Result<()> {
    let mut target_id = None;
    let mut cwd = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard open --cwd needs a path")?,
                ));
            }
            value if target_id.is_none() => target_id = Some(value),
            other => bail!("Portboard open argument `{other}` is unexpected"),
        }
        index += 1;
    }

    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    open_or_start_launch_target(&root, &config, target_id)
}

fn resolve_cli_worktree(cwd: Option<&Path>) -> Result<PathBuf> {
    let workspace = match cwd {
        Some(path) => path.to_path_buf(),
        None => env::current_dir().context("Portboard could not read the current directory")?,
    };
    resolve_current_worktree(&workspace)
}

fn print_usage() {
    println!(
        "Portboard\n\nUsage:\n  portboard status [--json] [--cwd PATH]\n  portboard open [TARGET] [--cwd PATH]\n  portboard ensure TARGET --herdr [--wait] [--cwd PATH]\n  portboard url TARGET [ENDPOINT] [--cwd PATH]\n  portboard serve [--cwd PATH] [--bind ADDRESS]\n  portboard panel\n"
    );
}
