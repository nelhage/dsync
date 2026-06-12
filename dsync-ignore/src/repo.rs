//! Loading the layered ignore rules of a git repository from disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::eval::IgnoreSet;

/// The dsync-specific overlay file, looked up at the repo root. Supports
/// gitignore syntax — including `!pattern` re-includes — and is layered
/// *after* (at higher precedence than) all of git's own rules.
pub const DSYNC_EXCLUDE_FILE: &str = ".dsyncexclude";

/// Loads the full layered rule set for the repository at `root`:
///
/// 1. `global_excludes` — the contents of the user's `core.excludesFile`,
///    if the caller has resolved one (this crate does not read git config);
/// 2. `$GIT_DIR/info/exclude`;
/// 3. every `.gitignore` in the worktree, root downward, skipping ignored
///    directories (whose `.gitignore` files git itself never reads);
/// 4. the root [`DSYNC_EXCLUDE_FILE`], at highest precedence.
pub fn load_repo(root: &Path, global_excludes: Option<&str>) -> io::Result<IgnoreSet> {
    let mut set = IgnoreSet::new();
    if let Some(contents) = global_excludes {
        set.add_source("", contents);
    }
    if let Some(exclude) = info_exclude_path(root)
        && let Some(contents) = read_if_file(&exclude)?
    {
        set.add_source("", contents.as_str());
    }
    walk_gitignores(&mut set, root, &mut Vec::new())?;
    if let Some(contents) = read_if_file(&root.join(DSYNC_EXCLUDE_FILE))? {
        set.add_source("", contents.as_str());
    }
    Ok(set)
}

/// Resolves `$GIT_DIR/info/exclude`, following a `.git` *file* (worktree /
/// submodule indirection) and its `commondir` if present.
fn info_exclude_path(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    let git_dir = if meta.is_dir() {
        dot_git
    } else {
        let contents = fs::read_to_string(&dot_git).ok()?;
        let target = contents.strip_prefix("gitdir:")?.trim();
        let git_dir = root.join(target);
        match fs::read_to_string(git_dir.join("commondir")) {
            Ok(common) => git_dir.join(common.trim()),
            Err(_) => git_dir,
        }
    };
    Some(git_dir.join("info").join("exclude"))
}

fn read_if_file(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        // `.git/info` may not exist, surfacing as NotADirectory on the join.
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => Ok(None),
        Err(e) => Err(e),
    }
}

/// Depth-first preorder walk adding `.gitignore` sources shallow-to-deep
/// (deeper files are added later, so they take precedence), pruning ignored
/// directories as git does.
fn walk_gitignores(set: &mut IgnoreSet, dir: &Path, rel: &mut Vec<String>) -> io::Result<()> {
    if let Some(contents) = read_if_file(&dir.join(".gitignore"))? {
        set.add_source(&rel.join("/"), contents.as_str());
    }
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue, // non-UTF-8 names are out of scope
        };
        // Don't follow symlinks (git does not descend into them either).
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if rel.is_empty() && (name == ".git" || name == ".dsync") {
            continue;
        }
        subdirs.push(name);
    }
    subdirs.sort();
    for name in subdirs {
        rel.push(name.clone());
        let path = rel.join("/");
        if !set.is_ignored(&path, true) {
            walk_gitignores(set, &dir.join(&name), rel)?;
        }
        rel.pop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn loads_layers_in_precedence_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        write(root, ".git/info/exclude", "from-info\n");
        write(root, ".gitignore", "*.log\nfrom-root\n!from-info\n");
        write(root, "sub/.gitignore", "!debug.log\n");
        write(root, ".dsyncexclude", "!from-root\nextra.txt\n");

        let set = load_repo(root, Some("from-global\n!never-mind\n")).unwrap();

        // Global layer.
        assert!(set.is_ignored("from-global", false));
        // info/exclude applies, and .gitignore can override it.
        assert!(!set.is_ignored("from-info", false));
        // Root .gitignore.
        assert!(set.is_ignored("x.log", false));
        // Deeper .gitignore overrides the root one.
        assert!(!set.is_ignored("sub/debug.log", false));
        assert!(set.is_ignored("sub/other.log", false));
        // .dsyncexclude overrides everything.
        assert!(!set.is_ignored("from-root", false));
        assert!(set.is_ignored("extra.txt", false));
    }

    #[test]
    fn gitignore_inside_ignored_dir_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "build/\n");
        write(root, "build/.gitignore", "!rescued.txt\nweird.txt\n");
        write(root, "other/.gitignore", "*.o\n");

        let set = load_repo(root, None).unwrap();
        assert!(set.is_ignored("build/rescued.txt", false));
        assert!(!set.is_ignored("weird.txt", false));
        assert!(set.is_ignored("other/x.o", false));
    }

    #[test]
    fn gitdir_file_indirection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "gitmeta/info/exclude", "from-exclude\n");
        write(root, ".git", "gitdir: gitmeta\n");

        let set = load_repo(root, None).unwrap();
        assert!(set.is_ignored("from-exclude", false));
    }

    #[test]
    fn missing_everything_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let set = load_repo(tmp.path(), None).unwrap();
        assert!(!set.is_ignored("anything", false));
    }
}
