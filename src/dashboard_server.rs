use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::net::ToSocketAddrs;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::current_workspace_status::inspect_worktree_launch_targets;
use crate::launch_target_config::{load_worktree_launch_targets, LaunchTarget};
use crate::launch_target_lock::LaunchTargetLock;
use crate::launch_target_processes::find_launch_target_processes;
use crate::launch_target_stop::stop_launch_target;
use crate::state_paths::worktree_state_directory;

const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:9777";

/// How many timestamped run logs are retained per launch target.
const LOGS_TO_KEEP: usize = 5;

/// Runs the current-worktree dashboard and JSON API on a loopback address.
pub fn serve_worktree_dashboard(worktree_root: &Path, bind_address: Option<&str>) -> Result<()> {
    // Fail before binding if the repository has no usable manifest.
    load_worktree_launch_targets(worktree_root)?;

    let bind_address = bind_address.unwrap_or(DEFAULT_BIND_ADDRESS);
    if !is_loopback_bind_address(bind_address) {
        bail!("Portboard dashboard only binds to loopback addresses; got `{bind_address}`");
    }
    let server = Server::http(bind_address)
        .map_err(|error| anyhow!("Portboard dashboard could not bind {bind_address}: {error}"))?;
    let token = request_token()?;
    let allowed_hosts = allowed_host_headers(bind_address)?;
    let mut state = DashboardState::new(worktree_root.to_path_buf())?;

    println!("Portboard dashboard: http://{bind_address}");
    loop {
        state.reap_finished_children();
        let Some(request) = server
            .recv_timeout(Duration::from_millis(250))
            .context("Portboard dashboard could not receive an HTTP request")?
        else {
            continue;
        };
        if let Err(error) = handle_request(request, &mut state, &token, &allowed_hosts) {
            eprintln!("Portboard dashboard request failed: {error:#}");
        }
    }
}

fn is_loopback_bind_address(address: &str) -> bool {
    let Ok(addresses) = address.to_socket_addrs() else {
        return false;
    };
    let addresses = addresses.collect::<Vec<_>>();
    !addresses.is_empty() && addresses.iter().all(|address| address.ip().is_loopback())
}

fn handle_request(
    request: Request,
    state: &mut DashboardState,
    token: &str,
    allowed_hosts: &[String],
) -> Result<()> {
    let method = request.method().clone();
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or(request.url())
        .to_string();

    if !has_allowed_host(&request, allowed_hosts) {
        return respond_json(
            request,
            StatusCode(421),
            &ApiMessage {
                message: "invalid Host header".to_string(),
            },
        );
    }

    if method == Method::Post && !has_request_token(&request, token) {
        return respond_json(
            request,
            StatusCode(403),
            &ApiMessage {
                message: "missing or invalid X-Portboard-Token".to_string(),
            },
        );
    }

    let response = (|| -> Result<(StatusCode, &'static str, String)> {
        match (method, path.as_str()) {
            (Method::Get, "/") => Ok((
                StatusCode(200),
                "text/html; charset=utf-8",
                dashboard_html(token),
            )),
            (Method::Get, "/api/status") => {
                let config = load_worktree_launch_targets(&state.worktree_root)?;
                let inspection = inspect_worktree_launch_targets(&state.worktree_root, &config)?;
                let targets = inspection
                    .statuses
                    .iter()
                    .zip(config.launch_targets.iter())
                    .map(|(status, _target)| DashboardTargetStatus { status })
                    .collect::<Vec<_>>();
                let status = DashboardStatus {
                    worktree_root: state.worktree_root.to_string_lossy().into_owned(),
                    findings: &inspection.findings,
                    targets,
                };
                Ok((
                    StatusCode(200),
                    "application/json; charset=utf-8",
                    serde_json::to_string_pretty(&status)?,
                ))
            }
            (Method::Post, path) if path.starts_with("/api/open/") => {
                let target_id = &path["/api/open/".len()..];
                let result = state.start_target(target_id)?;
                Ok((
                    StatusCode(200),
                    "application/json; charset=utf-8",
                    serde_json::to_string_pretty(&result)?,
                ))
            }
            (Method::Post, path) if path.starts_with("/api/stop/") => {
                let target_id = &path["/api/stop/".len()..];
                let result = state.stop_target(target_id)?;
                Ok((
                    StatusCode(200),
                    "application/json; charset=utf-8",
                    serde_json::to_string_pretty(&result)?,
                ))
            }
            _ => Ok((
                StatusCode(404),
                "application/json; charset=utf-8",
                serde_json::to_string_pretty(&ApiMessage {
                    message: "not found".to_string(),
                })?,
            )),
        }
    })();

    let (status, response_type, body) = match response {
        Ok(response) => response,
        Err(error) => (
            StatusCode(500),
            "application/json; charset=utf-8",
            serde_json::to_string_pretty(&ApiMessage {
                message: format!("{error:#}"),
            })?,
        ),
    };
    respond(
        request,
        Response::from_string(body)
            .with_status_code(status)
            .with_header(content_type(response_type)),
    )
}

