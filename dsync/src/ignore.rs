//! Bridge from the `dsync-ignore` crate into the sync loop.
//!
//! `dsync-ignore` is the single source of truth for "is this path synced?".
//! It parses the repository's layered ignore rules and translates them for
//! its consumers; this module loads those rules from a repo on disk (reading
//! the one git input `dsync-ignore` deliberately does not — the resolved
//! `core.excludesFile`) and exposes the translations the sync loop needs,
//! each with the fallback dsync's design requires when a rule set cannot be
//! translated exactly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::watch;
use tracing::warn;

use dsync_ignore::{IgnoreSet, load_repo, rsync_filter_rules, watchman_ignored_files_expr};

/// A shared, reloadable handle to the current [`Rules`]. The sync runner
/// owns the [`watch::Sender`] and publishes a fresh snapshot whenever an
/// ignore file changes; the IPC server holds a `RulesHandle` so its
/// since-queries always filter against the live rules.
pub type RulesHandle = watch::Receiver<Arc<Rules>>;

/// A snapshot of a repository's ignore rules, with the rsync and watchman
/// translations precomputed.
///
/// Loading walks the worktree (reading `.gitignore` files), so a `Rules` is
/// a point-in-time snapshot: the sync loop reloads it when an ignore file
/// changes. The two translations are computed up front because both can fail
/// for rule sets dsync cannot express exactly, and the failure mode differs
/// per consumer (see the field docs). The runtime sync path never evaluates
/// rules directly — both rsync and watchman apply them — so the underlying
/// [`IgnoreSet`] is not retained.
pub struct Rules {
    /// rsync `--filter=` rule bodies, highest-precedence first. `None` if
    /// the rules could not be translated (a pathological `**` blow-up); the
    /// caller falls back to the interim per-directory dir-merge filters,
    /// which always apply.
    rsync_filters: Option<Vec<String>>,
    /// Watchman expression matching the *ignored* files. `None` if the rules
    /// use a construct watchman cannot express (a negated `!pattern`); the
    /// caller must then treat the fast path and the filtered since-query as
    /// unavailable and fall back to a full rsync.
    ignored_expr: Option<Value>,
}

impl Rules {
    /// Load the repository's ignore rules from disk and translate them.
    /// Never fails: unreadable rules degrade to "nothing ignored" (a full
    /// sync), and untranslatable rules degrade per-consumer (see the field
    /// docs), both with a warning.
    pub fn load(repo_root: &Path) -> Rules {
        let global = global_excludes_content(repo_root);
        let set = match load_repo(repo_root, global.as_deref()) {
            Ok(set) => set,
            Err(err) => {
                warn!("cannot read ignore rules ({err}); treating the tree as fully synced");
                IgnoreSet::new()
            }
        };
        let rsync_filters = match rsync_filter_rules(&set) {
            Ok(rules) => Some(rules),
            Err(err) => {
                warn!(
                    "cannot translate ignore rules to rsync filters ({err}); using interim filters"
                );
                None
            }
        };
        let ignored_expr = match watchman_ignored_files_expr(&set) {
            Ok(expr) => Some(expr),
            Err(err) => {
                warn!(
                    "cannot translate ignore rules to a watchman expression ({err}); \
                     fast path disabled, full rsync only"
                );
                None
            }
        };
        Rules {
            rsync_filters,
            ignored_expr,
        }
    }

    /// rsync `--filter=` arguments for a full-tree sync, highest-precedence
    /// first, or `None` if the rules could not be translated.
    pub fn rsync_filter_args(&self) -> Option<Vec<OsString>> {
        self.rsync_filters.as_ref().map(|rules| {
            rules
                .iter()
                .map(|rule| {
                    let mut arg = OsString::from("--filter=");
                    arg.push(rule);
                    arg
                })
                .collect()
        })
    }

    /// The watchman expression matching ignored files, if the rules are
    /// translatable. Callers wrap this in `["not", …]` to select synced
    /// files; when `None`, the fast path is unavailable and the caller must
    /// fall back to a full rsync.
    pub fn ignored_expr(&self) -> Option<&Value> {
        self.ignored_expr.as_ref()
    }
}

/// The watchman expression matching the always-ignored internal paths
/// (`.git`/`.dsync` and their contents) and nothing else — the translation
/// of an empty rule set. Used as the fallback for since-queries when the
/// real rules don't translate (a negated pattern), so "pending" still never
/// counts `.git`/`.dsync`.
pub fn builtin_ignored_expr() -> Value {
    watchman_ignored_files_expr(&IgnoreSet::new()).expect("the empty rule set always translates")
}

/// The content of the user's resolved `core.excludesFile`, if one is set (or
/// at git's default location). `dsync-ignore` reads `.git/info/exclude` and
/// the per-directory `.gitignore` files itself, but deliberately leaves
/// resolving git config to the caller; this is that one input.
fn global_excludes_content(repo_root: &Path) -> Option<String> {
    let path = match git_output(
        repo_root,
        &["config", "--path", "--get", "core.excludesFile"],
    ) {
        Ok(Some(path)) => Some(PathBuf::from(path)),
        // Unset: fall back to git's documented default location.
        Ok(None) => default_excludes_file(),
        Err(err) => {
            warn!("cannot read core.excludesFile: {err}");
            None
        }
    }?;
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!("cannot read {}: {err}", path.display());
            None
        }
    }
}

fn default_excludes_file() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("git/ignore"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/git/ignore"))
}

/// Run git in `repo_root` and return its trimmed stdout. `Ok(None)` means
/// git exited non-zero with no output (e.g. `config --get` on an unset key);
/// other failures are errors.
fn git_output(repo_root: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .context("failed to run git")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        if stdout.is_empty() {
            return Ok(None);
        }
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn translates_gitignore_to_rsync_filters_and_watchman_expr() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "*.o\nbuild/\n").unwrap();

        let rules = Rules::load(repo);
        let filters = rules.rsync_filter_args().expect("translatable");
        let rendered: Vec<String> = filters
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Built-in internal excludes always lead.
        assert_eq!(rendered[0], "--filter=- /.git");
        assert_eq!(rendered[1], "--filter=- /.dsync");
        assert!(
            rendered.iter().any(|r| r.contains("*.o")),
            "missing *.o filter in {rendered:?}"
        );
        // The watchman expression is an `anyof` of match terms.
        let expr = rules.ignored_expr().expect("translatable");
        assert_eq!(expr.as_array().unwrap()[0], "anyof");
    }

    #[test]
    fn negated_pattern_disables_watchman_but_not_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "*.log\n!keep.log\n").unwrap();

        let rules = Rules::load(repo);
        // rsync handles negation (`+`/`-` rules)...
        assert!(rules.rsync_filter_args().is_some());
        // ...but watchman cannot, so the fast path is disabled.
        assert!(rules.ignored_expr().is_none());
        // The fallback still excludes the internal paths.
        assert_eq!(builtin_ignored_expr().as_array().unwrap()[0], "anyof");
    }
}
