//! Parsing and validation of the `[HOST:]PATH` sync target.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// A sync destination: either a local filesystem path or a remote
/// (ssh-reachable) `HOST:PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Local(PathBuf),
    Remote { host: String, path: String },
}

impl Target {
    /// Parse a `[HOST:]PATH` target, using scp-like rules: if the argument
    /// contains a `:` and the part before the first `:` is non-empty and
    /// contains no `/`, it names a remote host (which may include `user@`).
    /// Otherwise the whole argument is a local path, resolved relative to
    /// the current directory.
    pub fn parse(s: &str) -> Result<Target> {
        if s.is_empty() {
            bail!("sync target must not be empty");
        }
        if let Some((host, path)) = s.split_once(':')
            && !host.is_empty()
            && !host.contains('/')
        {
            if path.is_empty() {
                bail!("remote target {s:?} must include a path after the colon");
            }
            return Ok(Target::Remote {
                host: host.to_string(),
                path: path.to_string(),
            });
        }
        let path = std::path::absolute(s)
            .with_context(|| format!("cannot resolve local target path {s:?}"))?;
        Ok(Target::Local(path))
    }

    /// The destination argument to pass to rsync.
    pub fn rsync_dest(&self) -> OsString {
        match self {
            Target::Local(path) => path.clone().into_os_string(),
            Target::Remote { host, path } => format!("{host}:{path}").into(),
        }
    }

    /// Refuse targets that would sync the repository into itself: a local
    /// destination at or under the repo root would make every sync generate
    /// new changes (and recursively copy the replica), looping forever.
    pub fn validate_against_repo(&self, repo_root: &Path) -> Result<()> {
        let Target::Local(path) = self else {
            return Ok(());
        };
        let resolved = resolve_lexically(path);
        if resolved.starts_with(repo_root) {
            bail!(
                "sync target {} is inside the repository {}; syncing a repo into itself would loop forever",
                path.display(),
                repo_root.display()
            );
        }
        Ok(())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Local(path) => write!(f, "{}", path.display()),
            Target::Remote { host, path } => write!(f, "{host}:{path}"),
        }
    }
}

/// Best-effort resolution of an absolute path for ancestry checks: resolve
/// symlinks in the longest existing ancestor (the target itself may not
/// exist yet), then lexically apply any remaining `.`/`..` components.
fn resolve_lexically(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    let mut canonicalized = false;
    for (i, comp) in path.components().enumerate() {
        if !canonicalized {
            let mut candidate = resolved.clone();
            candidate.push(comp);
            match candidate.canonicalize() {
                Ok(c) => {
                    resolved = c;
                    continue;
                }
                Err(_) => {
                    canonicalized = true;
                    if i == 0 {
                        resolved.push(comp);
                        continue;
                    }
                }
            }
        }
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_targets() {
        assert_eq!(
            Target::parse("host:/srv/repo").unwrap(),
            Target::Remote {
                host: "host".into(),
                path: "/srv/repo".into()
            }
        );
        assert_eq!(
            Target::parse("user@host.example.com:relative/path").unwrap(),
            Target::Remote {
                host: "user@host.example.com".into(),
                path: "relative/path".into()
            }
        );
    }

    #[test]
    fn parses_local_targets() {
        assert_eq!(
            Target::parse("/srv/replica").unwrap(),
            Target::Local(PathBuf::from("/srv/replica"))
        );
        // A '/' before the ':' means local, per scp rules.
        let t = Target::parse("./odd:name").unwrap();
        match t {
            Target::Local(p) => assert!(p.ends_with("odd:name"), "got {}", p.display()),
            other => panic!("expected local target, got {other:?}"),
        }
        // Relative paths resolve against the current directory.
        let t = Target::parse("replica").unwrap();
        match t {
            Target::Local(p) => assert!(p.is_absolute()),
            other => panic!("expected local target, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_targets() {
        assert!(Target::parse("").is_err());
        assert!(Target::parse("host:").is_err());
    }

    #[test]
    fn rejects_target_inside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();

        for bad in [
            repo.clone(),
            repo.join("sub"),
            repo.join("not-yet-created/deeper"),
            repo.join("sub/../sub/x"),
        ] {
            let t = Target::Local(bad.clone());
            assert!(
                t.validate_against_repo(&repo).is_err(),
                "{} should be rejected",
                bad.display()
            );
        }

        let ok = Target::Local(tmp.path().join("elsewhere"));
        assert!(ok.validate_against_repo(&repo).is_ok());
        // Sibling whose name shares a prefix is not "inside".
        let sibling = Target::Local(tmp.path().join("repo2"));
        assert!(sibling.validate_against_repo(&repo).is_ok());

        let remote = Target::parse("host:/anything").unwrap();
        assert!(remote.validate_against_repo(&repo).is_ok());
    }
}
