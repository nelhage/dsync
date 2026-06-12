//! Parsing and matching of gitignore-style pattern lines.
//!
//! A [`Pattern`] is the structured form of one non-blank, non-comment line of
//! a gitignore-syntax file. The structured representation (path segments,
//! with `**` and per-segment glob tokens made explicit) is shared by the
//! direct evaluator and by the rsync / watchman translators, so that all
//! three consumers agree on what a pattern means.

/// One token within a single path-segment glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// A literal character (after backslash-unescaping).
    Literal(char),
    /// `?` — any single character except `/`.
    AnyChar,
    /// `*` — any run of characters (possibly empty) not crossing `/`.
    /// Consecutive stars within a segment are collapsed: gitignore treats
    /// `a**b` the same as `a*b`.
    Star,
    /// `[...]` bracket expression.
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

/// One element of a bracket expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassItem {
    Char(char),
    Range(char, char),
}

/// One path segment of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// `**` as a whole segment: matches zero or more whole path segments,
    /// except in trailing position where it matches one or more (gitignore's
    /// "`abc/**` matches everything *inside* `abc`").
    DoubleStar,
    /// An ordinary glob over a single path segment.
    Glob(Vec<Tok>),
}

/// A parsed gitignore pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// `!pattern` — a re-include rather than an exclude.
    pub negated: bool,
    /// Trailing `/` — only matches directories.
    pub dir_only: bool,
    /// Whether the pattern contained a non-trailing slash. Unanchored
    /// patterns are normalized into `segments` with a leading
    /// [`Segment::DoubleStar`], so matching code need not consult this; it is
    /// retained so translators can choose more idiomatic output.
    pub anchored: bool,
    /// The normalized segment list, relative to the pattern's base directory.
    pub segments: Vec<Segment>,
    /// The original line text (for diagnostics).
    pub original: String,
}

/// Parses one line of a gitignore-syntax file.
///
/// Returns `None` for blank lines, comments, and lines that can never match
/// anything (e.g. a bare `/` or `!`).
pub fn parse_line(line: &str) -> Option<Pattern> {
    let original = line;
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.starts_with('#') {
        return None;
    }
    let line = strip_trailing_spaces(line);
    if line.is_empty() {
        return None;
    }
    let (negated, line) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (dir_only, line) = match line.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let anchored = line.contains('/');
    let line = line.strip_prefix('/').unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    for raw in line.split('/') {
        if raw.is_empty() {
            // "a//b" cannot match any real path.
            return None;
        }
        let seg = if raw == "**" {
            Segment::DoubleStar
        } else {
            Segment::Glob(tokenize(raw))
        };
        // Collapse runs of `**` segments; they are equivalent to one.
        if seg == Segment::DoubleStar && segments.last() == Some(&Segment::DoubleStar) {
            continue;
        }
        segments.push(seg);
    }
    if !anchored {
        // An unanchored pattern matches its basename at any depth.
        debug_assert_eq!(segments.len(), 1);
        if segments != [Segment::DoubleStar] {
            segments.insert(0, Segment::DoubleStar);
        }
    }
    Some(Pattern {
        negated,
        dir_only,
        anchored,
        segments,
        original: original.to_string(),
    })
}

/// Strips unescaped trailing spaces, per gitignore: "Trailing spaces are
/// ignored unless they are quoted with backslash".
fn strip_trailing_spaces(line: &str) -> &str {
    let mut end = line.len();
    let bytes = line.as_bytes();
    while end > 0 && bytes[end - 1] == b' ' {
        if end >= 2 && bytes[end - 2] == b'\\' {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

fn tokenize(seg: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let chars: Vec<char> = seg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                if i + 1 < chars.len() {
                    toks.push(Tok::Literal(chars[i + 1]));
                    i += 2;
                } else {
                    toks.push(Tok::Literal('\\'));
                    i += 1;
                }
            }
            '?' => {
                toks.push(Tok::AnyChar);
                i += 1;
            }
            '*' => {
                if toks.last() != Some(&Tok::Star) {
                    toks.push(Tok::Star);
                }
                i += 1;
            }
            '[' => match parse_class(&chars, i) {
                Some((tok, next)) => {
                    toks.push(tok);
                    i = next;
                }
                None => {
                    // Unterminated bracket: literal `[` (fnmatch behavior).
                    toks.push(Tok::Literal('['));
                    i += 1;
                }
            },
            c => {
                toks.push(Tok::Literal(c));
                i += 1;
            }
        }
    }
    toks
}

/// Parses a bracket expression starting at `chars[start] == '['`. Returns the
/// token and the index just past the closing `]`, or `None` if unterminated.
fn parse_class(chars: &[char], start: usize) -> Option<(Tok, usize)> {
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let c = *chars.get(i)?;
        if c == ']' && !first {
            return Some((Tok::Class { negated, items }, i + 1));
        }
        first = false;
        let lo = if c == '\\' {
            i += 1;
            *chars.get(i)?
        } else {
            c
        };
        // Range?
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&c| c != ']') {
            let mut j = i + 2;
            let hi = if chars[j] == '\\' {
                j += 1;
                *chars.get(j)?
            } else {
                chars[j]
            };
            items.push(ClassItem::Range(lo, hi));
            i = j + 1;
        } else {
            items.push(ClassItem::Char(lo));
            i += 1;
        }
    }
}