fn has_allowed_host(request: &Request, allowed_hosts: &[String]) -> bool {
    let Some(host) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Host"))
        .map(|header| header.value.as_str().to_ascii_lowercase())
    else {
        return false;
    };
    allowed_hosts.iter().any(|allowed| allowed == &host)
}

fn has_request_token(request: &Request, expected: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("X-Portboard-Token"))
        .map(|header| header.value.as_str())
        == Some(expected)
}

fn respond_json<T: Serialize>(request: Request, status: StatusCode, value: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    respond(
        request,
        Response::from_string(body)
            .with_status_code(status)
            .with_header(content_type("application/json; charset=utf-8")),
    )
}

fn respond<R: io::Read + Send + 'static>(request: Request, response: Response<R>) -> Result<()> {
    request
        .respond(response)
        .context("Portboard dashboard could not write an HTTP response")
}

fn content_type(value: &'static str) -> Header {
    Header::from_bytes("Content-Type", value).expect("static HTTP header")
}

struct DashboardState {
    worktree_root: PathBuf,
    log_directory: PathBuf,
    children: HashMap<String, Child>,
}

impl DashboardState {
    fn new(worktree_root: PathBuf) -> Result<Self> {
        let log_directory = worktree_state_directory(&worktree_root)?.join("runs");
        fs::create_dir_all(&log_directory).with_context(|| {
            format!(
                "Portboard dashboard could not create {}",
                log_directory.display()
            )
        })?;
        Ok(Self {
            worktree_root,
            log_directory,
            children: HashMap::new(),
        })
    }

    fn start_target(&mut self, target_id: &str) -> Result<StartResult> {
        if let Some(child) = self.children.get_mut(target_id) {
            let child_running = child
                .try_wait()
                .context("Portboard dashboard could not read child status")?
                .is_none();
            if child_running || process_group_exists(child.id()) {
                return Ok(StartResult {
                    outcome: "already_running",
                    pid: Some(child.id()),
                    log_path: Some(self.log_directory.join(format!("{target_id}.log"))),
                });
            }
        }
        self.children.remove(target_id);

        let config = load_worktree_launch_targets(&self.worktree_root)?;
        let target = find_target(&config.launch_targets, target_id)?;
        let mut launch_lock = LaunchTargetLock::acquire(&self.worktree_root, target)?;
        let existing = find_launch_target_processes(&self.worktree_root, target)?;
        if !existing.is_empty() {
            launch_lock.clear()?;
            return Ok(StartResult {
                outcome: "already_running",
                pid: existing.first().map(|process| process.pid),
                log_path: None,
            });
        }

        if launch_lock.has_active_reservation()? {
            return Ok(StartResult {
                outcome: "starting",
                pid: None,
                log_path: None,
            });
        }

        let (log_path, log) = rotate_log(&self.log_directory, target.id.as_str())?;
        let stderr = log.try_clone().with_context(|| {
            format!("Portboard dashboard could not clone {}", log_path.display())
        })?;
        let mut command = detached_command(target, &self.worktree_root);
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| {
                format!(
                    "Portboard dashboard could not start `{}`",
                    target.argv.join(" ")
                )
            })?;
        let pid = child.id();
        launch_lock.reserve_process(pid)?;
        self.children.insert(target.id.as_str().to_string(), child);
        Ok(StartResult {
            outcome: "started",
            pid: Some(pid),
            log_path: Some(log_path),
        })
    }

    fn stop_target(&mut self, target_id: &str) -> Result<StopResult> {
        let config = load_worktree_launch_targets(&self.worktree_root)?;
        let target = find_target(&config.launch_targets, target_id)?;

        if let Some(child) = self.children.get(target_id) {
            let pid = i32::try_from(child.id())
                .with_context(|| format!("Portboard cannot signal pid {}", child.id()))?;
            // Dashboard children are process-group leaders (see
            // `detached_command`), so a negative pid stops the complete run.
            // SAFETY: libc::kill does not retain pointers and `pid` is positive.
            let result = unsafe { libc::kill(-pid, libc::SIGTERM) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error)
                        .with_context(|| format!("Portboard could not stop process group {pid}"));
                }
            }
            return Ok(StopResult {
                outcome: "stopping",
                pids: vec![pid as u32],
            });
        }

        let pids = stop_launch_target(&self.worktree_root, target)?;
        Ok(StopResult {
            outcome: "stopping",
            pids,
        })
    }

    fn reap_finished_children(&mut self) {
        self.children.retain(|_, child| match child.try_wait() {
            Ok(Some(_)) => process_group_exists(child.id()),
            Ok(None) => true,
            Err(error) => {
                eprintln!("Portboard dashboard could not read child status: {error}");
                false
            }
        });
    }
}

