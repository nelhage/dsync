//! Locating the git repository that `ds` operates on.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Find the root of the git repository containing `start`: the nearest
/// ancestor (including `start` itself) that contains a `.git` entry. `.git`
/// may be a directory (a normal repository) or a file (a worktree or
/// submodule checkout).
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", start.display()))?;
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "{} is not inside a git repository (ds sync must be run within one)",
        start.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_root_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sub = root.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();

        let found = find_repo_root(&sub).unwrap();
        assert_eq!(found, root.canonicalize().unwrap());
        let found = find_repo_root(&root).unwrap();
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn git_file_counts_as_root() {
        // Worktrees and submodules have a `.git` *file*.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").unwrap();

        let found = find_repo_root(&root).unwrap();
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn errors_outside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_repo_root(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not inside a git repository"));
    }
}
