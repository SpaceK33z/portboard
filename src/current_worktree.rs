use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Resolves a workspace directory to the root of its current Git worktree.
pub fn resolve_current_worktree(workspace_directory: &Path) -> Result<PathBuf> {
    let canonical_workspace = workspace_directory.canonicalize().with_context(|| {
        format!(
            "Portboard workspace could not resolve {}",
            workspace_directory.display()
        )
    })?;
    let output = Command::new("git")
        .args(["-c", "core.quotePath=false", "rev-parse", "--show-toplevel"])
        .current_dir(&canonical_workspace)
        .output()
        .context("Portboard workspace could not run git rev-parse")?;
    if !output.status.success() {
        bail!(
            "Portboard workspace {} is not inside a Git worktree",
            canonical_workspace.display()
        );
    }
    let mut root = output.stdout;
    // `git rev-parse` terminates its output with a newline. Remove only that
    // terminator: trimming text would corrupt valid repository names ending in
    // whitespace, and decoding as UTF-8 would reject valid Linux paths.
    if root.last() == Some(&b'\n') {
        root.pop();
    }
    if root.last() == Some(&b'\r') {
        root.pop();
    }
    let root = PathBuf::from(OsString::from_vec(root));
    root.canonicalize().with_context(|| {
        format!(
            "Portboard worktree root could not resolve {}",
            root.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    use super::resolve_current_worktree;

    #[test]
    fn supports_non_utf8_git_root_paths() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary
            .path()
            .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .expect("git init");
        assert!(status.success());

        let resolved = resolve_current_worktree(&nested).expect("Git worktree root");

        assert_eq!(resolved, root.canonicalize().expect("canonical root"));
    }

    #[test]
    fn preserves_trailing_spaces_in_git_root_paths() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("project with trailing space ");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .expect("git init");
        assert!(status.success());

        let resolved = resolve_current_worktree(&nested).expect("Git worktree root");

        assert_eq!(resolved, root.canonicalize().expect("canonical root"));
    }

    #[test]
    fn resolves_git_root_from_nested_workspace_directory() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path();
        let nested = root.join("packages/web");
        fs::create_dir_all(&nested).expect("nested directory");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .expect("git init");
        assert!(status.success());

        let resolved = resolve_current_worktree(&nested).expect("Git worktree root");

        assert_eq!(resolved, root.canonicalize().expect("canonical root"));
    }
}
