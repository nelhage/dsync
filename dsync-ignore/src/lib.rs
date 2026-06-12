//! Ignore-rule parsing and translation for dsync.
//!
//! This crate is the single source of truth for "is this path synced?". It
//! parses gitignore-syntax rules from a repository's layered sources (the
//! global excludes file, `.git/info/exclude`, per-directory `.gitignore`
//! files, and dsync's own [`DSYNC_EXCLUDE_FILE`] overlay) and serves three
//! consumers:
//!
//! 1. **Direct evaluation** — [`IgnoreSet::is_ignored`], used for sanity
//!    checks and wherever dsync itself needs a verdict.
//! 2. **rsync filter rules** — [`rsync_filter_rules`], used for full-tree
//!    syncs.
//! 3. **watchman query expressions** — [`watchman_ignored_files_expr`] /
//!    [`watchman_synced_files_expr`], used by the small-change fast path.
//!
//! It is intentionally independent of the sync and IPC code in the `dsync`
//! crate.
//!
//! Two rules are built in and not overridable: the repo-root `.git` and
//! `.dsync` entries are always ignored. Callers run rsync with `--delete`
//! but never `--delete-excluded`, so ignored paths are neither sent nor
//! deleted on the remote.
//!
//! # Known, accepted divergences
//!
//! Each translation is property-tested against the underlying tool's own
//! rule engine (`git check-ignore` semantics via `git ls-files -o -i`,
//! rsync's list-only mode, `watchman query`); see `tests/`. The following
//! divergences are known and accepted:
//!
//! - **Negated patterns have no watchman translation.** Watchman's term
//!   language cannot couple "this exclude matched an ancestor" with "no
//!   higher-precedence re-include matched the *same* ancestor", so
//!   [`watchman_ignored_files_expr`] returns
//!   [`TranslateError::UnsupportedNegation`] if any `!` pattern is present.
//!   Callers must treat this as uncertainty and fall back to a full rsync
//!   (whose filter translation *does* support negation).
//! - **Variant blow-up.** rsync's `**` cannot match zero path components,
//!   so each non-trailing `**` doubles the emitted rsync rules; patterns
//!   expanding past a small cap yield [`TranslateError::TooManyVariants`].
//! - **POSIX character classes** (`[[:alpha:]]`) are not supported; they
//!   parse as plain bracket expressions over the literal characters.
//! - **Exotic file names**: escaping of glob metacharacters (`*?[]\`)
//!   appearing *literally* in path or pattern text is best-effort for the
//!   rsync and watchman translations (rsync's own escaping rules are
//!   conditional); the direct evaluator is exact. Bracket expressions
//!   containing `]`, `-`, or backslash escapes round-trip best-effort.
//! - **Only the repo-root `.git`/`.dsync`** are built-in excludes. A nested
//!   `.git` (e.g. a submodule) is synced unless the user's rules say
//!   otherwise, while git itself would treat it as a repository boundary.
//! - **Unreadable ignore files**: git treats an unreadable `.gitignore` as
//!   empty; [`load_repo`] propagates the I/O error (except `NotFound`).
//! - **`.dsyncexclude` re-includes don't resurrect inner `.gitignore`
//!   files.** Which `.gitignore` files get *read* is decided by git's rules
//!   alone (git never reads ignore files inside directories it ignores); a
//!   `.dsyncexclude` re-include of such a directory re-includes its
//!   contents, but any `.gitignore` inside it stays unread.

mod eval;
mod pattern;
mod render;
mod repo;
mod rsync;
mod watchman;

pub use eval::IgnoreSet;
pub use pattern::{ClassItem, Pattern, Segment, Tok, parse_line};
pub use repo::{DSYNC_EXCLUDE_FILE, load_repo};
pub use rsync::rsync_filter_rules;
pub use watchman::{watchman_ignored_files_expr, watchman_synced_files_expr};

/// Why a rule set could not be translated for a particular consumer.
///
/// Translation failure is not fatal: per dsync's design, any uncertainty
/// falls back to a full rsync (and rsync translation failure would fall back
/// to syncing everything and letting `--delete` plus the direct evaluator
/// sort it out — in practice [`TooManyVariants`](Self::TooManyVariants) is
/// the only rsync-side failure and requires a pathological pattern).
#[derive(Debug, Clone, thiserror::Error)]
pub enum TranslateError {
    /// A `!` pattern has no exact watchman-expression equivalent.
    #[error("negated pattern `{pattern}` cannot be translated to a watchman expression")]
    UnsupportedNegation { pattern: String },
    /// Expanding `**` variants for rsync would produce too many rules.
    #[error("pattern `{pattern}` expands to too many rsync filter variants")]
    TooManyVariants { pattern: String },
}
