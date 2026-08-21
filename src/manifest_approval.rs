use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::launch_target_config::{parse_launch_target_config, LaunchTarget, PortboardConfig};
use crate::state_paths::state_root;

const APPROVAL_STORE_VERSION: u32 = 1;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ApprovalStore {
    version: u32,
    repositories: Vec<RepositoryApproval>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RepositoryApproval {
    worktree_root: Vec<u8>,
    manifest: String,
    target_ids: Vec<String>,
}

/// Requires explicit approval before executing a repository-provided command.
pub fn ensure_launch_target_approved(
    worktree_root: &Path,
    loaded_config: &PortboardConfig,
    target: &LaunchTarget,
) -> Result<()> {
    let manifest = current_manifest(worktree_root, loaded_config)?;
    let store_path = approval_store_path()?;
    if is_approved_at(&store_path, worktree_root, &manifest, target.id.as_str())? {
        return Ok(());
    }

    print_command_disclosure(worktree_root, target)?;
    let approved_by_environment = std::env::var("PORTBOARD_APPROVE").ok().as_deref() == Some("1");
    if !approved_by_environment {
        if !io::stdin().is_terminal() {
            bail!(
                "Portboard needs approval before running this repository command; rerun in a terminal or set PORTBOARD_APPROVE=1 for this invocation"
            );
        }
        eprint!("Approve this command for the current manifest? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("Portboard could not read manifest approval")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("Portboard command was not approved");
        }
    }

    record_approval_at(&store_path, worktree_root, &manifest, target.id.as_str())
}

/// Records an approval after a trusted UI has shown the command and received a
/// positive user action.
pub fn approve_launch_target(
    worktree_root: &Path,
    loaded_config: &PortboardConfig,
    target: &LaunchTarget,
    expected_manifest: &str,
) -> Result<()> {
    let manifest = current_manifest(worktree_root, loaded_config)?;
    if manifest != expected_manifest {
        bail!("Portboard manifest changed after it was displayed; refresh and review it again");
    }
    record_approval_at(
        &approval_store_path()?,
        worktree_root,
        &manifest,
        target.id.as_str(),
    )
}

/// Reads the exact manifest snapshot corresponding to a parsed configuration.
pub fn current_manifest_snapshot(
    worktree_root: &Path,
    loaded_config: &PortboardConfig,
) -> Result<String> {
    current_manifest(worktree_root, loaded_config)
}

/// Returns whether the exact current manifest and target were approved.
pub fn launch_target_is_approved(
    worktree_root: &Path,
    loaded_config: &PortboardConfig,
    target: &LaunchTarget,
) -> Result<bool> {
    let manifest = current_manifest(worktree_root, loaded_config)?;
    is_approved_at(
        &approval_store_path()?,
        worktree_root,
        &manifest,
        target.id.as_str(),
    )
}

fn current_manifest(worktree_root: &Path, loaded_config: &PortboardConfig) -> Result<String> {
    let path = worktree_root.join("portboard.toml");
    let manifest = fs::read_to_string(&path)
        .with_context(|| format!("Portboard manifest could not reread {}", path.display()))?;
    let current_config = parse_launch_target_config(&manifest)
        .with_context(|| format!("Portboard manifest changed at {}", path.display()))?;
    if &current_config != loaded_config {
        bail!(
            "Portboard manifest changed while the command was being prepared; retry the operation"
        );
    }
    Ok(manifest)
}

fn print_command_disclosure(worktree_root: &Path, target: &LaunchTarget) -> Result<()> {
    eprintln!("Portboard repository command requires approval:");
    eprintln!("  cwd: {}", worktree_root.display());
    eprintln!("  argv: {}", serde_json::to_string(&target.argv)?);
    Ok(())
}

fn approval_store_path() -> Result<PathBuf> {
    Ok(state_root()?.join("approvals.json"))
}

fn is_approved_at(
    store_path: &Path,
    worktree_root: &Path,
    manifest: &str,
    target_id: &str,
) -> Result<bool> {
    let lock = lock_store(store_path)?;
    let store = read_store(store_path)?;
    drop(lock);
    Ok(store.repositories.iter().any(|repository| {
        repository.worktree_root == worktree_identity(worktree_root)
            && repository.manifest == manifest
            && repository.target_ids.iter().any(|id| id == target_id)
    }))
}

fn record_approval_at(
    store_path: &Path,
    worktree_root: &Path,
    manifest: &str,
    target_id: &str,
) -> Result<()> {
    let _lock = lock_store(store_path)?;
    let mut store = read_store(store_path)?;
    store.version = APPROVAL_STORE_VERSION;

    if let Some(repository) = store
        .repositories
        .iter_mut()
        .find(|repository| repository.worktree_root == worktree_identity(worktree_root))
    {
        if repository.manifest != manifest {
            repository.manifest = manifest.to_string();
            repository.target_ids.clear();
        }
        if !repository.target_ids.iter().any(|id| id == target_id) {
            repository.target_ids.push(target_id.to_string());
            repository.target_ids.sort();
        }
    } else {
        store.repositories.push(RepositoryApproval {
            worktree_root: worktree_identity(worktree_root),
            manifest: manifest.to_string(),
            target_ids: vec![target_id.to_string()],
        });
    }
    write_store(store_path, &store)
}

fn worktree_identity(worktree_root: &Path) -> Vec<u8> {
    worktree_root.as_os_str().as_bytes().to_vec()
}

fn lock_store(store_path: &Path) -> Result<File> {
    let parent = store_path
        .parent()
        .context("Portboard approval store has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Portboard could not create approval directory {}",
            parent.display()
        )
    })?;
    let lock_path = store_path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("Portboard could not open {}", lock_path.display()))?;
    // SAFETY: flock only uses the live file descriptor for the duration of the
    // call. The returned File keeps the advisory lock until it is dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("Portboard could not lock {}", lock_path.display()));
    }
    Ok(lock)
}

