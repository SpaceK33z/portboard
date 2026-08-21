use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::current_workspace_status::{
    inspect_worktree_launch_targets, CurrentLaunchTargetStatus, LaunchTargetRuntimeState,
};
use crate::launch_target_config::load_worktree_launch_targets;
use crate::launch_target_open::open_or_start_launch_target;
use crate::launch_target_stop::stop_launch_target;

const PANEL_FOOTER: &str = "\x1b[2menter\x1b[0m open/start · \x1b[2mctrl-s\x1b[0m stop · \x1b[2mctrl-r\x1b[0m refresh · \x1b[2mesc\x1b[0m close";
const INFORMATION_FOOTER: &str = "\x1b[2mesc\x1b[0m close";

/// Opens the current worktree launch targets in a transient fzf panel.
pub fn open_current_workspace_panel(worktree_root: &Path) -> Result<()> {
    if !worktree_root.join("portboard.toml").is_file() {
        return open_information_panel(&format!(
            "No portboard.toml in {:?} · add one to configure launch targets",
            worktree_root
        ));
    }

    loop {
        let config = load_worktree_launch_targets(worktree_root)?;
        let statuses = inspect_worktree_launch_targets(worktree_root, &config)?;
        if statuses.is_empty() {
            bail!("Portboard manifest has no launch targets");
        }

        let rows = panel_rows(&statuses);
        let target_width = column_width("TARGET", rows.iter().map(|row| row.target.as_str()));
        let status_width = column_width("STATUS", rows.iter().map(|row| row.status));
        let endpoints_width =
            column_width("ENDPOINTS", rows.iter().map(|row| row.endpoints.as_str()));
        let header = format!(
            "{}  {}  {}  {}",
            pad_column("TARGET", target_width),
            pad_column("STATUS", status_width),
            pad_column("ENDPOINTS", endpoints_width),
            "PIDS"
        );

        let mut fzf = Command::new("fzf");
        fzf.args([
            "--delimiter=\\t",
            "--with-nth=2",
            "--no-sort",
            "--reverse",
            "--info=inline",
            "--border=rounded",
            "--ansi",
            "--prompt=portboard ❯ ",
            "--expect=enter,ctrl-s,ctrl-r",
            "--header",
            &header,
            "--footer",
            PANEL_FOOTER,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
        let mut fzf = fzf.spawn().context("Portboard panel could not start fzf")?;

        {
            let input = fzf
                .stdin
                .as_mut()
                .context("Portboard panel could not open fzf input")?;
            for row in &rows {
                writeln!(
                    input,
                    "{}\t{}  {}  {}  {}",
                    row.id,
                    pad_column(&row.target, target_width),
                    pad_column(row.status, status_width),
                    pad_column(&row.endpoints, endpoints_width),
                    row.pids
                )?;
            }
        }

        let output = fzf
            .wait_with_output()
            .context("Portboard panel could not wait for fzf")?;
        if !output.status.success() {
            // fzf uses 1 for no match and 130 for an interrupted/cancelled
            // selection. Both are normal ways to close a transient panel.
            if matches!(output.status.code(), Some(1 | 130)) {
                return Ok(());
            }
            bail!("Portboard panel fzf exited with {}", output.status);
        }

        let selection = String::from_utf8(output.stdout)
            .context("Portboard panel received a non-UTF-8 fzf selection")?;
        let mut lines = selection.lines();
        let key = lines.next().unwrap_or_default();
        if key == "ctrl-r" {
            continue;
        }
        let selected = lines
            .next()
            .filter(|value| !value.is_empty())
            .context("Portboard panel selection did not contain a launch target")?;
        let target_id = selected
            .split('\t')
            .next()
            .filter(|value| !value.is_empty())
            .context("Portboard panel selection did not contain a launch target id")?;

        if key == "ctrl-s" {
            let refreshed_config = load_worktree_launch_targets(worktree_root)?;
            if refreshed_config != config {
                continue;
            }
            let target = config
                .launch_targets
                .iter()
                .find(|target| target.id.as_str() == target_id)
                .context("Portboard panel selected an unknown launch target")?;
            match stop_launch_target(worktree_root, target) {
                Ok(_) => continue,
                Err(error) => {
                    eprintln!("{error:#}");
                    continue;
                }
            }
        }

        let refreshed_config = load_worktree_launch_targets(worktree_root)?;
        if refreshed_config != config {
            continue;
        }
        return open_or_start_launch_target(worktree_root, &config, Some(target_id));
    }
}

struct PanelRow {
    id: String,
    target: String,
    status: &'static str,
    endpoints: String,
    pids: String,
}

fn panel_rows(statuses: &[CurrentLaunchTargetStatus]) -> Vec<PanelRow> {
    statuses
        .iter()
        .map(|target| {
            let (status, pids) = match &target.state {
                LaunchTargetRuntimeState::Stopped => ("stopped", "—".to_string()),
                LaunchTargetRuntimeState::Running { processes } => (
                    "running",
                    processes
                        .iter()
                        .map(|process| process.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            };
            let endpoints = if target.endpoints.is_empty() {
                "—".to_string()
            } else {
                target
                    .endpoints
                    .iter()
                    .map(|endpoint| {
                        if endpoint.address.contains(':') {
                            format!("[{}]:{}", endpoint.address, endpoint.port)
                        } else {
                            format!("{}:{}", endpoint.address, endpoint.port)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            PanelRow {
                id: target.id.as_str().to_string(),
                target: truncate_column(&target.label, 32),
                status,
                endpoints: truncate_column(&endpoints, 42),
                pids: truncate_column(&pids, 24),
            }
        })
        .collect()
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

fn open_information_panel(message: &str) -> Result<()> {
    let mut fzf = Command::new("fzf")
        .args([
            "--disabled",
            "--no-sort",
            "--reverse",
            "--info=inline",
            "--border=rounded",
            "--ansi",
            "--prompt=portboard ❯ ",
            "--footer",
            INFORMATION_FOOTER,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("Portboard panel could not start fzf")?;
    writeln!(
        fzf.stdin
            .as_mut()
            .context("Portboard panel could not open fzf input")?,
        "{message}"
    )?;
    let status = fzf
        .wait()
        .context("Portboard panel could not wait for fzf")?;
    if status.success() || matches!(status.code(), Some(1 | 130)) {
        return Ok(());
    }
    bail!("Portboard panel fzf exited with {status}")
}
