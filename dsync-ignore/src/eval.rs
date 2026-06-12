//! Layered ignore-rule evaluation with gitignore semantics.

use crate::pattern::{Pattern, parse_line};

/// One ignore file's worth of rules, rooted at a directory.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    /// Path segments of the directory containing the ignore file, relative
    /// to the repo root. Empty for the root (and for root-scoped sources
    /// such as `.git/info/exclude` or the global excludes file).
    pub(crate) base: Vec<String>,
    /// Patterns in file order (later patterns take precedence, per
    /// gitignore's last-match-wins rule).
    pub(crate) patterns: Vec<Pattern>,
}

/// What the rule set says about a single path (ignoring parent directories).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Ignored,
    Reincluded,
    Unmatched,
}

/// A layered set of ignore rules, evaluated with gitignore semantics.
///
/// Sources are added in increasing precedence order; for git's rules that is
/// global excludes, then `.git/info/exclude`, then `.gitignore` files from
/// the root downward (deeper files take precedence), then the
/// [`.dsyncexclude`](crate::DSYNC_EXCLUDE_FILE) layer on top.
///
/// Two rules are built in and cannot be overridden: the repo-root `.git` and
/// `.dsync` entries are always ignored (dsync never syncs them).
#[derive(Debug, Clone, Default)]
pub struct IgnoreSet {
    /// In increasing precedence order.
    pub(crate) sources: Vec<Source>,
}

impl IgnoreSet {
    /// An empty rule set (only the built-in `.git` / `.dsync` exclusions).
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one ignore file. `base` is the repo-root-relative directory the
    /// file's patterns are anchored to (`""` for the root). Sources added
    /// later take precedence over sources added earlier.
    pub fn add_source(&mut self, base: &str, contents: &str) {
        let base: Vec<String> = base
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let patterns = contents.lines().filter_map(parse_line).collect();
        self.sources.push(Source { base, patterns });
    }

    /// Is `path` ignored (i.e. *not* synced)?
    ///
    /// `path` is relative to the repo root, `/`-separated, with no leading
    /// slash. `is_dir` is whether it names a directory. Per gitignore
    /// semantics, a path is ignored if it or any of its ancestor directories
    /// is excluded by the rules; a `!` pattern cannot re-include a path whose
    /// parent directory is excluded.
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return false;
        }
        for i in 1..segs.len() {
            if self.decide(&segs[..i], true) == Decision::Ignored {
                return true;
            }
        }
        self.decide(&segs, is_dir) == Decision::Ignored
    }

    /// The verdict for exactly this path: the highest-precedence matching
    /// pattern wins (later sources beat earlier ones; within a source, later
    /// patterns beat earlier ones).
    fn decide(&self, path: &[&str], is_dir: bool) -> Decision {
        debug_assert!(!path.is_empty());
        // Built-in, non-overridable: the repo-root .git and .dsync entries.
        if path[0] == ".git" || path[0] == ".dsync" {
            return Decision::Ignored;
        }
        for source in self.sources.iter().rev() {
            if path.len() <= source.base.len() {
                continue;
            }
            if !source.base.iter().zip(path).all(|(b, p)| b == p) {
                continue;
            }
            let rel = &path[source.base.len()..];
            for pat in source.patterns.iter().rev() {
                if pat.matches(rel, is_dir) {
                    return if pat.negated {
                        Decision::Reincluded
                    } else {
                        Decision::Ignored
                    };
                }
            }
        }
        Decision::Unmatched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(sources: &[(&str, &str)]) -> IgnoreSet {
        let mut s = IgnoreSet::new();
        for (base, contents) in sources {
            s.add_source(base, contents);
        }
        s
    }

    #[test]
    fn empty_set_ignores_only_builtins() {
        let s = IgnoreSet::new();
        assert!(!s.is_ignored("foo", false));
        assert!(!s.is_ignored("a/b", false));
        assert!(s.is_ignored(".git", true));
        assert!(s.is_ignored(".git/config", false));
        assert!(s.is_ignored(".dsync", true));
        assert!(s.is_ignored(".dsync/dsync.sock", false));
        // Only at the root.
        assert!(!s.is_ignored("sub/.git", true));
    }

    #[test]
    fn basic_ignore_and_reinclude() {
        let s = set(&[("", "*.tmp\n!keep.tmp\n")]);
        assert!(s.is_ignored("a.tmp", false));
        assert!(s.is_ignored("sub/a.tmp", false));
        assert!(!s.is_ignored("keep.tmp", false));
        assert!(!s.is_ignored("other.txt", false));
    }

    #[test]
    fn last_match_wins_within_a_file() {
        let s = set(&[("", "!keep.tmp\n*.tmp\n")]);
        // The later *.tmp overrides the earlier negation.
        assert!(s.is_ignored("keep.tmp", false));
    }

    #[test]
    fn ignored_dir_contents_cannot_be_reincluded() {
        let s = set(&[("", "build/\n!build/keep.txt\n")]);
        assert!(s.is_ignored("build", true));
        assert!(s.is_ignored("build/keep.txt", false));
        assert!(s.is_ignored("build/other.txt", false));
    }

    #[test]
    fn dir_only_does_not_match_files() {
        let s = set(&[("", "build/\n")]);
        assert!(s.is_ignored("build", true));
        assert!(!s.is_ignored("build", false));
        assert!(s.is_ignored("build/x", false));
    }

    #[test]
    fn deeper_gitignore_takes_precedence() {
        let s = set(&[("", "*.log\n"), ("sub", "!debug.log\n")]);
        assert!(s.is_ignored("a.log", false));
        assert!(s.is_ignored("sub2/a.log", false));
        assert!(!s.is_ignored("sub/debug.log", false));
        assert!(s.is_ignored("sub/other.log", false));
    }

    #[test]
    fn nested_source_is_relative_to_its_dir() {
        let s = set(&[("sub", "/top.txt\n")]);
        assert!(s.is_ignored("sub/top.txt", false));
        assert!(!s.is_ignored("top.txt", false));
        assert!(!s.is_ignored("sub/deeper/top.txt", false));
    }

    #[test]
    fn source_patterns_do_not_match_their_own_base_dir() {
        let s = set(&[("sub", "sub\n")]);
        assert!(!s.is_ignored("sub", true));
        assert!(s.is_ignored("sub/sub", true));
    }

    #[test]
    fn later_sources_beat_earlier_ones() {
        // Models .dsyncexclude layered over the git rules.
        let s = set(&[("", "*.bin\n"), ("", "!special.bin\nextra.txt\n")]);
        assert!(s.is_ignored("a.bin", false));
        assert!(!s.is_ignored("special.bin", false));
        assert!(s.is_ignored("extra.txt", false));
    }

    #[test]
    fn builtin_git_cannot_be_reincluded() {
        let s = set(&[("", "!.git\n!.git/**\n")]);
        assert!(s.is_ignored(".git", true));
        assert!(s.is_ignored(".git/HEAD", false));
    }
}