#[cfg(unix)]
fn process_group_exists(leader_pid: u32) -> bool {
    let Ok(group_id) = i32::try_from(leader_pid) else {
        return false;
    };
    // SAFETY: signal 0 performs existence/permission checking only.
    let result = unsafe { libc::kill(-group_id, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn detached_command(target: &LaunchTarget, worktree_root: &Path) -> Command {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(&target.argv[0]);
    command
        .args(&target.argv[1..])
        .current_dir(worktree_root)
        .process_group(0);
    command
}

/// Creates a fresh timestamped log file, repoints the stable `{id}.log`
/// symlink at it, and prunes older runs beyond [`LOGS_TO_KEEP`].
fn rotate_log(log_directory: &Path, target_id: &str) -> Result<(PathBuf, File)> {
    let mut stamp = unix_time_millis();
    let log_path = loop {
        let candidate = log_directory.join(format!("{target_id}.{stamp}.log"));
        if !candidate.exists() {
            break candidate;
        }
        stamp += 1;
    };
    let log = open_log(&log_path)?;
    let link_path = log_directory.join(format!("{target_id}.log"));
    let _ = fs::remove_file(&link_path);
    symlink(
        log_path.file_name().with_context(|| {
            format!("Portboard dashboard could not name {}", log_path.display())
        })?,
        &link_path,
    )
    .with_context(|| format!("Portboard dashboard could not link {}", link_path.display()))?;
    prune_old_logs(log_directory, target_id, LOGS_TO_KEEP)?;
    Ok((log_path, log))
}

fn open_log(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Portboard dashboard could not open {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Portboard dashboard could not secure {}", path.display()))?;
    Ok(file)
}

/// Returns milliseconds since the Unix epoch for timestamped log names.
fn unix_time_millis() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or_default()
}

/// Collects the numeric stamps of one target's timestamped run logs.
fn timestamped_log_stamps(log_directory: &Path, target_id: &str) -> Result<Vec<u64>> {
    let prefix = format!("{target_id}.");
    let mut stamps = fs::read_dir(log_directory)
        .with_context(|| {
            format!(
                "Portboard dashboard could not read {}",
                log_directory.display()
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".log")?;
            stamp.parse::<u64>().ok()
        })
        .collect::<Vec<_>>();
    stamps.sort_unstable();
    stamps.dedup();
    Ok(stamps)
}

/// Removes the oldest timestamped run logs so only the newest `keep` remain.
fn prune_old_logs(log_directory: &Path, target_id: &str, keep: usize) -> Result<()> {
    let prefix = format!("{target_id}.");
    let stamps = timestamped_log_stamps(log_directory, target_id)?;
    let cutoff = stamps.len().saturating_sub(keep);
    for stamp in &stamps[..cutoff] {
        let path = log_directory.join(format!("{prefix}{stamp}.log"));
        fs::remove_file(&path)
            .with_context(|| format!("Portboard dashboard could not prune {}", path.display()))?;
    }
    Ok(())
}

fn find_target<'a>(targets: &'a [LaunchTarget], target_id: &str) -> Result<&'a LaunchTarget> {
    targets
        .iter()
        .find(|target| target.id.as_str() == target_id)
        .with_context(|| format!("Portboard launch target `{target_id}` is not configured"))
}

fn allowed_host_headers(bind_address: &str) -> Result<Vec<String>> {
    let mut allowed = vec![bind_address.to_ascii_lowercase()];
    for address in bind_address
        .to_socket_addrs()
        .with_context(|| format!("Portboard could not resolve {bind_address}"))?
    {
        allowed.push(address.to_string().to_ascii_lowercase());
    }
    allowed.sort();
    allowed.dedup();
    Ok(allowed)
}

fn request_token() -> Result<String> {
    let mut random = [0_u8; 32];
    File::open("/dev/urandom")
        .context("Portboard dashboard could not open /dev/urandom")?
        .read_exact(&mut random)
        .context("Portboard dashboard could not create a request token")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Serialize)]
struct DashboardStatus<'a> {
    worktree_root: String,
    findings: &'a [String],
    targets: Vec<DashboardTargetStatus<'a>>,
}

#[derive(Serialize)]
struct DashboardTargetStatus<'a> {
    #[serde(flatten)]
    status: &'a crate::current_workspace_status::CurrentLaunchTargetStatus,
}

#[derive(Serialize)]
struct ApiMessage {
    message: String,
}

