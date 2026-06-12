//! Shared harness for the property tests: random file-tree / ignore-file
//! generation, materialization into a temp dir, and runners for the ground
//! truth tools (git, rsync, watchman).

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;

/// A proptest config with an env-overridable case count (`PROPTEST_CASES`).
pub fn config(default_cases: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_cases);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

/// Path-segment names used for both directories and files. Kept free of
/// whitespace, glob metacharacters, and reserved names (`.git`, `.dsync`,
/// `.gitignore`, `.dsyncexclude`).
const NAMES: &[&str] = &["a", "b", "cc", "d1", "e.txt", "f.tmp", ".h", "x-y"];

/// Single-segment glob bodies for generated patterns.
const ATOMS: &[&str] = &[
    "a", "b", "cc", "d1", "e.txt", "f.tmp", ".h", "x-y", "*", "*.txt", "*.tmp", "?", "??", "c*",
    "*c", "d?", "[a-c]", "[!ab]", "[bd]1", "a**b",
];

#[derive(Debug, Clone)]
pub struct Case {
    /// Regular files to create (repo-root-relative, `/`-separated).
    pub files: BTreeSet<String>,
    /// `.gitignore` files: directory (`""` = root) -> contents.
    pub gitignores: BTreeMap<String, String>,
    /// Contents of the root `.dsyncexclude`, if any.
    pub dsyncexclude: Option<String>,
}

impl Case {
    /// Every regular file that will exist in the tree (including the ignore
    /// files themselves).
    pub fn all_paths(&self) -> BTreeSet<String> {
        let mut paths = self.files.clone();
        for dir in self.gitignores.keys() {
            if dir.is_empty() {
                paths.insert(".gitignore".to_string());
            } else {
                paths.insert(format!("{dir}/.gitignore"));
            }
        }
        if self.dsyncexclude.is_some() {
            paths.insert(".dsyncexclude".to_string());
        }
        paths
    }
}

fn name() -> impl Strategy<Value = String> {
    proptest::sample::select(NAMES).prop_map(str::to_string)
}

fn atom() -> impl Strategy<Value = String> {
    proptest::sample::select(ATOMS).prop_map(str::to_string)
}

/// One gitignore-syntax pattern line.
fn pattern(allow_negation: bool) -> impl Strategy<Value = String> {
    let core = prop_oneof![
        3 => atom(),
        1 => atom().prop_map(|s| format!("/{s}")),
        1 => (atom(), atom()).prop_map(|(a, b)| format!("{a}/{b}")),
        1 => atom().prop_map(|s| format!("**/{s}")),
        1 => atom().prop_map(|s| format!("{s}/**")),
        1 => (atom(), atom()).prop_map(|(a, b)| format!("{a}/**/{b}")),
    ];
    let negated = if allow_negation {
        proptest::bool::weighted(0.3).boxed()
    } else {
        Just(false).boxed()
    };
    (negated, core, proptest::bool::weighted(0.25)).prop_map(|(neg, core, dir_only)| {
        format!(
            "{}{core}{}",
            if neg { "!" } else { "" },
            if dir_only { "/" } else { "" }
        )
    })
}

/// Generates a whole test case: a file tree plus ignore files.
pub fn case_strategy(allow_negation: bool, with_dsyncexclude: bool) -> BoxedStrategy<Case> {
    let dirs = proptest::collection::vec(
        proptest::collection::vec(name(), 1..=2).prop_map(|v| v.join("/")),
        0..3,
    );
    dirs.prop_flat_map(move |dirs| {
        let mut all_dirs: Vec<String> = vec![String::new()];
        all_dirs.extend(dirs);
        all_dirs.sort();
        all_dirs.dedup();
        let file = (proptest::sample::select(all_dirs.clone()), name())
            .prop_map(|(d, n)| if d.is_empty() { n } else { format!("{d}/{n}") });
        let files = proptest::collection::vec(file, 1..12);
        let ignore_file = (
            proptest::sample::select(all_dirs.clone()),
            proptest::collection::vec(pattern(allow_negation), 1..5),
        );
        let gitignores = proptest::collection::vec(ignore_file, 0..3);
        let dsyncexclude = if with_dsyncexclude {
            proptest::option::of(proptest::collection::vec(pattern(allow_negation), 1..4)).boxed()
        } else {
            Just(None).boxed()
        };
        (files, gitignores, dsyncexclude)
    })
    .prop_map(|(files, gitignores, dsyncexclude)| canonicalize(files, gitignores, dsyncexclude))
    .boxed()
}

