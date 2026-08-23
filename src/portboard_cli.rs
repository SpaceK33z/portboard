use std::env;
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::browser_url::{open_browser_url, osc8_hyperlink, UrlOpenOutcome};
use crate::current_workspace_panel::open_current_workspace_panel;
use crate::current_workspace_status::{inspect_worktree_launch_targets, LaunchTargetRuntimeState};
use crate::current_worktree::resolve_current_worktree;
use crate::dashboard_server::serve_worktree_dashboard;
use crate::herdr_workspace::{
    current_herdr_workspace_id, focus_herdr_tab, launch_target_herdr_tab_id,
    open_current_workspace_plugin_panel,
};
use crate::launch_target_config::{load_worktree_launch_targets, LaunchTarget, PortboardConfig};
use crate::launch_target_ensure::{ensure_launch_target_in_herdr, EnsureLaunchTargetOutcome};
use crate::launch_target_open::open_or_start_launch_target;
use crate::launch_target_processes::find_launch_target_processes;
use crate::launch_target_stop::{stop_launch_target, stop_launch_target_and_close_herdr_tab};
use crate::runtime_metadata::load_launch_target_runtime_metadata;
use crate::state_paths::worktree_state_directory;
use crate::worktree_processes::inspect_worktree_processes;

/// Runs the Portboard command line interface for the current workspace.
pub fn run_portboard_cli(arguments: &[String]) -> Result<()> {
    let command = arguments.first().map(String::as_str).unwrap_or("status");
    match command {
        "status" => run_status_command(&arguments[1..]),
        "ps" => run_ps_command(&arguments[1..]),
        "open" => run_open_command(&arguments[1..]),
        "ensure" => run_ensure_command(&arguments[1..]),
        "url" => run_url_command(&arguments[1..]),
        "open-url" => run_open_url_command(&arguments[1..]),
        "logs" => run_logs_command(&arguments[1..]),
        "stop" => run_stop_command(&arguments[1..]),
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
    let inspection = inspect_worktree_launch_targets(&root, &config)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&inspection.statuses)?);
    } else {
        println!("Portboard · {}", root.display());
        for status in &inspection.statuses {
            let endpoint_summary = if status.named_endpoints.is_empty() {
                status
                    .endpoints
                    .iter()
                    .map(|endpoint| format!("{}:{}", endpoint.address, endpoint.port))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                status
                    .named_endpoints
                    .iter()
                    .map(|endpoint| format!("{} {}", endpoint.id, endpoint.url))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let state = match &status.state {
                LaunchTargetRuntimeState::Stopped => "stopped".to_string(),
                LaunchTargetRuntimeState::Running { processes } => {
                    let pids = processes
                        .iter()
                        .map(|process| process.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if status.duplicate {
                        format!(
                            "running · DUPLICATE ({} instances) (pid {pids})",
                            processes.len()
                        )
                    } else {
                        format!("running (pid {pids})")
                    }
                }
            };
            if endpoint_summary.is_empty() {
                println!("  {:<24} {state}", status.label);
            } else {
                println!("  {:<24} {state} · {endpoint_summary}", status.label);
            }
        }
        for finding in &inspection.findings {
            println!("  warning: {finding}");
        }
    }
    Ok(())
}

fn run_ps_command(arguments: &[String]) -> Result<()> {
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
                        .context("Portboard ps --cwd needs a path")?,
                ));
            }
            other => bail!("Portboard ps option `{other}` is unknown"),
        }
        index += 1;
    }

    let root = resolve_cli_worktree(cwd.as_deref())?;
    let targets = optional_worktree_launch_targets(&root)?;
    let entries = inspect_worktree_processes(&root, &targets)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    println!("Portboard · {}", root.display());
    let rows = entries
        .iter()
        .map(|entry| PsRow {
            pid: entry.pid.to_string(),
            target: if entry.target_ids.is_empty() {
                "—".to_string()
            } else {
                entry.target_ids.join(", ")
            },
            ports: if entry.endpoints.is_empty() {
                "—".to_string()
            } else {
                entry
                    .endpoints
                    .iter()
                    .map(|endpoint| format!(":{}", endpoint.port))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            argv: entry.argv.join(" "),
        })
        .collect::<Vec<_>>();
    let pid_width = column_width("PID", rows.iter().map(|row| row.pid.as_str()));
    let target_width = column_width("TARGET", rows.iter().map(|row| row.target.as_str()));
    let port_width = column_width("PORTS", rows.iter().map(|row| row.ports.as_str()));
    println!(
        "{}  {}  {}  ARGV",
        pad_column("PID", pid_width),
        pad_column("TARGET", target_width),
        pad_column("PORTS", port_width)
    );
    for row in &rows {
        println!(
            "{}  {}  {}  {}",
            pad_column(&row.pid, pid_width),
            pad_column(&row.target, target_width),
            pad_column(&row.ports, port_width),
            truncate_column(&row.argv, 60)
        );
    }
    Ok(())
}

