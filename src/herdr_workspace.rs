use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::{process_has_portboard_target_identity, LaunchTargetProcess};

/// Current workspace data injected when a Herdr plugin action is invoked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct HerdrPluginContext {
    pub workspace_id: Option<String>,
    pub workspace_cwd: Option<String>,
    pub focused_pane_id: Option<String>,
}

/// Parses the workspace context supplied to a Herdr plugin action.
pub fn parse_herdr_plugin_context(contents: &str) -> Result<HerdrPluginContext> {
    serde_json::from_str(contents).context("Portboard Herdr context is not valid JSON")
}

/// Opens the current-workspace Portboard popup from a Herdr plugin action.
pub fn open_current_workspace_plugin_panel() -> Result<()> {
    let context_json = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .context("Portboard Herdr action did not receive HERDR_PLUGIN_CONTEXT_JSON")?;
    let context = parse_herdr_plugin_context(&context_json)?;
    let workspace_id = context
        .workspace_id
        .context("Portboard Herdr action did not receive a workspace id")?;
    let workspace_cwd = context
        .workspace_cwd
        .context("Portboard Herdr action did not receive a workspace cwd")?;
    let plugin_id = env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "portboard".to_string());
    let workspace_environment = format!("PORTBOARD_WORKSPACE_ID={workspace_id}");

    run_herdr_command(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        &plugin_id,
        "--entrypoint",
        "current",
        "--cwd",
        &workspace_cwd,
        "--env",
        &workspace_environment,
        "--focus",
    ])?;
    Ok(())
}

/// Returns the workspace selected for a plugin pane or ordinary Herdr terminal.
pub fn current_herdr_workspace_id() -> Option<String> {
    env::var("PORTBOARD_WORKSPACE_ID")
        .ok()
        .or_else(|| env::var("HERDR_WORKSPACE_ID").ok())
        .filter(|value| !value.is_empty())
}

/// Starts one launch target in a dedicated tab of the current Herdr workspace.
pub fn launch_target_in_herdr(
    worktree_root: &Path,
    workspace_id: &str,
    target: &LaunchTarget,
    focus_after_start: bool,
) -> Result<u32> {
    let cwd = worktree_root.to_string_lossy().into_owned();
    let run_identity = format!("PORTBOARD_TARGET_ID={}", target.id.as_str());
    let response: HerdrResponse<HerdrTabCreateResult> = run_herdr_json(&[
        "tab",
        "create",
        "--workspace",
        workspace_id,
        "--cwd",
        &cwd,
        "--label",
        target.id.as_str(),
        "--env",
        &run_identity,
        "--no-focus",
    ])?;
    let tab_id = response.result.tab.tab_id;
    let pane_id = response.result.root_pane.pane_id;

    // `herdr pane run` joins its arguments with spaces and lets the pane's
    // shell re-parse the line, so every argument needs shell quoting to
    // survive; otherwise spaces and metacharacters split or reinterpret.
    let mut run_arguments = vec!["pane", "run", pane_id.as_str()];
    let quoted_argv: Vec<String> = target
        .argv
        .iter()
        .map(|argument| shell_quote(argument))
        .collect();
    run_arguments.extend(quoted_argv.iter().map(String::as_str));
    if let Err(error) = run_herdr_command(&run_arguments) {
        let _ = run_herdr_command(&["tab", "close", &tab_id]);
        return Err(error).context("Portboard Herdr launch could not start the target command");
    }
    // Manifests may run preparation hooks (dependency installs, reapers)
    // before the target command itself appears, so allow a generous window.
    let Some(launcher_pid) = foreground_process_pid(&pane_id, Duration::from_secs(10)) else {
        let _ = run_herdr_command(&["tab", "close", &tab_id]);
        bail!(
            "Portboard Herdr launch could not identify the target process; the new tab was closed"
        );
    };
    if focus_after_start {
        run_herdr_command(&["tab", "focus", &tab_id])?;
    }
    Ok(launcher_pid)
}

