use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

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
    let Some(runtime_file) = &target.runtime_file else {
        return Ok(None);
    };
    let path = worktree_root.join(runtime_file);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    if !processes.iter().any(|process| process.pid == metadata.pid) {
        return Ok(None);
    }
    Ok(Some(metadata))
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
        let parsed_url = Url::parse(&endpoint.url).with_context(|| {
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
            if status.trim().is_empty() {
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
            pid: std::process::id(),
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
            pid: std::process::id(),
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
            pid: std::process::id(),
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
            pid: std::process::id(),
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
