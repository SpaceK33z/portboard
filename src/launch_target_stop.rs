use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::herdr_workspace::{close_idle_launch_target_tab, launch_target_herdr_tab_id};
use crate::launch_target_config::LaunchTarget;
use crate::launch_target_lock::LaunchTargetLock;
use crate::launch_target_processes::{
    find_launch_target_processes, process_has_portboard_target_identity, LaunchTargetProcess,
};
use crate::process_identity::ProcessIdentity;
use anyhow::{bail, Context, Result};

const KILL_GRACE: Duration = Duration::from_secs(5);
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Blocking, bounded stop. Hold the launch lock through confirmed shutdown and
/// optional safe tab cleanup so open/ensure cannot race a partially stopped run.
pub fn stop_launch_target_and_close_herdr_tab(
    worktree_root: &Path,
    target: &LaunchTarget,
    workspace_id: &str,
) -> Result<Vec<u32>> {
    stop_locked(worktree_root, target, Some(workspace_id), None)
}

pub fn stop_launch_target(worktree_root: &Path, target: &LaunchTarget) -> Result<Vec<u32>> {
    stop_locked(worktree_root, target, None, None)
}

pub fn stop_launch_target_with_owned_group(
    worktree_root: &Path,
    target: &LaunchTarget,
    group: &crate::process_identity::OwnedProcessGroup,
) -> Result<Vec<u32>> {
    stop_locked(worktree_root, target, None, Some(group))
}

