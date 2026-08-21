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
    if processes.is_empty() {
        return Ok(Vec::new());
    }
    let roots = processes
        .iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    let parents = read_process_parents()?;
    let owners = parents
        .keys()
        .copied()
        .filter(|pid| descends_from_any(*pid, &roots, &parents))
        .chain(roots.iter().copied())
        .collect::<HashSet<_>>();
    let socket_inodes = owners
        .iter()
        .flat_map(|pid| process_socket_inodes(*pid))
        .collect::<HashSet<_>>();

    let mut endpoints = Vec::new();
    read_tcp_endpoints(
        Path::new("/proc/net/tcp"),
        false,
        &socket_inodes,
        &mut endpoints,
    )?;
    read_tcp_endpoints(
        Path::new("/proc/net/tcp6"),
        true,
        &socket_inodes,
        &mut endpoints,
    )?;
    endpoints.sort();
    endpoints.dedup();
    Ok(endpoints)
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

fn descends_from_any(pid: u32, roots: &HashSet<u32>, parents: &HashMap<u32, u32>) -> bool {
    let mut current = pid;
    for _ in 0..128 {
        if roots.contains(&current) {
            return true;
        }
        let Some(parent) = parents.get(&current).copied() else {
            return false;
        };
        if parent <= 1 || parent == current {
            return roots.contains(&parent);
        }
        current = parent;
    }
    false
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
    owned_inodes: &HashSet<u64>,
    output: &mut Vec<ProcessEndpoint>,
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
        let Ok(inode) = fields[9].parse::<u64>() else {
            continue;
        };
        if !owned_inodes.contains(&inode) {
            continue;
        }
        let Some((address, port)) = parse_local_address(fields[1], ipv6) else {
            continue;
        };
        output.push(ProcessEndpoint {
            protocol: "tcp",
            address,
            port,
        });
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

    use super::{find_process_endpoints, parse_local_address};

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
            pid: std::process::id(),
            argv: Vec::new(),
        }];

        let endpoints = find_process_endpoints(&processes).expect("endpoint scan");

        assert!(endpoints
            .iter()
            .any(|endpoint| endpoint.address == "127.0.0.1" && endpoint.port == port));
    }
}