#[derive(Serialize)]
struct StartResult {
    outcome: &'static str,
    pid: Option<u32>,
    log_path: Option<PathBuf>,
}

#[derive(Serialize)]
struct StopResult {
    outcome: &'static str,
    pids: Vec<u32>,
}

fn dashboard_html(token: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Portboard</title>
<style>
:root {{ color-scheme: dark; font-family: ui-monospace, monospace; }}
body {{ max-width: 900px; margin: 3rem auto; padding: 0 1rem; background: #101216; color: #e6e8ee; }}
h1 {{ font-size: 1.4rem; }}
.target {{ display: grid; grid-template-columns: 1fr auto auto; gap: 1rem; align-items: center; padding: .9rem 0; border-bottom: 1px solid #2a2e38; }}
.state {{ color: #9da6b8; }}
button {{ font: inherit; padding: .4rem .7rem; }}
#error {{ color: #ff8f8f; white-space: pre-wrap; }}
</style>
</head>
<body>
<h1>Portboard</h1>
<p id="error" role="alert"></p>
<main id="targets">Loading…</main>
<script>
const token = {token:?};
const targetList = document.querySelector('#targets');
const errorBox = document.querySelector('#error');
async function api(path, options = {{}}) {{
  options.headers = {{...(options.headers || {{}}), 'X-Portboard-Token': token}};
  const response = await fetch(path, options);
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}}
async function refresh() {{
  try {{
    const data = await api('/api/status');
    errorBox.textContent = '';
    targetList.replaceChildren(...data.targets.map(target => {{
      const row = document.createElement('section'); row.className = 'target';
      const label = document.createElement('strong'); label.textContent = target.label;
      const state = document.createElement('span'); state.className = 'state';
      const endpoints = target.named_endpoints.length
        ? target.named_endpoints.map(endpoint => `${{endpoint.id}} ${{endpoint.url}}`).join(', ')
        : target.endpoints.map(endpoint => `${{endpoint.address}}:${{endpoint.port}}`).join(', ');
      const detail = endpoints ? `${{target.state}} · ${{endpoints}}` : target.state;
      if (target.duplicate) {{
        state.style.color = '#ff8f8f';
        state.textContent = `duplicate! (${{detail}})`;
      }} else {{
        state.textContent = detail;
      }}
      const button = document.createElement('button');
      const running = target.state === 'running';
      button.textContent = running ? 'Stop' : 'Start';
      button.onclick = async () => {{
        button.disabled = true;
        try {{
          await api(`/api/${{running ? 'stop' : 'open'}}/${{target.id}}`, {{method:'POST'}});
          await refresh();
        }} catch (error) {{ errorBox.textContent = error.message; button.disabled = false; }}
      }};
      row.append(label, state, button); return row;
    }}));
  }} catch (error) {{ errorBox.textContent = error.message; }}
}}
refresh(); setInterval(refresh, 2000);
</script>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{is_loopback_bind_address, rotate_log};

    #[test]
    fn only_accepts_loopback_dashboard_addresses() {
        for address in [
            "127.0.0.1:9777",
            "127.0.0.2:9777",
            "localhost:9777",
            "[::1]:9777",
        ] {
            assert!(is_loopback_bind_address(address), "{address}");
        }
        for address in ["0.0.0.0:9777", "192.168.1.10:9777", "192.0.2.1:9777"] {
            assert!(!is_loopback_bind_address(address), "{address}");
        }
    }

    #[test]
    fn rotates_logs_and_prunes_older_runs() {
        let directory = tempfile::tempdir().expect("temporary log directory");
        for stamp in 1..=7_u64 {
            fs::write(
                directory.path().join(format!("probe.{stamp}.log")),
                format!("run {stamp}\n"),
            )
            .expect("seed log");
        }

        let (current, _) = rotate_log(directory.path(), "probe").expect("rotated log");

        // The stable symlink points at the newest timestamped file.
        let link = directory.path().join("probe.log");
        assert_eq!(
            fs::read_link(&link).expect("current log symlink"),
            Path::new(current.file_name().expect("timestamped name"))
        );
        assert_eq!(fs::read_to_string(&link).expect("linked contents"), "");

        // Only LOGS_TO_KEEP timestamped logs remain, and they are the newest.
        let remaining = fs::read_dir(directory.path())
            .expect("log directory")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("probe.") && name != "probe.log")
            })
            .count();
        assert_eq!(remaining, 5);
        assert!(!directory.path().join("probe.1.log").exists());
        assert!(!directory.path().join("probe.2.log").exists());
        assert!(!directory.path().join("probe.3.log").exists());
        assert!(directory.path().join("probe.7.log").exists());
        assert!(current.exists());
    }
}