fn read_store(path: &Path) -> Result<ApprovalStore> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ApprovalStore {
                version: APPROVAL_STORE_VERSION,
                repositories: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Portboard could not read {}", path.display()));
        }
    };
    let store: ApprovalStore = serde_json::from_str(&contents)
        .with_context(|| format!("Portboard approval store is invalid at {}", path.display()))?;
    if store.version != APPROVAL_STORE_VERSION {
        bail!(
            "Portboard approval store version {} is unsupported",
            store.version
        );
    }
    Ok(store)
}

fn write_store(path: &Path, store: &ApprovalStore) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("Portboard could not create {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut output, store)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    fs::rename(&temporary, path).with_context(|| {
        format!(
            "Portboard could not replace approval store {}",
            path.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_approved_at, record_approval_at};

    #[test]
    fn approvals_are_bound_to_exact_manifest_and_target() {
        let temporary = tempfile::tempdir().expect("temporary state");
        let store = temporary.path().join("approvals.json");
        let worktree = temporary.path().join("repo");

        assert!(!is_approved_at(&store, &worktree, "version = 1", "web").expect("approval status"));
        record_approval_at(&store, &worktree, "version = 1", "web").expect("record approval");
        assert!(is_approved_at(&store, &worktree, "version = 1", "web").expect("approval status"));
        assert!(!is_approved_at(&store, &worktree, "version = 2", "web").expect("changed manifest"));
        assert!(
            !is_approved_at(&store, &worktree, "version = 1", "worker").expect("different target")
        );
    }

    #[test]
    fn approving_a_changed_manifest_revokes_other_targets() {
        let temporary = tempfile::tempdir().expect("temporary state");
        let store = temporary.path().join("approvals.json");
        let worktree = temporary.path().join("repo");

        record_approval_at(&store, &worktree, "first", "web").expect("first approval");
        record_approval_at(&store, &worktree, "first", "worker").expect("second approval");
        record_approval_at(&store, &worktree, "changed", "web").expect("changed approval");

        assert!(!is_approved_at(&store, &worktree, "changed", "worker").expect("revoked target"));
        assert!(is_approved_at(&store, &worktree, "changed", "web").expect("current target"));
    }
}