/// Quotes one argument as a POSIX shell word. Single quotes preserve every
/// character except the single quote itself, which closes the word, escapes
/// via `\'`, and reopens.
fn shell_quote(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('\'');
    for character in argument.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// Resolves the pid of the command started via `herdr pane run`. Right after
/// the run request the pane's foreground process is still its interactive
/// shell; reserving that shell would pin the launch reservation to a process
/// that outlives the target, so shell pids are skipped until the real command
/// appears (or the timeout expires).
fn foreground_process_pid(pane_id: &str, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = run_herdr_json::<HerdrResponse<HerdrPaneProcessResult>>(&[
            "pane",
            "process-info",
            "--pane",
            pane_id,
        ]) {
            let shell_pid = response.result.process_info.shell_pid;
            if let Some(process) = response
                .result
                .process_info
                .foreground_processes
                .iter()
                .find(|process| process.pid != shell_pid)
            {
                return Some(process.pid);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Focuses the Herdr tab that owns any of the matching launch target processes.
pub fn focus_launch_target_in_herdr(
    workspace_id: &str,
    processes: &[LaunchTargetProcess],
) -> Result<bool> {
    let Some(tab_id) = launch_target_herdr_tab_id(workspace_id, processes)? else {
        return Ok(false);
    };
    run_herdr_command(&["tab", "focus", &tab_id])?;
    Ok(true)
}

/// Returns whether a target process belongs to a pane in the given Herdr workspace.
pub fn launch_target_runs_in_herdr(
    workspace_id: &str,
    processes: &[LaunchTargetProcess],
) -> Result<bool> {
    Ok(launch_target_herdr_tab_id(workspace_id, processes)?.is_some())
}

/// Returns the Herdr tab containing one of the matching target processes.
pub fn launch_target_herdr_tab_id(
    workspace_id: &str,
    processes: &[LaunchTargetProcess],
) -> Result<Option<String>> {
    let response: HerdrResponse<HerdrPaneListResult> =
        run_herdr_json(&["pane", "list", "--workspace", workspace_id])?;
    for pane in response.result.panes {
        let process_response = run_herdr_json::<HerdrResponse<HerdrPaneProcessResult>>(&[
            "pane",
            "process-info",
            "--pane",
            &pane.pane_id,
        ]);
        let Ok(process_response) = process_response else {
            continue;
        };
        let shell_pid = process_response.result.process_info.shell_pid;
        if !processes
            .iter()
            .any(|process| process_descends_from(process.pid, shell_pid))
        {
            continue;
        }
        return Ok(Some(pane.tab_id));
    }
    Ok(None)
}

/// Closes leftover Herdr tabs that a launch target left behind after dying on
/// its own (crash, Ctrl+C in the pane, external kill). A tab is stale when its
/// label matches the target id and every pane in it is a Portboard-owned host
/// shell with no foreground command; busy or foreign panes are never touched.
/// Returns the closed tab ids.
pub fn reap_stale_launch_target_tabs(
    workspace_id: &str,
    target: &LaunchTarget,
) -> Result<Vec<String>> {
    let tabs: HerdrResponse<HerdrTabListResult> =
        run_herdr_json(&["tab", "list", "--workspace", workspace_id])?;
    let panes: HerdrResponse<HerdrPaneListResult> =
        run_herdr_json(&["pane", "list", "--workspace", workspace_id])?;
    let mut pane_ids_by_tab: HashMap<String, Vec<String>> = HashMap::new();
    for pane in panes.result.panes {
        pane_ids_by_tab
            .entry(pane.tab_id)
            .or_default()
            .push(pane.pane_id);
    }

    let mut closed_tabs = Vec::new();
    for tab in &tabs.result.tabs {
        if tab.label != target.id.as_str() {
            continue;
        }
        let Some(pane_ids) = pane_ids_by_tab.get(&tab.tab_id) else {
            continue;
        };
        let liveness = pane_ids
            .iter()
            .map(|pane_id| pane_liveness(pane_id, target.id.as_str()))
            .collect::<Vec<_>>();
        if !tab_is_reapable(&liveness) {
            continue;
        }
        run_herdr_command(&["tab", "close", &tab.tab_id])?;
        closed_tabs.push(tab.tab_id.clone());
    }
    Ok(closed_tabs)
}

struct LaunchTargetPaneLiveness {
    portboard_owned: bool,
    has_foreground_command: bool,
}

fn pane_liveness(pane_id: &str, target_id: &str) -> LaunchTargetPaneLiveness {
    let default = LaunchTargetPaneLiveness {
        portboard_owned: false,
        has_foreground_command: true,
    };
    let Ok(response) = run_herdr_json::<HerdrResponse<HerdrPaneProcessResult>>(&[
        "pane",
        "process-info",
        "--pane",
        pane_id,
    ]) else {
        return default;
    };
    let process_info = response.result.process_info;
    let portboard_owned = process_has_portboard_target_identity(
        &crate::launch_target_processes::LaunchTargetProcess {
            pid: process_info.shell_pid,
            argv: Vec::new(),
        },
        target_id,
    );
    LaunchTargetPaneLiveness {
        portboard_owned,
        has_foreground_command: process_info
            .foreground_processes
            .iter()
            .any(|process| process.pid != process_info.shell_pid),
    }
}

/// Decides whether a candidate tab may be reaped. Every pane must be an idle
/// Portboard-owned host shell, so user work in splits is never discarded and a
/// live run keeps its tab.
fn tab_is_reapable(panes: &[LaunchTargetPaneLiveness]) -> bool {
    !panes.is_empty()
        && panes
            .iter()
            .all(|pane| pane.portboard_owned && !pane.has_foreground_command)
}

/// Closes a Herdr tab after its Portboard-owned target has stopped.
pub fn close_launch_target_herdr_tab(tab_id: &str) -> Result<()> {
    run_herdr_command(&["tab", "close", tab_id])
}

/// Focuses an explicit Herdr tab by identifier.
pub fn focus_herdr_tab(tab_id: &str) -> Result<()> {
    run_herdr_command(&["tab", "focus", tab_id])
}

fn run_herdr_json<T: DeserializeOwned>(arguments: &[&str]) -> Result<T> {
    let output = herdr_command().args(arguments).output().with_context(|| {
        format!(
            "Portboard Herdr command could not run: {}",
            arguments.join(" ")
        )
    })?;
    require_herdr_success(arguments, &output)?;
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "Portboard Herdr command returned invalid JSON: {}",
            arguments.join(" ")
        )
    })
}

fn run_herdr_command(arguments: &[&str]) -> Result<()> {
    let output = herdr_command().args(arguments).output().with_context(|| {
        format!(
            "Portboard Herdr command could not run: {}",
            arguments.join(" ")
        )
    })?;
    require_herdr_success(arguments, &output)
}

fn herdr_command() -> Command {
    Command::new(env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()))
}

