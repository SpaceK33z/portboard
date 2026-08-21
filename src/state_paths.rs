use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Resolves Portboard's user state directory.
pub fn state_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PORTBOARD_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("portboard"));
    }
    let home = std::env::var_os("HOME")
        .context("Portboard needs HOME, XDG_STATE_HOME, or PORTBOARD_STATE_DIR for state")?;
    Ok(PathBuf::from(home).join(".local/state/portboard"))
}

/// Returns a stable, non-revealing state directory for one exact worktree path.
pub fn worktree_state_directory(worktree_root: &Path) -> Result<PathBuf> {
    let digest = Sha256::digest(worktree_root.as_os_str().as_bytes());
    let identity = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(state_root()?.join("worktrees").join(identity))
}