impl Pattern {
    /// Does this pattern match `path` (segments relative to the pattern's
    /// base directory)? `is_dir` is whether the path names a directory.
    ///
    /// This is a *single-path* match: the gitignore rule that everything
    /// under an ignored directory stays ignored is the evaluator's job, not
    /// the pattern's.
    pub fn matches(&self, path: &[&str], is_dir: bool) -> bool {
        if path.is_empty() {
            return false;
        }
        if self.dir_only && !is_dir {
            return false;
        }
        match_segments(&self.segments, path)
    }
}

fn match_segments(pat: &[Segment], path: &[&str]) -> bool {
    let Some((first, rest)) = pat.split_first() else {
        return path.is_empty();
    };
    match first {
        Segment::DoubleStar => {
            if rest.is_empty() {
                // Trailing `**` matches everything inside, i.e. one or more
                // remaining segments.
                return !path.is_empty();
            }
            (0..=path.len()).any(|i| match_segments(rest, &path[i..]))
        }
        Segment::Glob(toks) => {
            let Some((seg, prest)) = path.split_first() else {
                return false;
            };
            let seg_chars: Vec<char> = seg.chars().collect();
            match_glob(toks, &seg_chars) && match_segments(rest, prest)
        }
    }
}

fn match_glob(toks: &[Tok], s: &[char]) -> bool {
    let Some((t, rest)) = toks.split_first() else {
        return s.is_empty();
    };
    match t {
        Tok::Star => (0..=s.len()).any(|i| match_glob(rest, &s[i..])),
        Tok::AnyChar => !s.is_empty() && match_glob(rest, &s[1..]),
        Tok::Literal(c) => s.first() == Some(c) && match_glob(rest, &s[1..]),
        Tok::Class { negated, items } => s.first().is_some_and(|&c| {
            let inside = items.iter().any(|item| match item {
                ClassItem::Char(x) => c == *x,
                ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
            });
            inside != *negated && match_glob(rest, &s[1..])
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(line: &str) -> Pattern {
        parse_line(line).expect("pattern should parse")
    }

    fn matches(line: &str, path: &str, is_dir: bool) -> bool {
        let segs: Vec<&str> = path.split('/').collect();
        pat(line).matches(&segs, is_dir)
    }

    #[test]
    fn blank_and_comment_lines() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("# comment").is_none());
        assert!(parse_line("/").is_none());
        assert!(parse_line("!").is_none());
        assert!(parse_line("a//b").is_none());
    }

    #[test]
    fn escaped_hash_and_bang() {
        let p = pat("\\#literal");
        assert!(!p.negated);
        assert!(p.matches(&["#literal"], false));
        let p = pat("\\!literal");
        assert!(!p.negated);
        assert!(p.matches(&["!literal"], false));
    }

    #[test]
    fn trailing_spaces() {
        assert!(matches("foo  ", "foo", false));
        assert!(!matches("foo  ", "foo  ", false));
        let p = pat("foo\\ ");
        assert!(p.matches(&["foo "], false));
    }

    #[test]
    fn basename_match_any_depth() {
        assert!(matches("foo", "foo", false));
        assert!(matches("foo", "a/b/foo", false));
        assert!(!matches("foo", "foo/bar", false));
        assert!(!matches("foo", "xfoo", false));
    }

    #[test]
    fn anchored() {
        assert!(matches("/foo", "foo", false));
        assert!(!matches("/foo", "a/foo", false));
        assert!(matches("a/b", "a/b", false));
        assert!(!matches("a/b", "x/a/b", false));
    }

    #[test]
    fn dir_only() {
        assert!(matches("build/", "build", true));
        assert!(!matches("build/", "build", false));
        assert!(matches("build/", "a/build", true));
    }

    #[test]
    fn single_star_does_not_cross_slash() {
        assert!(matches("a/*", "a/b", false));
        assert!(!matches("a/*", "a/b/c", false));
        assert!(matches("*.txt", "x/y.txt", false));
        assert!(matches("a/*", "a/.hidden", false));
    }

    #[test]
    fn double_star() {
        // Leading **/: any depth, including zero.
        assert!(matches("**/foo", "foo", false));
        assert!(matches("**/foo", "a/b/foo", false));
        // Trailing /**: everything inside, but not the dir itself.
        assert!(matches("abc/**", "abc/x", false));
        assert!(matches("abc/**", "abc/x/y", false));
        assert!(!matches("abc/**", "abc", true));
        // Middle /**/: zero or more directories.
        assert!(matches("a/**/b", "a/b", false));
        assert!(matches("a/**/b", "a/x/b", false));
        assert!(matches("a/**/b", "a/x/y/b", false));
        // Segment-internal ** is just *.
        assert!(matches("a**b", "ab", false));
        assert!(matches("a**b", "axxb", false));
        assert!(!matches("a**b", "a/b", false));
    }

    #[test]
    fn classes() {
        assert!(matches("[ab]c", "ac", false));
        assert!(matches("[ab]c", "bc", false));
        assert!(!matches("[ab]c", "cc", false));
        assert!(matches("[a-z]x", "qx", false));
        assert!(matches("[!a]x", "bx", false));
        assert!(!matches("[!a]x", "ax", false));
        // Unterminated class is a literal '['.
        assert!(matches("a[b", "a[b", false));
        // ']' directly after '[' (or '[!') is a literal member.
        assert!(matches("[]a]x", "]x", false));
        assert!(matches("[]a]x", "ax", false));
    }

    #[test]
    fn escaped_wildcards() {
        assert!(matches("a\\*b", "a*b", false));
        assert!(!matches("a\\*b", "axb", false));
        assert!(matches("a\\?b", "a?b", false));
        assert!(!matches("a\\?b", "axb", false));
    }

    #[test]
    fn negation_flag() {
        assert!(pat("!foo").negated);
        assert!(!pat("foo").negated);
    }
}