struct PsRow {
    pid: String,
    target: String,
    ports: String,
    argv: String,
}

fn column_width<'a>(heading: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values
        .map(|value| value.chars().count())
        .max()
        .unwrap_or_default()
        .max(heading.chars().count())
}

fn pad_column(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    format!("{value}{}", " ".repeat(padding))
}

fn truncate_column(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

/// Loads the worktree manifest when one exists; an absent manifest yields no
/// targets so manifest-less inventories and panels still work.
fn optional_worktree_launch_targets(worktree_root: &Path) -> Result<Vec<LaunchTarget>> {
    if !worktree_root.join("portboard.toml").is_file() {
        return Ok(Vec::new());
    }
    Ok(load_worktree_launch_targets(worktree_root)?.launch_targets)
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
    let mut open = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--open" => open = true,
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
    let target_id = positional
        .first()
        .context("Portboard url needs a launch target id")?;
    if positional.len() > 2 {
        bail!("Portboard url received too many endpoint identifiers");
    }
    let endpoint_id = positional.get(1).copied();
    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let target = configured_launch_target(&config, target_id)?;
    let processes = find_launch_target_processes(&root, target)?;
    if processes.is_empty() {
        bail!("Portboard launch target `{target_id}` is not running");
    }
    let metadata =
        load_launch_target_runtime_metadata(&root, target, &processes)?.with_context(|| {
            format!("Portboard launch target `{target_id}` has no current runtime metadata")
        })?;
    let endpoint = match endpoint_id {
        Some(endpoint_id) => metadata
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == endpoint_id)
            .with_context(|| {
                format!("Portboard launch target `{target_id}` has no endpoint `{endpoint_id}`")
            })?,
        None => metadata.primary_endpoint().with_context(|| {
            format!("Portboard launch target `{target_id}` has no primary endpoint")
        })?,
    };
    if std::io::stdout().is_terminal() {
        print!("{}", osc8_hyperlink(&endpoint.url));
        std::io::stdout()
            .flush()
            .context("Portboard url could not write to stdout")?;
    } else {
        print!("{}", endpoint.url);
    }
    if open {
        match open_browser_url(&endpoint.url) {
            UrlOpenOutcome::OpenedLocally => println!("\n{target_id}: opened in your browser"),
            UrlOpenOutcome::OfferedLink => {
                println!("\n{target_id}: ctrl-click the URL above to open it in your local browser")
            }
        }
    } else {
        println!();
    }
    Ok(())
}

/// Opens a clicked loopback URL (from the Herdr link handler) in a browser.
fn run_open_url_command(arguments: &[String]) -> Result<()> {
    let url = arguments
        .first()
        .cloned()
        .or_else(|| {
            env::var_os("HERDR_PLUGIN_CLICKED_URL")
                .map(|value| value.to_string_lossy().into_owned())
        })
        .context("Portboard open-url needs a URL argument or HERDR_PLUGIN_CLICKED_URL")?;
    // The manifest pattern already restricts clicks to loopback http(s) URLs;
    // revalidate here so this command is safe to invoke by hand too.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("Portboard open-url only opens http and https URLs, got `{url}`");
    }
    match open_browser_url(&url) {
        UrlOpenOutcome::OpenedLocally => println!("opened {url}"),
        UrlOpenOutcome::OfferedLink => {
            println!("{url} (no local browser; ctrl-click it in a pane)")
        }
    }
    Ok(())
}