fn canonicalize(
    files: Vec<String>,
    gitignores: Vec<(String, Vec<String>)>,
    dsyncexclude: Option<Vec<String>>,
) -> Case {
    let mut ignore_map: BTreeMap<String, String> = BTreeMap::new();
    for (dir, lines) in gitignores {
        let entry = ignore_map.entry(dir).or_default();
        for line in lines {
            entry.push_str(&line);
            entry.push('\n');
        }
    }
    // Every directory that must exist: ancestors (and the dir itself) of
    // ignore-file locations, plus proper ancestors of every file.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut add_prefixes = |path: &str, include_self: bool| {
        let segs: Vec<&str> = path.split('/').collect();
        let n = if include_self {
            segs.len()
        } else {
            segs.len() - 1
        };
        for i in 1..=n {
            dirs.insert(segs[..i].join("/"));
        }
    };
    for dir in ignore_map.keys().filter(|d| !d.is_empty()) {
        add_prefixes(dir, true);
    }
    for f in &files {
        add_prefixes(f, false);
    }
    // Drop files whose path collides with a needed directory.
    let files: BTreeSet<String> = files.into_iter().filter(|f| !dirs.contains(f)).collect();
    Case {
        files,
        gitignores: ignore_map,
        dsyncexclude: dsyncexclude.map(|lines| {
            let mut s = lines.join("\n");
            s.push('\n');
            s
        }),
    }
}

/// Writes the case's tree into a fresh temp dir.
pub fn materialize(case: &Case) -> tempfile::TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("dsync-ignore-prop")
        .tempdir()
        .expect("create tempdir");
    let root = tmp.path();
    for f in &case.files {
        let path = root.join(f);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x\n").unwrap();
    }
    for (dir, contents) in &case.gitignores {
        let path = root.join(dir).join(".gitignore");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    if let Some(contents) = &case.dsyncexclude {
        fs::write(root.join(".dsyncexclude"), contents).unwrap();
    }
    tmp
}

/// A `git` command isolated from the user's and system's configuration.
fn git_cmd(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".nonexistent-xdg"));
    cmd
}

pub fn git_init(root: &Path) {
    let out = git_cmd(root)
        .args(["init", "-q"])
        .output()
        .expect("run git init");
    assert!(
        out.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Ground truth: the set of untracked files git considers ignored.
pub fn git_ignored_files(root: &Path) -> BTreeSet<String> {
    let out = git_cmd(root)
        .args(["ls-files", "-o", "-i", "--exclude-standard", "-z"])
        .output()
        .expect("run git ls-files");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).expect("utf-8 path"))
        .collect()
}

/// Runs rsync in list-only mode with the given filter rules and returns the
/// set of regular files it would transfer.
pub fn rsync_list_files(root: &Path, rules: &[String]) -> BTreeSet<String> {
    let mut cmd = Command::new("rsync");
    cmd.arg("-r").arg("--list-only");
    for rule in rules {
        cmd.arg(format!("--filter={rule}"));
    }
    cmd.arg(format!("{}/", root.display()));
    let out = cmd.output().expect("run rsync");
    assert!(
        out.status.success(),
        "rsync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 rsync output");
    stdout
        .lines()
        .filter(|line| line.starts_with('-'))
        .map(|line| {
            // perms size date time name — generated names contain no spaces.
            line.split_whitespace()
                .nth(4)
                .unwrap_or_else(|| panic!("unparseable rsync line: {line:?}"))
                .to_string()
        })
        .collect()
}

fn watchman_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("watchman");
    cmd.args(args);
    cmd
}

fn run_ok(cmd: &mut Command, what: &str) -> Vec<u8> {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Watches `root`, runs a query with the given expression, deletes the
/// watch, and returns the matched names (sans watchman's own cookie files).
pub fn watchman_query_files(root: &Path, expr: &serde_json::Value) -> BTreeSet<String> {
    let root_str = root.to_str().expect("utf-8 root");
    let _watch = WatchGuard::watch(root_str);
    let query = serde_json::json!([
        "query",
        root_str,
        {
            "expression": expr,
            "fields": ["name"],
            "sync_timeout": 60_000,
        }
    ]);
    let mut child = watchman_cmd(&["-j", "--no-pretty"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watchman -j");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(query.to_string().as_bytes())
        .expect("write watchman query");
    let out = child.wait_with_output().expect("wait for watchman");
    assert!(
        out.status.success(),
        "watchman query failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let resp: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse watchman response");
    if let Some(err) = resp.get("error") {
        panic!("watchman query error: {err}");
    }
    resp["files"]
        .as_array()
        .unwrap_or_else(|| panic!("no files in watchman response: {resp}"))
        .iter()
        .map(|v| v.as_str().expect("file name is a string").to_string())
        .filter(|name| !name.starts_with(".watchman-cookie"))
        .collect()
}

/// Removes the watchman watch on drop, so failures don't leak watches on
/// deleted temp dirs.
struct WatchGuard<'a>(&'a str);

impl<'a> WatchGuard<'a> {
    fn watch(root: &'a str) -> Self {
        run_ok(&mut watchman_cmd(&["watch", root]), "watchman watch");
        WatchGuard(root)
    }
}

impl Drop for WatchGuard<'_> {
    fn drop(&mut self) {
        let _ = watchman_cmd(&["watch-del", self.0]).output();
    }
}