fn require_herdr_success(arguments: &[&str], output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "Portboard Herdr command failed: {}: {}",
        arguments.join(" "),
        stderr.trim()
    )
}

fn process_descends_from(mut pid: u32, ancestor_pid: u32) -> bool {
    for _ in 0..128 {
        if pid == ancestor_pid {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        let Some(parent_pid) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        }) else {
            return false;
        };
        pid = parent_pid;
    }
    false
}

#[derive(Deserialize)]
struct HerdrResponse<T> {
    result: T,
}

#[derive(Deserialize)]
struct HerdrTabCreateResult {
    tab: HerdrTabIdentity,
    root_pane: HerdrPaneIdentity,
}

#[derive(Deserialize)]
struct HerdrTabIdentity {
    tab_id: String,
}

#[derive(Deserialize)]
struct HerdrPaneIdentity {
    pane_id: String,
}

#[derive(Deserialize)]
struct HerdrTabListResult {
    #[serde(default)]
    tabs: Vec<HerdrTabRecord>,
}

#[derive(Deserialize)]
struct HerdrTabRecord {
    tab_id: String,
    label: String,
}

#[derive(Deserialize)]
struct HerdrPaneListResult {
    panes: Vec<HerdrPaneRecord>,
}

#[derive(Deserialize)]
struct HerdrPaneRecord {
    pane_id: String,
    tab_id: String,
}

#[derive(Deserialize)]
struct HerdrPaneProcessResult {
    process_info: HerdrPaneProcessInfo,
}

#[derive(Deserialize)]
struct HerdrPaneProcessInfo {
    shell_pid: u32,
    #[serde(default)]
    foreground_processes: Vec<HerdrForegroundProcess>,
}

#[derive(Deserialize)]
struct HerdrForegroundProcess {
    pid: u32,
}

#[cfg(test)]
mod tests {
    use super::{
        parse_herdr_plugin_context, shell_quote, tab_is_reapable, HerdrTabRecord,
        LaunchTargetPaneLiveness,
    };

    fn pane(portboard_owned: bool, has_foreground_command: bool) -> LaunchTargetPaneLiveness {
        LaunchTargetPaneLiveness {
            portboard_owned,
            has_foreground_command,
        }
    }

    #[test]
    fn reaps_a_tab_whose_host_shells_are_all_idle_and_owned() {
        assert!(tab_is_reapable(&[pane(true, false)]));
        assert!(tab_is_reapable(&[pane(true, false), pane(true, false)]));
    }

    #[test]
    fn keeps_tabs_with_busy_or_foreign_panes() {
        // The server is still running in one of the panes.
        assert!(!tab_is_reapable(&[pane(true, true)]));
        // A foreign shell (user split, no Portboard marker) blocks reaping.
        assert!(!tab_is_reapable(&[pane(true, false), pane(false, false)]));
        // Nothing Portboard-owned means the tab is not ours to close.
        assert!(!tab_is_reapable(&[pane(false, false)]));
        assert!(!tab_is_reapable(&[]));
    }

    #[test]
    fn tab_records_need_label_and_id_only() {
        let record: HerdrTabRecord =
            serde_json::from_str(r#"{"tab_id":"w2:t31","label":"dev","number":97,"pane_count":1}"#)
                .expect("tab record");
        assert_eq!(record.tab_id, "w2:t31");
        assert_eq!(record.label, "dev");
    }

    #[test]
    fn shell_quotes_arguments_for_pane_run() {
        assert_eq!(shell_quote("sleep"), "'sleep'");
        assert_eq!(
            shell_quote("exec -a renamed sleep 300"),
            "'exec -a renamed sleep 300'"
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME `x` ;"), "'$HOME `x` ;'");
    }

    #[test]
    fn parses_workspace_identity_from_plugin_action_context() {
        let context = parse_herdr_plugin_context(
            r#"{"workspace_id":"w6","workspace_cwd":"/tmp/project","focused_pane_id":"w6:p1"}"#,
        )
        .expect("plugin context");

        assert_eq!(context.workspace_id.as_deref(), Some("w6"));
        assert_eq!(context.workspace_cwd.as_deref(), Some("/tmp/project"));
    }
}
