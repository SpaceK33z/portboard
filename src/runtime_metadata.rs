use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::launch_target_config::LaunchTarget;
use crate::launch_target_processes::LaunchTargetProcess;

/// Browser-ready named endpoint published by a running launch target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEndpoint {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub primary: bool,
    /// Optional project-owned lifecycle for a supervised endpoint, such as the
    /// `starting`, `running`, or `failed` state of an optional model server.
    /// Portboard carries it through JSON output but does not interpret it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Versioned runtime metadata published by a project-owned process supervisor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchTargetRuntimeMetadata {
    pub version: u32,
    pub target_id: String,
    pub pid: u32,
    pub started_at: String,
    pub endpoints: Vec<RuntimeEndpoint>,
}

impl LaunchTargetRuntimeMetadata {
    /// Returns the one endpoint designated as the default browser destination.
    pub fn primary_endpoint(&self) -> Option<&RuntimeEndpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.primary)
    }
}

/// Loads runtime metadata only when it belongs to the target's current live process.
pub fn load_launch_target_runtime_metadata(
    worktree_root: &Path,
    target: &LaunchTarget,
    processes: &[LaunchTargetProcess],
) -> Result<Option<LaunchTargetRuntimeMetadata>> {
    if processes.is_empty() {
        return Ok(None);
    }
    let Some(runtime_file) = &target.runtime_file else {
        return Ok(None);
    };
    let path = worktree_root.join(runtime_file);
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context("Portboard runtime metadata path cannot be resolved")
        }
    };
    let root = worktree_root.canonicalize()?;
    if !path.starts_with(&root) {
        bail!("Portboard runtime metadata must remain inside the worktree");
    }
    let contents = match read_runtime_file(&path) {
        Ok(contents) => contents,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Portboard runtime metadata could not read {}",
                    path.display()
                )
            });
        }
    };
    let metadata: LaunchTargetRuntimeMetadata =
        serde_json::from_str(&contents).with_context(|| {
            format!(
                "Portboard runtime metadata is invalid at {}",
                path.display()
            )
        })?;
    validate_runtime_metadata(target, &metadata, &path)?;
    if !processes.iter().any(|process| {
        process.identity().is_live()
            && (process.pid == metadata.pid
                || process
                    .metadata_members
                    .iter()
                    .any(|identity| identity.pid == metadata.pid && identity.is_live()))
    }) {
        return Ok(None);
    }
    Ok(Some(metadata))
}

fn read_runtime_file(path: &Path) -> Result<String> {
    if !fs::metadata(path)?.is_file() {
        bail!("Portboard runtime metadata must be a regular file");
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("Portboard runtime metadata must be a regular file");
    }
    let mut contents = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut contents)?;
    if contents.len() > 1024 * 1024 {
        bail!("Portboard runtime metadata exceeds 1 MiB");
    }
    Ok(contents)
}

