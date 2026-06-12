//! Translation of an [`IgnoreSet`] into watchman query expressions.

use serde_json::{Value, json};

use crate::TranslateError;
use crate::eval::IgnoreSet;
use crate::pattern::{Pattern, Segment};
use crate::render::{render_glob, render_literal};

/// Builds a watchman expression matching the *files* (not directories) that
/// the rule set ignores: a file matches if some pattern matches it directly,
/// or matches one of its ancestor directories.
///
/// Negated (`!`) patterns cannot be expressed this way — deciding a path
/// requires knowing which of several patterns matched *the same* ancestor,
/// which watchman's term language cannot couple — so any negated pattern
/// yields [`TranslateError::UnsupportedNegation`]. Per dsync's design, the
/// caller treats an untranslatable rule set as "uncertainty" and falls back
/// to a full rsync.
///
/// Watchman's wildmatch (unlike rsync's) lets `**/` match zero directories,
/// matching gitignore, so no variant expansion is needed. All `match` terms
/// set `includedotfiles` (gitignore globs match dotfiles).
pub fn watchman_ignored_files_expr(set: &IgnoreSet) -> Result<Value, TranslateError> {
    let mut terms = vec![
        json!(["dirname", ".git"]),
        json!(["name", ".git", "wholename"]),
        json!(["dirname", ".dsync"]),
        json!(["name", ".dsync", "wholename"]),
    ];
    for source in &set.sources {
        let mut prefix = String::new();
        for seg in &source.base {
            prefix.push_str(&render_literal(seg));
            prefix.push('/');
        }
        for pat in &source.patterns {
            if pat.negated {
                return Err(TranslateError::UnsupportedNegation {
                    pattern: pat.original.clone(),
                });
            }
            let glob = format!("{prefix}{}", render_segments(pat));
            if !pat.dir_only {
                terms.push(match_term(&glob));
            }
            // Files under a directory matched by the pattern. (For a file,
            // `glob + "/**"` simply matches nothing.) When the pattern itself
            // ends in `**`, naively appending `/**` would yield `**/**`,
            // which watchman collapses to a single `**` (the first may match
            // zero segments) — weaker than gitignore's trailing `**`, which
            // matches one or more segments. Substitute `*` for the trailing
            // `**` instead: one segment for the matched directory's required
            // interior component, then `/**` for the file(s) beneath it.
            let under = if pat.segments.last() == Some(&Segment::DoubleStar) {
                let head = glob.strip_suffix("**").expect("DoubleStar renders as **");
                format!("{head}*/**")
            } else {
                format!("{glob}/**")
            };
            terms.push(match_term(&under));
        }
    }
    let mut expr = vec![Value::from("anyof")];
    expr.append(&mut terms);
    Ok(Value::Array(expr))
}

/// Builds the expression for files that *should be synced*: regular files
/// not ignored by the rule set.
pub fn watchman_synced_files_expr(set: &IgnoreSet) -> Result<Value, TranslateError> {
    let ignored = watchman_ignored_files_expr(set)?;
    Ok(json!(["allof", ["type", "f"], ["not", ignored]]))
}

fn match_term(glob: &str) -> Value {
    json!(["match", glob, "wholename", {"includedotfiles": true}])
}

fn render_segments(pat: &Pattern) -> String {
    let parts: Vec<String> = pat
        .segments
        .iter()
        .map(|seg| match seg {
            Segment::DoubleStar => "**".to_string(),
            Segment::Glob(toks) => render_glob(toks),
        })
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(sources: &[(&str, &str)]) -> Result<Value, TranslateError> {
        let mut set = IgnoreSet::new();
        for (base, contents) in sources {
            set.add_source(base, contents);
        }
        watchman_ignored_files_expr(&set)
    }

    #[test]
    fn negation_is_unsupported() {
        assert!(matches!(
            expr(&[("", "*.tmp\n!keep.tmp\n")]),
            Err(TranslateError::UnsupportedNegation { .. })
        ));
    }

    #[test]
    fn empty_set_is_just_builtins() {
        let e = expr(&[]).unwrap();
        assert_eq!(
            e,
            json!([
                "anyof",
                ["dirname", ".git"],
                ["name", ".git", "wholename"],
                ["dirname", ".dsync"],
                ["name", ".dsync", "wholename"],
            ])
        );
    }

    #[test]
    fn pattern_terms() {
        let e = expr(&[("sub", "build/\n*.tmp\n")]).unwrap();
        let arr = e.as_array().unwrap();
        // builtins (4) + 1 term for the dir-only pattern + 2 for *.tmp.
        assert_eq!(arr.len(), 1 + 4 + 3);
        assert_eq!(
            arr[5],
            json!(["match", "sub/**/build/**", "wholename", {"includedotfiles": true}])
        );
        assert_eq!(
            arr[6],
            json!(["match", "sub/**/*.tmp", "wholename", {"includedotfiles": true}])
        );
        assert_eq!(
            arr[7],
            json!(["match", "sub/**/*.tmp/**", "wholename", {"includedotfiles": true}])
        );
    }

    #[test]
    fn trailing_doublestar_requires_interior_segment() {
        // `??/**/` matches directories strictly *inside* a `??` directory;
        // the files-under term must therefore require two segments past the
        // `??`. Appending `/**` naively would give `??/**/**`, which
        // watchman's wildmatch collapses to a single `**` (one segment).
        let e = expr(&[("", "??/**/\n")]).unwrap();
        let arr = e.as_array().unwrap();
        assert_eq!(arr.len(), 1 + 4 + 1);
        assert_eq!(
            arr[5],
            json!(["match", "??/*/**", "wholename", {"includedotfiles": true}])
        );
        // Non-dir-only: the direct term keeps the trailing `**` (one or more
        // segments inside), and the under-term still requires two.
        let e = expr(&[("", "abc/**\n")]).unwrap();
        let arr = e.as_array().unwrap();
        assert_eq!(
            arr[5],
            json!(["match", "abc/**", "wholename", {"includedotfiles": true}])
        );
        assert_eq!(
            arr[6],
            json!(["match", "abc/*/**", "wholename", {"includedotfiles": true}])
        );
    }

    #[test]
    fn synced_files_expr_wraps() {
        let mut set = IgnoreSet::new();
        set.add_source("", "*.o\n");
        let e = watchman_synced_files_expr(&set).unwrap();
        let arr = e.as_array().unwrap();
        assert_eq!(arr[0], json!("allof"));
        assert_eq!(arr[1], json!(["type", "f"]));
        assert_eq!(arr[2].as_array().unwrap()[0], json!("not"));
    }
}
