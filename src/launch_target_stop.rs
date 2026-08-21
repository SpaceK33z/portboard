use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::herdr_workspace::{close_launch_target_herdr_tab, launch_target_herdr_tab_id};
use crate::launch_target_config::LaunchTarget;
use crate::launch_target_lock::LaunchTargetLock;
use crate::launch_target_processes::{
    find_launch_target_processes, process_has_portboard_target_identity,
};

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long SIGTERM survivors get to exit before Portboard escalates to
/// SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// How often the post-SIGTERM wait rechecks whether the processes exited.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Sends SIGTERM to every live process that currently matches a launch target.
///
/// Processes are discovered again immediately before signaling so a stale panel
/// selection cannot stop an unrelated PID.
pub fn stop_launch_target_and_close_herdr_tab(
    worktree_root: &Path,
    target: &LaunchTarget,
    workspace_id: &str,
) -> Result<Vec<u32>> {
    let processes = find_launch_target_processes(worktree_root, target)?;
    let tab_candidate_processes = if processes.is_empty() {
        let mut launch_lock = LaunchTargetLock::acquire(worktree_root, target)?;
        launch_lock
            .active_reservation_pid()?
            .map(|pid| {
                vec![crate::launch_target_processes::LaunchTargetProcess {
                    pid,
                    argv: Vec::new(),
                }]
            })
            .unwrap_or_default()
    } else {
        processes.clone()
    };
    let owned_processes = tab_candidate_processes
        .iter()
        .filter(|process| process_has_portboard_target_identity(process, target.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let herdr_tab_id = if owned_processes.is_empty() {
        None
    } else {
        launch_target_herdr_tab_id(workspace_id, &owned_processes)?
    };
    let stopped_pids = stop_launch_target(worktree_root, target)?;
    if let Some(tab_id) = herdr_tab_id {
        if !processes.is_empty() {
            wait_for_launch_target_to_stop(worktree_root, target, STOP_TIMEOUT)?;
        }
        close_launch_target_herdr_tab(&tab_id)?;
    }
    Ok(stopped_pids)
}

/// Sends SIGTERM to each matching process without requiring a Herdr workspace.
pub fn stop_launch_target(worktree_root: &Path, target: &LaunchTarget) -> Result<Vec<u32>> {
    let processes = find_launch_target_processes(worktree_root, target)?;
    if processes.is_empty() {
        let mut launch_lock = LaunchTargetLock::acquire(worktree_root, target)?;
        return Ok(launch_lock
            .cancel_active_reservation()?
            .into_iter()
            .collect());
    }

    let mut signaled = Vec::with_capacity(processes.len());
    for process in processes {
        let pid = i32::try_from(process.pid)
            .with_context(|| format!("Portboard cannot signal pid {}", process.pid))?;
        let pidfd = PidFd::open(pid)?;

        // Validate again after opening the pidfd. If the original process exited
        // before pidfd_open and its numeric PID was reused, the descriptor now
        // refers to that replacement; never signal it unless it still matches.
        let still_matches = find_launch_target_processes(worktree_root, target)?
            .iter()
            .any(|candidate| candidate.pid == process.pid);
        if !still_matches {
            continue;
        }

        let result = match &pidfd {
            Some(pidfd) => pidfd.send_sigterm(),
            None => {
                // Old kernels without pidfds get the narrowest available
                // fallback immediately after the second revalidation.
                // SAFETY: libc::kill does not retain pointers and `pid` is a
                // validated, positive process id obtained from /proc.
                let result = unsafe { libc::kill(pid, libc::SIGTERM) };
                if result == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        };
        match result {
            Ok(()) => signaled.push(SignaledProcess {
                pid: process.pid,
                pidfd,
            }),
            // A natural exit already satisfies the requested stop operation.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Portboard could not stop pid {}", process.pid));
            }
        }
    }
    wait_for_sigterm_exit(&signaled);
    kill_survivors(&signaled)?;
    Ok(signaled.iter().map(|process| process.pid).collect())
}

/// One process that received SIGTERM together with its pidfd, when the kernel
/// supports them, so escalation cannot hit a reused numeric PID.
struct SignaledProcess {
    pid: u32,
    pidfd: Option<PidFd>,
}

/// Waits up to [`KILL_GRACE`] for every signaled process to exit on its own.
fn wait_for_sigterm_exit(signaled: &[SignaledProcess]) {
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        if signaled.iter().all(|process| !process_exists(process.pid)) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(KILL_POLL_INTERVAL);
    }
}

/// Sends SIGKILL to every signaled process that ignored SIGTERM.
fn kill_survivors(signaled: &[SignaledProcess]) -> Result<()> {
    for process in signaled
        .iter()
        .filter(|process| process_exists(process.pid))
    {
        let raw_pid = i32::try_from(process.pid)
            .with_context(|| format!("Portboard cannot signal pid {}", process.pid))?;
        let result = match &process.pidfd {
            Some(pidfd) => pidfd.send_sigkill(),
            None => {
                // Old kernels without pidfds get the narrowest available
                // fallback after the same revalidation-free grace window.
                // SAFETY: libc::kill does not retain pointers and `raw_pid` is
                // a validated, positive process id obtained from /proc.
                let result = unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                if result == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        };
        match result {
            // The process exited between the liveness check and the signal.
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Portboard could not kill pid {}", process.pid));
            }
        }
    }
    Ok(())
}

/// Returns whether a numeric PID still names a live or zombie process.
fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_for_launch_target_to_stop(
    worktree_root: &Path,
    target: &LaunchTarget,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if find_launch_target_processes(worktree_root, target)?.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "Portboard launch target `{}` did not stop within {} seconds",
                target.id.as_str(),
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
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

    fn send_sigterm(&self) -> io::Result<()> {
        self.send_signal(libc::SIGTERM)
    }

    fn send_sigkill(&self) -> io::Result<()> {
        self.send_signal(libc::SIGKILL)
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
