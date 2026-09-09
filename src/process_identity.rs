use std::fs;

use serde::Serialize;

/// Linux process identity; numeric PIDs alone are never safe stop handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcessIdentity {
    pub fn read(pid: u32) -> Option<Self> {
        read_stat(pid).map(|(identity, _, _)| identity)
    }

    pub fn is_live(self) -> bool {
        Self::read(self.pid) == Some(self)
    }
}

fn read_stat(pid: u32) -> Option<(ProcessIdentity, u32, u32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    Some((
        ProcessIdentity {
            pid,
            start_time: fields.get(19)?.parse().ok()?,
        },
        fields.get(1)?.parse().ok()?,
        fields.get(2)?.parse().ok()?,
    ))
}

/// Explicit per-launch group ownership. The random inherited token prevents a
/// recycled PGID from authorizing a stop after the original leader is reaped.
#[derive(Clone, Debug)]
pub struct OwnedProcessGroup {
    pub pgid: u32,
    pub token: String,
}

impl OwnedProcessGroup {
    pub fn members(&self) -> anyhow::Result<Vec<ProcessIdentity>> {
        let expected = format!("PORTBOARD_RUN_TOKEN={}", self.token);
        let mut members = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let Some((identity, _, group)) = read_stat(pid) else {
                continue;
            };
            if group != self.pgid {
                continue;
            }
            let Ok(environment) = fs::read(entry.path().join("environ")) else {
                continue;
            };
            if environment
                .split(|byte| *byte == 0)
                .any(|entry| entry == expected.as_bytes())
                && identity.is_live()
            {
                members.push(identity);
            }
        }
        Ok(members)
    }
}

/// Captures descendants while the root identity is still valid. Capturing exact
/// identities before signaling avoids both orphan leakage and group-wide kills.
pub fn process_trees(roots: &[ProcessIdentity]) -> anyhow::Result<Vec<ProcessIdentity>> {
    let mut selected = roots
        .iter()
        .copied()
        .filter(|id| id.is_live())
        .collect::<Vec<_>>();
    selected.sort_by_key(|identity| (identity.pid, identity.start_time));
    selected.dedup();
    let mut candidates = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Some(stat) = read_stat(pid) {
            candidates.push(stat);
        }
    }
    loop {
        let before = selected.len();
        for (identity, parent, _) in &candidates {
            if !selected.contains(identity)
                && selected.iter().any(|id| id.pid == *parent && id.is_live())
                && identity.is_live()
            {
                selected.push(*identity);
            }
        }
        if selected.len() == before {
            break;
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn owned_group_survives_leader_exit_without_claiming_foreign_members() {
        let token = format!("test-run-{}", std::process::id());
        let mut leader = Command::new("sleep")
            .arg("30")
            .env("PORTBOARD_RUN_TOKEN", &token)
            .process_group(0)
            .spawn()
            .unwrap();
        let group = OwnedProcessGroup {
            pgid: leader.id(),
            token: token.clone(),
        };
        let mut follower = Command::new("bash")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .env("PORTBOARD_RUN_TOKEN", &token)
            .process_group(leader.id() as i32)
            .spawn()
            .unwrap();
        let mut foreign = Command::new("sleep")
            .arg("30")
            .process_group(leader.id() as i32)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        leader.kill().unwrap();
        leader.wait().unwrap();
        let members = group.members().unwrap();
        let result = crate::launch_target_stop::stop_process_identities(&members);
        let follower_exited = follower.try_wait().unwrap().is_some();
        let foreign_survived = foreign.try_wait().unwrap().is_none();
        let _ = follower.kill();
        follower.wait().unwrap();
        let _ = foreign.kill();
        foreign.wait().unwrap();
        result.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].pid, follower.id());
        assert!(follower_exited);
        assert!(
            foreign_survived,
            "even a shared group does not authorize killing foreign members"
        );
    }
}
