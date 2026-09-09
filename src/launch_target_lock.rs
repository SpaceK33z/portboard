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
        Ok(self.active_reservation()?.is_some())
    }

    /// Returns the reserved launcher PID while its Linux process identity matches.
    pub fn active_reservation_pid(&mut self) -> Result<Option<u32>> {
        Ok(self.active_reservation()?.map(|(pid, _)| pid))
    }

    pub fn active_reservation_identity(
        &mut self,
    ) -> Result<Option<crate::process_identity::ProcessIdentity>> {
        Ok(self
            .active_reservation()?
            .map(|(pid, start_time)| crate::process_identity::ProcessIdentity { pid, start_time }))
    }

    /// Retains the lock and reservation until the exact launcher has exited.
    pub fn cancel_active_reservation(&mut self) -> Result<Option<u32>> {
        let Some(identity) = self.active_reservation_identity()? else {
            return Ok(None);
        };
        let identities = crate::process_identity::process_trees(&[identity])?;
        crate::launch_target_stop::stop_process_identities(&identities)?;
        self.clear()?;
        Ok(Some(identity.pid))
    }

    fn active_reservation(&mut self) -> Result<Option<(u32, u64)>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        self.file.read_to_string(&mut contents)?;
        let reservation = contents.split_once(' ').and_then(|(pid, start_time)| {
            Some((
                pid.parse::<u32>().ok()?,
                start_time.trim().parse::<u64>().ok()?,
            ))
        });
        let active = reservation.filter(|(pid, expected_start_time)| {
            process_start_time(*pid) == Some(*expected_start_time)
        });
        if active.is_none() && !contents.is_empty() {
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

    /// Clears a reservation only after its launcher and stopped run are confirmed gone.
    pub fn clear(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_data()?;
        Ok(())
    }
}

fn process_start_time(pid: u32) -> Option<u64> {
    crate::process_identity::ProcessIdentity::read(pid).map(|identity| identity.start_time)
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
    fn cancellation_serializes_open_and_keeps_reservation_during_grace() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let config = parse_launch_target_config(
            r#"version = 1
[[launch_targets]]
id = "race"
label = "Race"
argv = ["never-visible"]
"#,
        )
        .unwrap();
        let mut child = Command::new("bash")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut lock = LaunchTargetLock::acquire(root.path(), &config.launch_targets[0]).unwrap();
        lock.reserve_process(child.id()).unwrap();
        let path = super::lock_directory(root.path())
            .unwrap()
            .join("race.lock");
        let worker = std::thread::spawn(move || lock.cancel_active_reservation());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let contender = std::fs::File::open(&path).unwrap();
        let locked =
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
        let reserved = !std::fs::read_to_string(path).unwrap().is_empty();
        worker.join().unwrap().unwrap();
        child.wait().unwrap();
        assert!(
            locked,
            "open must not enter its scan/start critical section during stop"
        );
        assert!(reserved, "reservation must survive the TERM grace window");
    }

    #[test]
    fn cancellation_waits_for_term_ignoring_reservation() {
        let temporary = tempfile::tempdir().unwrap();
        let config = parse_launch_target_config(
            r#"version = 1
[[launch_targets]]
id = "reserved"
label = "Reserved"
argv = ["never-visible"]
"#,
        )
        .unwrap();
        let mut child = Command::new("bash")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .current_dir(temporary.path())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut lock =
            LaunchTargetLock::acquire(temporary.path(), &config.launch_targets[0]).unwrap();
        lock.reserve_process(child.id()).unwrap();
        lock.cancel_active_reservation().unwrap();
        let exited = child.try_wait().unwrap().is_some();
        if !exited {
            child.kill().unwrap();
        }
        child.wait().unwrap();
        assert!(
            exited,
            "reservation must not be released while launcher survives"
        );
    }

    #[test]
    fn reservation_tracks_process_identity_and_clears_after_exit() {
        let temporary = tempfile::tempdir().expect("temporary worktree");
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
        assert_eq!(
            lock.cancel_active_reservation().expect("cancel"),
            Some(child.id())
        );
        child.wait().expect("wait");
        assert!(!lock.has_active_reservation().expect("cancelled"));
    }
}