fn stop_locked(
    worktree_root: &Path,
    target: &LaunchTarget,
    workspace: Option<&str>,
    group: Option<&crate::process_identity::OwnedProcessGroup>,
) -> Result<Vec<u32>> {
    let mut lock = LaunchTargetLock::acquire(worktree_root, target)?;
    let mut processes = find_launch_target_processes(worktree_root, target)?;
    let reservation = lock.active_reservation_identity()?;
    if let Some(identity) = reservation {
        if !processes
            .iter()
            .any(|process| process.identity() == identity)
        {
            processes.push(LaunchTargetProcess {
                metadata_members: Vec::new(),
                pid: identity.pid,
                start_time: identity.start_time,
                argv: Vec::new(),
            });
        }
    }
    let owned = processes
        .iter()
        .filter(|p| {
            Some(p.identity()) == reservation
                || process_has_portboard_target_identity(p, target.id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    let tab = match workspace {
        Some(workspace) if !owned.is_empty() => launch_target_herdr_tab_id(workspace, &owned)?,
        _ => None,
    };
    let mut identities = processes
        .iter()
        .map(LaunchTargetProcess::identity)
        .collect::<Vec<_>>();
    if let Some(group) = group {
        identities.extend(group.members()?);
    }
    let identities = crate::process_identity::process_trees(&identities)?;
    let pids = stop_process_identities(&identities)?;
    if let Some(group) = group {
        if !group.members()?.is_empty() {
            bail!("Portboard owned process group still has live members after stop");
        }
    }
    if !find_launch_target_processes(worktree_root, target)?.is_empty() {
        bail!("Portboard launch target still has live processes after stop");
    }
    lock.clear()?;
    if let (Some(workspace), Some(tab)) = (workspace, tab) {
        close_idle_launch_target_tab(workspace, &tab, target)?;
    }
    Ok(pids)
}

/// Common stop seam for reservations, matching runs, and explicitly captured
/// dashboard group members. Never signals a numeric process group. Call from a
/// worker thread, not the dashboard accept loop; completion confirms exit.
pub fn stop_process_identities(identities: &[ProcessIdentity]) -> Result<Vec<u32>> {
    let mut signaled = Vec::new();
    for identity in identities {
        if !identity.is_live() {
            continue;
        }
        let pid = i32::try_from(identity.pid).context("invalid process pid")?;
        let pidfd = match PidFd::open(pid) {
            Ok(fd) => fd,
            Err(_) if !identity.is_live() => continue,
            Err(error) => return Err(error),
        };
        let process = SignaledProcess {
            identity: *identity,
            pidfd,
        };
        process.send(libc::SIGTERM)?;
        signaled.push(process);
    }
    wait_for_exit(&signaled, KILL_GRACE);
    for process in &signaled {
        process.send(libc::SIGKILL)?;
    }
    wait_for_exit(&signaled, KILL_GRACE);
    if signaled.iter().any(|process| process.identity.is_live()) {
        bail!("Portboard could not confirm process exit after SIGKILL");
    }
    Ok(signaled
        .iter()
        .map(|process| process.identity.pid)
        .collect())
}

struct SignaledProcess {
    identity: ProcessIdentity,
    pidfd: Option<PidFd>,
}

/// Injected read/signal functions allow safe PID-reuse regression tests without
/// ever signaling real unrelated processes.
fn signal_if_current(
    identity: ProcessIdentity,
    signal: i32,
    read: impl FnOnce(u32) -> Option<ProcessIdentity>,
    send: impl FnOnce(u32, i32) -> io::Result<()>,
) -> io::Result<()> {
    if read(identity.pid) != Some(identity) {
        return Ok(());
    }
    match send(identity.pid, signal) {
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        result => result,
    }
}

impl SignaledProcess {
    fn send(&self, signal: i32) -> Result<()> {
        signal_if_current(
            self.identity,
            signal,
            ProcessIdentity::read,
            |pid, signal| {
                if let Some(fd) = &self.pidfd {
                    return fd.send_signal(signal);
                }
                // SAFETY: positive PID, exact start time checked immediately above.
                if unsafe { libc::kill(pid as i32, signal) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            },
        )
        .with_context(|| format!("Portboard could not signal pid {}", self.identity.pid))
    }
}

fn wait_for_exit(processes: &[SignaledProcess], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while processes.iter().any(|process| process.identity.is_live()) && Instant::now() < deadline {
        thread::sleep(KILL_POLL_INTERVAL);
    }
}

struct PidFd(i32);

impl PidFd {
    fn open(pid: i32) -> Result<Option<Self>> {
        // SAFETY: pidfd_open takes only integer values and returns a new file
        // descriptor owned by the caller.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor >= 0 {
            return Ok(Some(Self(descriptor as i32)));
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Ok(None);
        }
        Err(error).with_context(|| format!("Portboard could not open a pidfd for pid {pid}"))
    }

    fn send_signal(&self, signal: i32) -> io::Result<()> {
        // SAFETY: pidfd_send_signal reads no siginfo when passed null and uses
        // the live descriptor owned by this guard.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0,
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for PidFd {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the descriptor.
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::launch_target_config::parse_launch_target_config;

    use super::stop_launch_target;

    #[test]
    fn fallback_escalation_does_not_signal_reused_pid() {
        let original = crate::process_identity::ProcessIdentity {
            pid: 123,
            start_time: 10,
        };
        let replacement = crate::process_identity::ProcessIdentity {
            pid: 123,
            start_time: 20,
        };
        let calls = std::cell::Cell::new(0);
        super::signal_if_current(
            original,
            libc::SIGKILL,
            |_| Some(replacement),
            |_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 0);
        super::signal_if_current(
            original,
            libc::SIGKILL,
            |_| Some(original),
            |_, signal| {
                assert_eq!(signal, libc::SIGKILL);
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn stops_a_matching_process() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-stop-test-{}", std::process::id());
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
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test process");

        let stopped =
            stop_launch_target(temporary.path(), &config.launch_targets[0]).expect("stop target");
        assert!(stopped.contains(&child.id()));

        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(observed) = child.try_wait().expect("child status") {
                break observed;
            }
            assert!(Instant::now() < deadline, "process did not stop");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(15));
    }

    #[test]
    fn escalates_to_sigkill_when_sigterm_is_ignored() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let marker = format!("portboard-stop-kill-test-{}", std::process::id());
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
        // An ignored SIGTERM disposition survives exec, so sleep never sees
        // the graceful stop and Portboard must escalate to SIGKILL.
        let mut child = Command::new("bash")
            .args(["-c", &format!("trap '' TERM; exec -a {marker} sleep 30")])
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test process");

        let stopped =
            stop_launch_target(temporary.path(), &config.launch_targets[0]).expect("stop target");
        assert!(stopped.contains(&child.id()));

        // The escalation grace is five seconds, so allow extra headroom for
        // slow machines before declaring the process unkillable.
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(observed) = child.try_wait().expect("child status") {
                break observed;
            }
            assert!(Instant::now() < deadline, "process ignored SIGKILL");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.signal(), Some(9));
    }
}