fn run_logs_command(arguments: &[String]) -> Result<()> {
    let mut target_id = None;
    let mut cwd = None;
    let mut follow = false;
    let mut line_count = 200_u32;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--follow" | "-f" => follow = true,
            "--lines" | "-n" => {
                index += 1;
                line_count = arguments
                    .get(index)
                    .context("Portboard logs --lines needs a positive integer")?
                    .parse()
                    .context("Portboard logs --lines needs a positive integer")?;
                if line_count == 0 {
                    bail!("Portboard logs --lines needs a positive integer");
                }
            }
            "--cwd" => {
                index += 1;
                cwd = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("Portboard logs --cwd needs a path")?,
                ));
            }
            value if target_id.is_none() => target_id = Some(value),
            other => bail!("Portboard logs argument `{other}` is unexpected"),
        }
        index += 1;
    }
    let target_id = target_id.context("Portboard logs needs a launch target id")?;
    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let target = configured_launch_target(&config, target_id)?;

    // A project-declared log file wins: it works for every run host and
    // survives Portboard process restarts.
    if let Some(log_file) = target.log_file.as_deref() {
        print_project_log(&root, target_id, log_file, line_count, follow)?;
        return Ok(());
    }

    let processes = find_launch_target_processes(&root, target)?;

    // A Herdr-hosted run logs to its owning tab's scrollback.
    if !processes.is_empty() {
        if let Some(workspace_id) = current_herdr_workspace_id() {
            if let Some(tab_id) = launch_target_herdr_tab_id(&workspace_id, &processes)? {
                focus_herdr_tab(&tab_id)?;
                println!(
                    "Portboard launch target `{target_id}` logs live in Herdr tab {tab_id}; focused it"
                );
                return Ok(());
            }
        }
    }

    // A dashboard-owned run logs below the Portboard state directory. The
    // newest log is shown even after the run exits so crashes stay inspectable.
    if let Some(log_path) = newest_dashboard_log(&root, target_id)? {
        println!("Portboard · {}", log_path.display());
        print_dashboard_log(&log_path, line_count, follow)?;
        return Ok(());
    }

    // A manually started run has no Portboard-owned log at all.
    if !processes.is_empty() {
        println!("Portboard launch target `{target_id}` is running without Portboard-owned logs:");
        for process in &processes {
            println!("  pid {}: {}", process.pid, process.argv.join(" "));
        }
        println!("Its output is attached to the terminal that started it.");
        return Ok(());
    }

    bail!("Portboard launch target `{target_id}` is not running");
}

/// Prints a bounded tail of one project-declared log file, following it when
/// requested.
fn print_project_log(
    root: &Path,
    target_id: &str,
    log_file: &str,
    line_count: u32,
    follow: bool,
) -> Result<()> {
    let configured_log_path = root.join(log_file);
    if !configured_log_path.is_file() {
        bail!(
            "Portboard launch target `{target_id}` log does not exist at {}",
            configured_log_path.display()
        );
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("Portboard could not resolve worktree {}", root.display()))?;
    let log_path = configured_log_path.canonicalize().with_context(|| {
        format!(
            "Portboard could not resolve {}",
            configured_log_path.display()
        )
    })?;
    if !log_path.starts_with(&canonical_root) {
        bail!(
            "Portboard launch target `{target_id}` log resolves outside the worktree: {}",
            configured_log_path.display()
        );
    }
    let mut command = Command::new("tail");
    command.arg("-n").arg(line_count.to_string());
    if follow {
        command.arg("-F");
    }
    let status = command
        .arg(&log_path)
        .status()
        .with_context(|| format!("Portboard could not read {}", log_path.display()))?;
    if !status.success() {
        bail!("Portboard log reader exited with {status}");
    }
    Ok(())
}

/// Returns the newest dashboard-owned run log for one target, preferring the
/// stable symlink and falling back to the newest timestamped file.
fn newest_dashboard_log(worktree_root: &Path, target_id: &str) -> Result<Option<PathBuf>> {
    let runs_directory = worktree_state_directory(worktree_root)?.join("runs");
    if !runs_directory.is_dir() {
        return Ok(None);
    }
    let link = runs_directory.join(format!("{target_id}.log"));
    if link.is_file() {
        return Ok(Some(link));
    }
    let prefix = format!("{target_id}.");
    let mut stamps = fs::read_dir(&runs_directory)
        .with_context(|| format!("Portboard could not read {}", runs_directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".log")?;
            stamp.parse::<u64>().ok()
        })
        .collect::<Vec<_>>();
    stamps.sort_unstable();
    Ok(stamps
        .last()
        .map(|stamp| runs_directory.join(format!("{prefix}{stamp}.log"))))
}

