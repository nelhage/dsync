//! Translation of an [`IgnoreSet`] into rsync filter rules.

use crate::TranslateError;
use crate::eval::IgnoreSet;
use crate::pattern::{Pattern, Segment};
use crate::render::{render_glob, render_literal};

/// Expanding a `**` into "zero segments" / "`**`" variants is exponential in
/// the number of non-trailing `**` segments; patterns beyond this many
/// variants are rejected rather than translated.
const MAX_VARIANTS: usize = 16;

/// Translates the rule set into an ordered list of rsync filter rules
/// (each suitable as a `--filter=RULE` argument).
///
/// rsync applies the first matching rule, while gitignore applies the *last*
/// matching pattern from the highest-precedence file — so rules are emitted
/// highest-precedence first: the built-in `.git` / `.dsync` excludes, then
/// each source from last-added to first, with each file's patterns reversed.
///
/// Every rule is anchored at the transfer root (a leading `/`), prefixed
/// with its source's base directory. Because rsync's `**` cannot match zero
/// path components (unlike gitignore's), each non-trailing `**` is expanded
/// into both variants.
///
/// The caller must run rsync *without* `--delete-excluded`, so that
/// excluded paths are neither sent nor deleted.
pub fn rsync_filter_rules(set: &IgnoreSet) -> Result<Vec<String>, TranslateError> {
    let mut rules = vec!["- /.git".to_string(), "- /.dsync".to_string()];
    for source in set.sources.iter().rev() {
        let mut prefix = String::from("/");
        for seg in &source.base {
            prefix.push_str(&render_literal(seg));
            prefix.push('/');
        }
        for pat in source.patterns.iter().rev() {
            let action = if pat.negated { "+ " } else { "- " };
            let suffix = if pat.dir_only { "/" } else { "" };
            for variant in expand_variants(pat)? {
                rules.push(format!("{action}{prefix}{variant}{suffix}"));
            }
        }
    }
    Ok(rules)
}

/// Renders a pattern's segments into one or more rsync glob strings,
/// expanding each non-trailing `**` into "absent" and "`**`" variants.
fn expand_variants(pat: &Pattern) -> Result<Vec<String>, TranslateError> {
    let mut variants: Vec<Vec<String>> = vec![Vec::new()];
    let last = pat.segments.len() - 1;
    for (i, seg) in pat.segments.iter().enumerate() {
        match seg {
            Segment::Glob(toks) => {
                let rendered = render_glob(toks);
                for v in &mut variants {
                    v.push(rendered.clone());
                }
            }
            Segment::DoubleStar if i == last => {
                // Trailing `**` matches one-or-more segments in gitignore;
                // rsync's `dir/**` likewise only matches strictly inside.
                for v in &mut variants {
                    v.push("**".to_string());
                }
            }
            Segment::DoubleStar => {
                let mut doubled = Vec::with_capacity(variants.len() * 2);
                for v in variants {
                    let mut with = v.clone();
                    with.push("**".to_string());
                    doubled.push(with);
                    doubled.push(v);
                }
                variants = doubled;
                if variants.len() > MAX_VARIANTS {
                    return Err(TranslateError::TooManyVariants {
                        pattern: pat.original.clone(),
                    });
                }
            }
        }
    }
    Ok(variants
        .into_iter()
        .filter(|v| !v.is_empty())
        .map(|v| v.join("/"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(sources: &[(&str, &str)]) -> Vec<String> {
        let mut set = IgnoreSet::new();
        for (base, contents) in sources {
            set.add_source(base, contents);
        }
        rsync_filter_rules(&set).unwrap()
    }

    #[test]
    fn builtins_come_first() {
        let r = rules(&[("", "foo\n")]);
        assert_eq!(r[0], "- /.git");
        assert_eq!(r[1], "- /.dsync");
    }

    #[test]
    fn unanchored_pattern_expands_to_both_depths() {
        let r = rules(&[("", "foo\n")]);
        assert_eq!(r[2..], ["- /**/foo", "- /foo"]);
    }

    #[test]
    fn anchored_pattern_is_anchored() {
        let r = rules(&[("", "/foo\n")]);
        assert_eq!(r[2..], ["- /foo"]);
    }

    #[test]
    fn base_dir_prefixes_rules() {
        let r = rules(&[("sub/dir", "/x.txt\n")]);
        assert_eq!(r[2..], ["- /sub/dir/x.txt"]);
    }

    #[test]
    fn rules_are_emitted_in_reverse_precedence_order() {
        let r = rules(&[("", "/a\n/b\n"), ("", "/c\n")]);
        assert_eq!(r[2..], ["- /c", "- /b", "- /a"]);
    }

    #[test]
    fn negation_and_dir_only() {
        let r = rules(&[("", "build/\n!/keep/\n")]);
        assert_eq!(r[2..], ["+ /keep/", "- /**/build/", "- /build/"]);
    }

    #[test]
    fn middle_doublestar_expands() {
        let r = rules(&[("", "a/**/b\n")]);
        assert_eq!(r[2..], ["- /a/**/b", "- /a/b"]);
    }

    #[test]
    fn trailing_doublestar_is_kept() {
        let r = rules(&[("", "abc/**\n")]);
        assert_eq!(r[2..], ["- /abc/**"]);
    }

    #[test]
    fn bare_doublestar() {
        let r = rules(&[("", "**\n")]);
        assert_eq!(r[2..], ["- /**"]);
    }

    #[test]
    fn too_many_variants_is_an_error() {
        let mut set = IgnoreSet::new();
        set.add_source("", "a/**/b/**/c/**/d/**/e/**/f\n");
        assert!(matches!(
            rsync_filter_rules(&set),
            Err(TranslateError::TooManyVariants { .. })
        ));
    }

    #[test]
    fn literal_metacharacters_are_escaped() {
        let r = rules(&[("", "/a\\*b\n")]);
        assert_eq!(r[2..], ["- /a\\*b"]);
    }
}
