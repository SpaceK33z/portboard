use std::collections::{HashMap, HashSet};

use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::launch_target_processes::LaunchTargetProcess;

/// A listening TCP endpoint owned by a launch-target process or descendant.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProcessEndpoint {
    pub protocol: &'static str,
    pub address: String,
    pub port: u16,
}

/// Discovers Linux TCP listeners owned by matching processes and their child
/// process trees.
pub fn find_process_endpoints(processes: &[LaunchTargetProcess]) -> Result<Vec<ProcessEndpoint>> {
    let mut endpoints = find_process_endpoints_by_process(processes)?
        .into_values()
        .flatten()
        .collect::<Vec<_>>();
    endpoints.sort();
    endpoints.dedup();
    Ok(endpoints)
}

/// Discovers Linux TCP listeners and attributes every endpoint to the root
/// process whose tree (the process itself or one of its descendants) owns the
/// socket. Roots with no listeners are absent from the result.
pub fn find_process_endpoints_by_process(
    processes: &[LaunchTargetProcess],
) -> Result<HashMap<u32, Vec<ProcessEndpoint>>> {
    let mut grouped: HashMap<u32, Vec<ProcessEndpoint>> = HashMap::new();
    if processes.is_empty() {
        return Ok(grouped);
    }
    let roots = processes
        .iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    let parents = read_process_parents()?;
    // Inherited descriptors may belong to several roots. Retain every owner
    // instead of letting hash iteration order select a different winner.
    let mut socket_owners: HashMap<u64, HashSet<u32>> = HashMap::new();
    for pid in parents.keys().copied().chain(roots.iter().copied()) {
        let Some(root) = owning_root(pid, &roots, &parents) else {
            continue;
        };
        for inode in process_socket_inodes(pid) {
            socket_owners.entry(inode).or_default().insert(root);
        }
    }
    read_tcp_endpoints(
        Path::new("/proc/net/tcp"),
        false,
        &socket_owners,
        &mut grouped,
    )?;
    read_tcp_endpoints(
        Path::new("/proc/net/tcp6"),
        true,
        &socket_owners,
        &mut grouped,
    )?;
    for endpoints in grouped.values_mut() {
        endpoints.sort();
        endpoints.dedup();
    }
    Ok(grouped)
}

/// Walks the parent chain from `pid` and returns the root process whose tree
/// contains it, or `None` when the chain leaves every root.
fn owning_root(pid: u32, roots: &HashSet<u32>, parents: &HashMap<u32, u32>) -> Option<u32> {
    let mut current = pid;
    for _ in 0..128 {
        if roots.contains(&current) {
            return Some(current);
        }
        let parent = parents.get(&current).copied()?;
        if parent <= 1 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}

fn read_process_parents() -> Result<HashMap<u32, u32>> {
    let mut parents = HashMap::new();
    for entry in fs::read_dir("/proc").context("Portboard endpoint scan could not read /proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        if let Some(parent) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        }) {
            parents.insert(pid, parent);
        }
    }
    Ok(parents)
}

fn process_socket_inodes(pid: u32) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter_map(|target| {
            let text = target.to_str()?;
            text.strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse::<u64>()
                .ok()
        })
        .collect()
}

fn read_tcp_endpoints(
    path: &Path,
    ipv6: bool,
    socket_owners: &HashMap<u64, HashSet<u32>>,
    output: &mut HashMap<u32, Vec<ProcessEndpoint>>,
) -> Result<()> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Portboard endpoint scan could not read {}", path.display())
            });
        }
    };
    for line in contents.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 10 || fields[3] != "0A" {
            continue;
        }
        let Some(inode) = fields[9].parse::<u64>().ok() else {
            continue;
        };
        let Some(roots) = socket_owners.get(&inode) else {
            continue;
        };
        let Some((address, port)) = parse_local_address(fields[1], ipv6) else {
            continue;
        };
        for &root in roots {
            output.entry(root).or_default().push(ProcessEndpoint {
                protocol: "tcp",
                address: address.clone(),
                port,
            });
        }
    }
    Ok(())
}

fn parse_local_address(value: &str, ipv6: bool) -> Option<(String, u16)> {
    let (address, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if !ipv6 {
        let address = u32::from_str_radix(address, 16).ok()?;
        return Some((Ipv4Addr::from(address.to_le_bytes()).to_string(), port));
    }
    if address.len() != 32 {
        return None;
    }
    let mut octets = [0_u8; 16];
    for (index, chunk) in address.as_bytes().chunks_exact(8).enumerate() {
        let chunk = std::str::from_utf8(chunk).ok()?;
        let word = u32::from_str_radix(chunk, 16).ok()?;
        octets[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    Some((Ipv6Addr::from(octets).to_string(), port))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use crate::launch_target_processes::LaunchTargetProcess;

    use super::{find_process_endpoints, find_process_endpoints_by_process, parse_local_address};

    #[test]
    fn parses_proc_tcp_addresses() {
        assert_eq!(
            parse_local_address("0100007F:45E8", false),
            Some(("127.0.0.1".to_string(), 17_896))
        );
        assert_eq!(
            parse_local_address("00000000000000000000000001000000:1F90", true),
            Some(("::1".to_string(), 8080))
        );
    }

    #[test]
    fn finds_listener_owned_by_a_process() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let port = listener.local_addr().expect("listener address").port();
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let endpoints = find_process_endpoints(&processes).expect("endpoint scan");

        assert!(endpoints
            .iter()
            .any(|endpoint| endpoint.address == "127.0.0.1" && endpoint.port == port));
    }

    #[test]
    fn attributes_listeners_to_their_owning_root() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let port = listener.local_addr().expect("listener address").port();
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let grouped = find_process_endpoints_by_process(&processes).expect("grouped endpoints");

        let owned = grouped
            .get(&std::process::id())
            .expect("owning root present");
        assert!(owned
            .iter()
            .any(|endpoint| endpoint.address == "127.0.0.1" && endpoint.port == port));
    }
    #[test]
    fn inherited_listener_is_attributed_to_every_owning_root() {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fd = listener.as_raw_fd();
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pids = [std::process::id(), child.id()];
        let processes = pids
            .iter()
            .map(|&pid| LaunchTargetProcess {
                metadata_members: Vec::new(),
                pid,
                start_time: crate::process_identity::ProcessIdentity::read(pid)
                    .unwrap()
                    .start_time,
                argv: vec![],
            })
            .collect::<Vec<_>>();
        let result = find_process_endpoints_by_process(&processes);
        child.kill().unwrap();
        child.wait().unwrap();
        let grouped = result.unwrap();
        for pid in pids {
            assert!(
                grouped
                    .get(&pid)
                    .is_some_and(|e| e.iter().any(|e| e.port == port)),
                "missing owner {pid}: {grouped:?}"
            );
        }
    }
}