/// Prints a bounded tail of one dashboard run log, then polls for appended
/// output when following.
fn print_dashboard_log(log_path: &Path, line_count: u32, follow: bool) -> Result<()> {
    print_log_tail(log_path, line_count)?;
    if !follow {
        return Ok(());
    }
    let mut offset = fs::metadata(log_path)
        .with_context(|| format!("Portboard could not read {}", log_path.display()))?
        .len();
    loop {
        thread::sleep(Duration::from_millis(250));
        let length = match fs::metadata(log_path) {
            // A vanished or replaced log ends the follow quietly.
            Err(_) => return Ok(()),
            Ok(metadata) => metadata.len(),
        };
        if length < offset {
            // The log was truncated or replaced; restart from the beginning.
            offset = 0;
        }
        if length == offset {
            continue;
        }
        let mut file = File::open(log_path)
            .with_context(|| format!("Portboard could not read {}", log_path.display()))?;
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("Portboard could not seek {}", log_path.display()))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .with_context(|| format!("Portboard could not read {}", log_path.display()))?;
        offset = length;
        print!("{}", String::from_utf8_lossy(&appended));
        std::io::stdout()
            .flush()
            .context("Portboard logs could not write to stdout")?;
    }
}

/// Prints the last `line_count` lines of one log file.
fn print_log_tail(log_path: &Path, line_count: u32) -> Result<()> {
    let mut contents = Vec::new();
    File::open(log_path)
        .with_context(|| format!("Portboard could not read {}", log_path.display()))?
        .read_to_end(&mut contents)
        .with_context(|| format!("Portboard could not read {}", log_path.display()))?;
    let rendered = String::from_utf8_lossy(&contents);
    let lines: Vec<&str> = rendered.lines().collect::<Vec<_>>();
    let skip = lines.len().saturating_sub(line_count as usize);
    for line in &lines[skip..] {
        println!("{line}");
    }
    Ok(())
}

fn run_stop_command(arguments: &[String]) -> Result<()> {
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
                        .context("Portboard stop --cwd needs a path")?,
                ));
            }
            value if target_id.is_none() => target_id = Some(value),
            other => bail!("Portboard stop argument `{other}` is unexpected"),
        }
        index += 1;
    }
    let target_id = target_id.context("Portboard stop needs a launch target id")?;
    let root = resolve_cli_worktree(cwd.as_deref())?;
    let config = load_worktree_launch_targets(&root)?;
    let target = configured_launch_target(&config, target_id)?;
    let stopped_pids = match current_herdr_workspace_id() {
        Some(workspace_id) => stop_launch_target_and_close_herdr_tab(&root, target, &workspace_id)?,
        None => stop_launch_target(&root, target)?,
    };
    if stopped_pids.is_empty() {
        println!("{target_id}: already stopped");
    } else {
        println!(
            "{target_id}: stopping pid {}",
            stopped_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
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

fn configured_launch_target<'a>(
    config: &'a PortboardConfig,
    target_id: &str,
) -> Result<&'a LaunchTarget> {
    config
        .launch_targets
        .iter()
        .find(|target| target.id.as_str() == target_id)
        .with_context(|| format!("Portboard launch target `{target_id}` is not configured"))
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
        "Portboard\n\nUsage:\n  portboard status [--json] [--cwd PATH]\n  portboard ps [--json] [--cwd PATH]\n  portboard open [TARGET] [--cwd PATH]\n  portboard ensure TARGET --herdr [--wait] [--cwd PATH]\n  portboard url TARGET [ENDPOINT] [--open] [--cwd PATH]\n  portboard open-url [URL]\n  portboard logs TARGET [--lines N] [--follow] [--cwd PATH]\n  portboard stop TARGET [--cwd PATH]\n  portboard serve [--cwd PATH] [--bind ADDRESS]\n  portboard panel\n"
    );
}
