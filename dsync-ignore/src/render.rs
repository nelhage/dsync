//! Shared rendering of parsed patterns back into glob strings, for the rsync
//! and watchman translators (both consume rsync-style wildmatch globs).

use crate::pattern::{ClassItem, Tok};

/// Characters that are glob metacharacters in rsync filters and watchman
/// `match` expressions, escaped when they appear as literals.
fn push_escaped(out: &mut String, c: char) {
    if matches!(c, '*' | '?' | '[' | ']' | '\\') {
        out.push('\\');
    }
    out.push(c);
}

/// Renders a literal path component (e.g. a base-directory name) with glob
/// metacharacters escaped.
pub(crate) fn render_literal(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        push_escaped(&mut out, c);
    }
    out
}

/// Renders one segment-glob token list as a wildmatch glob string.
pub(crate) fn render_glob(toks: &[Tok]) -> String {
    let mut out = String::new();
    for tok in toks {
        match tok {
            Tok::Literal(c) => push_escaped(&mut out, *c),
            Tok::AnyChar => out.push('?'),
            Tok::Star => out.push('*'),
            Tok::Class { negated, items } => {
                out.push('[');
                if *negated {
                    out.push('!');
                }
                for (i, item) in items.iter().enumerate() {
                    match item {
                        ClassItem::Char(c) => {
                            // Best-effort escaping of characters that would
                            // change meaning at this position.
                            if matches!(c, ']' | '\\' | '-') || (i == 0 && matches!(c, '!' | '^')) {
                                out.push('\\');
                            }
                            out.push(*c);
                        }
                        ClassItem::Range(lo, hi) => {
                            out.push(*lo);
                            out.push('-');
                            out.push(*hi);
                        }
                    }
                }
                out.push(']');
            }
        }
    }
    out
}
