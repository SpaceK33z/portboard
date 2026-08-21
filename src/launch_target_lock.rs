use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::launch_target_config::LaunchTarget;
use crate::state_paths::worktree_state_directory;

/// Holds the cross-process launch reservation for one worktree target.
pub struct LaunchTargetLock {
    file: File,
}

impl LaunchTargetLock {
    /// Serializes the scan-and-start section used for duplicate prevention.
    pub fn acquire(worktree_root: &Path, target: &LaunchTarget) -> Result<Self> {
        let directory = lock_directory(worktree_root)?;
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "Portboard could not create launch lock directory {}",
                directory.display()
            )
        })?;
        let path = directory.join(format!("{}.lock", target.id.as_str()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("Portboard could not open {}", path.display()))?;
        // SAFETY: flock only uses the live file descriptor for this call. The
        // File stored in the guard keeps the lock until the guard is dropped.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("Portboard could not lock {}", path.display()));
        }
        Ok(Self { file })
    }

    /// Returns true while a previously launched root process has the same PID
    /// and Linux start time. Stale reservations are removed while locked.
    pub fn has_active_reservation(&mut self) -> Result<bool> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        self.file.read_to_string(&mut contents)?;
        let reservation = contents.split_once(' ').and_then(|(pid, start_time)| {
            Some((
                pid.parse::<u32>().ok()?,
                start_time.trim().parse::<u64>().ok()?,
            ))
        });
        let active = reservation.is_some_and(|(pid, expected_start_time)| {
            process_start_time(pid) == Some(expected_start_time)
        });
        if !active && !contents.is_empty() {
            self.clear()?;
        }
        Ok(active)
    }

    /// Ties the in-flight reservation to a concrete launched process identity.
    /// Returns false when a short-lived process exited before it could be read.
    pub fn reserve_process(&mut self, pid: u32) -> Result<bool> {
        let Some(start_time) = process_start_time(pid) else {
            self.clear()?;
            return Ok(false);
        };
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        writeln!(self.file, "{pid} {start_time}")?;
        self.file.sync_data()?;
        Ok(true)
    }

    /// Clears a reservation after a matching run is visible or confirmed gone.
    pub fn clear(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_data()?;
        Ok(())
    }
}

fn process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn lock_directory(worktree_root: &Path) -> Result<std::path::PathBuf> {
    Ok(worktree_state_directory(worktree_root)?.join("locks"))
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use crate::launch_target_config::parse_launch_target_config;

    use super::LaunchTargetLock;

    #[test]
    fn reservation_tracks_process_identity_and_clears_after_exit() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let state = tempfile::tempdir().expect("temporary state");
        // Tests call the state-path helper through this process-wide override;
        // this module has one test and does not run alongside integration tests
        // in the same test process.
        std::env::set_var("PORTBOARD_STATE_DIR", state.path());
        let config = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "test"
label = "Test"
argv = ["sleep", "30"]
"#,
        )
        .expect("config");
        let target = &config.launch_targets[0];
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .expect("child");
        let mut lock = LaunchTargetLock::acquire(temporary.path(), target).expect("lock");
        assert!(lock.reserve_process(child.id()).expect("reserve"));
        drop(lock);

        let mut lock = LaunchTargetLock::acquire(temporary.path(), target).expect("second lock");
        assert!(lock.has_active_reservation().expect("active"));
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert!(!lock.has_active_reservation().expect("stale"));
        std::env::remove_var("PORTBOARD_STATE_DIR");
    }
}