fn validate_runtime_metadata(
    target: &LaunchTarget,
    metadata: &LaunchTargetRuntimeMetadata,
    path: &Path,
) -> Result<()> {
    if metadata.version != 1 {
        bail!(
            "Portboard runtime metadata version {} is unsupported at {}; expected version 1",
            metadata.version,
            path.display()
        );
    }
    if metadata.target_id != target.id.as_str() {
        bail!(
            "Portboard runtime metadata at {} belongs to target `{}`, expected `{}`",
            path.display(),
            metadata.target_id,
            target.id.as_str()
        );
    }
    if metadata.started_at.trim().is_empty() {
        bail!(
            "Portboard runtime metadata at {} has an empty startedAt",
            path.display()
        );
    }

    let mut endpoint_ids = HashSet::new();
    let mut primary_count = 0;
    for endpoint in &metadata.endpoints {
        if endpoint.id.is_empty()
            || !endpoint.id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            bail!(
                "Portboard runtime endpoint id `{}` at {} is invalid",
                endpoint.id,
                path.display()
            );
        }
        if !endpoint_ids.insert(endpoint.id.as_str()) {
            bail!(
                "Portboard runtime metadata at {} has duplicate endpoint id `{}`",
                path.display(),
                endpoint.id
            );
        }
        let parsed_url =
            crate::browser_url::validate_browser_url(&endpoint.url).with_context(|| {
                format!(
                    "Portboard runtime endpoint `{}` at {} has an invalid URL",
                    endpoint.id,
                    path.display()
                )
            })?;
        if !matches!(parsed_url.scheme(), "http" | "https") || parsed_url.host_str().is_none() {
            bail!(
                "Portboard runtime endpoint `{}` at {} needs an HTTP or HTTPS URL",
                endpoint.id,
                path.display()
            );
        }
        if let Some(status) = &endpoint.status {
            if status.trim().is_empty() || status.chars().any(char::is_control) {
                bail!(
                    "Portboard runtime endpoint `{}` at {} has an empty status",
                    endpoint.id,
                    path.display()
                );
            }
        }
        primary_count += usize::from(endpoint.primary);
    }
    if primary_count > 1 {
        bail!(
            "Portboard runtime metadata at {} has more than one primary endpoint",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::launch_target_config::parse_launch_target_config;
    use crate::launch_target_processes::LaunchTargetProcess;

    use super::load_launch_target_runtime_metadata;

    #[test]
    fn stale_malformed_files_are_ignored_and_live_unsafe_files_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = parse_launch_target_config(
            r#"version = 1
[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["probe"]
runtime_file = "runtime.json"
"#,
        )
        .unwrap();
        let target = &config.launch_targets[0];
        fs::write(root.path().join("runtime.json"), "not json").unwrap();
        assert!(
            load_launch_target_runtime_metadata(root.path(), target, &[])
                .unwrap()
                .is_none()
        );
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: 0,
            argv: vec![],
        }];
        let valid = serde_json::json!({"version":1,"targetId":"probe","pid":std::process::id(),"startedAt":"now","endpoints":[{"id":"web","url":"http://localhost:3000/","primary":true}]});
        fs::write(outside.path().join("runtime.json"), valid.to_string()).unwrap();
        fs::remove_file(root.path().join("runtime.json")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("runtime.json"),
            root.path().join("runtime.json"),
        )
        .unwrap();
        assert!(load_launch_target_runtime_metadata(root.path(), target, &processes).is_err());
        fs::remove_file(root.path().join("runtime.json")).unwrap();
        let mut malicious = valid;
        malicious["endpoints"][0]["url"] =
            serde_json::json!("http://localhost:3000/\x07\x1b]52;c;AAAA\x07");
        fs::write(root.path().join("runtime.json"), malicious.to_string()).unwrap();
        assert!(load_launch_target_runtime_metadata(root.path(), target, &processes).is_err());
        fs::remove_file(root.path().join("runtime.json")).unwrap();
        // O_NONBLOCK plus fstat must reject FIFOs without waiting for a writer.
        let fifo = std::ffi::CString::new(
            root.path()
                .join("runtime.json")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(load_launch_target_runtime_metadata(root.path(), target, &processes).is_err());
        fs::remove_file(root.path().join("runtime.json")).unwrap();
        fs::create_dir(root.path().join("runtime.json")).unwrap();
        assert!(load_launch_target_runtime_metadata(root.path(), target, &processes).is_err());
    }

    #[test]
    fn loads_named_endpoints_for_the_matching_live_process() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        fs::create_dir(worktree.path().join("logs")).expect("logs directory");
        fs::write(
            worktree.path().join("logs/dev-instance.json"),
            format!(
                r#"{{
  "version": 1,
  "targetId": "dev-full",
  "pid": {},
  "startedAt": "2026-08-21T12:37:22.695Z",
  "endpoints": [
    {{"id": "web", "url": "http://localhost:14715", "primary": true}},
    {{"id": "api", "url": "http://localhost:11100", "primary": false}}
  ]
}}"#,
                std::process::id()
            ),
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["pnpm", "dev:full"]
process_match = ["scripts/dev-full.mjs"]
runtime_file = "logs/dev-instance.json"
"#,
        )
        .expect("manifest");
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let metadata = load_launch_target_runtime_metadata(
            worktree.path(),
            &config.launch_targets[0],
            &processes,
        )
        .expect("runtime metadata")
        .expect("matching metadata");

        assert_eq!(metadata.primary_endpoint().expect("primary").id, "web");
        assert_eq!(
            metadata.primary_endpoint().expect("primary").url,
            "http://localhost:14715"
        );
    }

    #[test]
    fn ignores_metadata_owned_by_a_stale_process() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        fs::write(
            worktree.path().join("runtime.json"),
            r#"{
  "version": 1,
  "targetId": "dev-full",
  "pid": 999999,
  "startedAt": "2026-08-21T12:37:22.695Z",
  "endpoints": [{"id": "web", "url": "http://localhost:3001", "primary": true}]
}"#,
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["dev-full"]
runtime_file = "runtime.json"
"#,
        )
        .expect("manifest");
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let metadata = load_launch_target_runtime_metadata(
            worktree.path(),
            &config.launch_targets[0],
            &processes,
        )
        .expect("runtime metadata");

        assert!(metadata.is_none());
    }

    #[test]
    fn accepts_optional_endpoint_status() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        fs::create_dir(worktree.path().join("logs")).expect("logs directory");
        fs::write(
            worktree.path().join("logs/dev-instance.json"),
            format!(
                r#"{{
  "version": 1,
  "targetId": "dev-full",
  "pid": {},
  "startedAt": "2026-08-21T12:37:22.695Z",
  "endpoints": [
    {{"id": "web", "url": "http://localhost:3001", "primary": true}},
    {{"id": "model", "url": "http://127.0.0.1:8200", "primary": false, "status": "running"}}
  ]
}}"#,
                std::process::id()
            ),
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["pnpm", "dev:full"]
process_match = ["scripts/dev-full.mjs"]
runtime_file = "logs/dev-instance.json"
"#,
        )
        .expect("manifest");
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let metadata = load_launch_target_runtime_metadata(
            worktree.path(),
            &config.launch_targets[0],
            &processes,
        )
        .expect("runtime metadata")
        .expect("matching metadata");

        let model = metadata
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == "model")
            .expect("model endpoint");
        assert_eq!(model.status.as_deref(), Some("running"));
    }

    #[test]
    fn rejects_empty_endpoint_status() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        fs::create_dir(worktree.path().join("logs")).expect("logs directory");
        fs::write(
            worktree.path().join("logs/dev-instance.json"),
            format!(
                r#"{{
  "version": 1,
  "targetId": "dev-full",
  "pid": {},
  "startedAt": "2026-08-21T12:37:22.695Z",
  "endpoints": [
    {{"id": "model", "url": "http://127.0.0.1:8200", "primary": false, "status": "  "}}
  ]
}}"#,
                std::process::id()
            ),
        )
        .expect("runtime metadata");
        let config = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["pnpm", "dev:full"]
process_match = ["scripts/dev-full.mjs"]
runtime_file = "logs/dev-instance.json"
"#,
        )
        .expect("manifest");
        let processes = vec![LaunchTargetProcess {
            metadata_members: Vec::new(),
            pid: std::process::id(),
            start_time: crate::process_identity::ProcessIdentity::read(std::process::id())
                .unwrap()
                .start_time,
            argv: Vec::new(),
        }];

        let error = load_launch_target_runtime_metadata(
            worktree.path(),
            &config.launch_targets[0],
            &processes,
        )
        .expect_err("runtime metadata should be rejected");

        assert!(format!("{error:#}").contains("empty status"));
    }
}
