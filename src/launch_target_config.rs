use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Stable manifest identifier for one launch target in a project.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct LaunchTargetId(String);

impl LaunchTargetId {
    /// Returns the launch target identifier as manifest text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Command and process signature that define one project launch target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchTarget {
    pub id: LaunchTargetId,
    pub label: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub process_match: Vec<String>,
    #[serde(default)]
    pub runtime_file: Option<String>,
    #[serde(default)]
    pub log_file: Option<String>,
}

/// Parsed `portboard.toml` configuration for the current Git worktree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PortboardConfig {
    pub version: u32,
    #[serde(default)]
    pub launch_targets: Vec<LaunchTarget>,
}

/// Parses and validates a project launch target manifest.
pub fn parse_launch_target_config(contents: &str) -> Result<PortboardConfig> {
    let config: PortboardConfig =
        toml::from_str(contents).context("Portboard manifest is not valid TOML")?;
    if config.version != 1 {
        bail!(
            "Portboard manifest version {} is unsupported; expected version 1",
            config.version
        );
    }

    let mut ids = HashSet::new();
    for target in &config.launch_targets {
        validate_launch_target(target)?;
        if !ids.insert(target.id.clone()) {
            bail!(
                "Portboard manifest has duplicate launch target id `{}`",
                target.id.as_str()
            );
        }
    }
    Ok(config)
}

/// Loads `portboard.toml` from the root of the current Git worktree.
pub fn load_worktree_launch_targets(worktree_root: &Path) -> Result<PortboardConfig> {
    let path = worktree_root.join("portboard.toml");
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Portboard manifest could not read {}", path.display()))?;
    parse_launch_target_config(&contents)
        .with_context(|| format!("Portboard manifest failed at {}", path.display()))
}

fn validate_launch_target(target: &LaunchTarget) -> Result<()> {
    let id = target.id.as_str();
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("Portboard launch target id `{id}` must contain only letters, numbers, `-`, or `_`");
    }
    if target.label.trim().is_empty() {
        bail!("Portboard launch target `{id}` needs a non-empty label");
    }
    if target.label.chars().any(char::is_control) {
        bail!("Portboard launch target `{id}` label cannot contain control characters");
    }
    if target.argv.is_empty()
        || target
            .argv
            .iter()
            .any(|argument| argument.trim().is_empty())
    {
        bail!("Portboard launch target `{id}` needs a non-empty argv command");
    }
    if target
        .process_match
        .iter()
        .any(|part| part.trim().is_empty())
    {
        bail!("Portboard launch target `{id}` has an empty process_match value");
    }
    validate_worktree_relative_file(id, "runtime_file", target.runtime_file.as_deref())?;
    validate_worktree_relative_file(id, "log_file", target.log_file.as_deref())?;
    Ok(())
}

fn validate_worktree_relative_file(id: &str, field: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("Portboard launch target `{id}` {field} must be a relative path inside the worktree");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_launch_target_config;

    #[test]
    fn parses_launch_targets_from_project_manifest() {
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["pnpm", "dev:full"]
process_match = ["scripts/dev-full.mjs"]
runtime_file = "logs/dev-instance.json"
log_file = "logs/dev.log"
"#,
        )
        .expect("valid manifest");

        assert_eq!(config.version, 1);
        assert_eq!(config.launch_targets.len(), 1);
        assert_eq!(config.launch_targets[0].id.as_str(), "dev-full");
        assert_eq!(config.launch_targets[0].argv, ["pnpm", "dev:full"]);
        assert_eq!(
            config.launch_targets[0].runtime_file.as_deref(),
            Some("logs/dev-instance.json")
        );
        assert_eq!(
            config.launch_targets[0].log_file.as_deref(),
            Some("logs/dev.log")
        );
    }

    #[test]
    fn rejects_blank_command_and_process_match_values() {
        for manifest in [
            r#"
version = 1
[[launch_targets]]
id = "dev"
label = "Dev"
argv = ["   "]
"#,
            r#"
version = 1
[[launch_targets]]
id = "dev"
label = "Dev"
argv = ["dev"]
process_match = ["\t"]
"#,
        ] {
            parse_launch_target_config(manifest).expect_err("blank values must fail");
        }
    }

    #[test]
    fn rejects_files_outside_the_worktree() {
        for field in ["runtime_file", "log_file"] {
            let manifest = format!(
                r#"
version = 1
[[launch_targets]]
id = "dev"
label = "Dev"
argv = ["dev"]
{field} = "../outside"
"#
            );
            let error = parse_launch_target_config(&manifest)
                .expect_err("worktree-relative file path must fail");
            assert!(error.to_string().contains(field));
        }
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let error = parse_launch_target_config(
            r#"
version = 1
[[launch_targets]]
id = "dev"
label = "Dev"
argv = ["dev"]
process_matches = ["dev"]
"#,
        )
        .expect_err("misspelled fields must fail");

        assert!(error.to_string().contains("not valid TOML"));
    }

    #[test]
    fn rejects_duplicate_launch_target_ids() {
        let error = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "dev"
label = "First"
argv = ["first"]

[[launch_targets]]
id = "dev"
label = "Second"
argv = ["second"]
"#,
        )
        .expect_err("duplicate ids must fail");

        assert_eq!(
            error.to_string(),
            "Portboard manifest has duplicate launch target id `dev`"
        );
    }
}
