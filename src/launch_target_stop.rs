use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::find_launch_target_processes;

/// Sends SIGTERM to every live process that currently matches a launch target.
///
/// Processes are discovered again immediately before signaling so a stale panel
/// selection cannot stop an unrelated PID.
pub fn stop_launch_target(worktree_root: &Path, target: &LaunchTarget) -> Result<Vec<u32>> {
    let processes = find_launch_target_processes(worktree_root, target)?;
    if processes.is_empty() {
        bail!(
            "Portboard launch target `{}` is not running",
            target.id.as_str()
        );
    }

    let mut stopped = Vec::with_capacity(processes.len());
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

        let result = match pidfd {
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
            Ok(()) => stopped.push(process.pid),
            // A natural exit already satisfies the requested stop operation.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Portboard could not stop pid {}", process.pid));
            }
        }
    }
    Ok(stopped)
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
        // SAFETY: pidfd_send_signal reads no siginfo when passed null and uses
        // the live descriptor owned by this guard.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0,
                libc::SIGTERM,
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
        loop {
            if child.try_wait().expect("child status").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "process did not stop");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
