//! The small-change fast path.
//!
//! For a handful of changed files, rsync's cost is dominated by scanning and
//! comparing the whole filesystem tree, not by sending data. When watchman
//! is healthy it tells us exactly which paths changed, so we can skip that
//! scan: query watchman for the changed *syncable* paths since the last
//! completed sync, then stream just those files (as a tar, zstd-compressed
//! when both ends have zstd) plus a deletion list to a thin shell unpacker
//! on the target.
//!
//! This path is a pure optimization guarded by a correctness valve: any
//! uncertainty — a watchman fresh instance, an untranslatable rule set, a
//! file that vanished mid-flight, an unpacker that fails — returns
//! [`Outcome::Fallback`] (or an error) and the caller runs a full rsync,
//! which is always correct on its own.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use watchman_client::prelude::Clock;

use crate::exec::sh_quote;
use crate::server::Watchman;
use crate::target::Target;
use crate::wquery;

/// The design calls the fast path appropriate for "O(1) files modified".
/// Past these limits a whole-tree rsync is competitive and the inline tar
/// stream stops being a win, so we fall back to rsync.
pub const MAX_FILES: usize = 64;
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Whether to zstd-compress the tar stream — decided once at startup by
/// [`detect_compression`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd,
}

/// The result of a fast-path attempt.
pub enum Outcome {
    /// The changes were applied to the target; the sync is complete.
    Applied,
    /// The fast path declined (with a human-readable reason); the caller
    /// must run a full rsync instead. This is the correctness valve, not an
    /// error: the reason is expected (a fresh instance, an oversized change,
    /// a mid-flight race), not a failure.
    Fallback(String),
}

/// Probe both ends for `zstd` once, at startup. Local-path syncs copy on one
/// machine with no network, so compression is pointless and skipped.
pub async fn detect_compression(target: &Target) -> Compression {
    let Target::Remote { host, .. } = target else {
        return Compression::None;
    };
    if has_zstd(None).await && has_zstd(Some(host)).await {
        debug!("zstd available on both ends; fast-path streams will be compressed");
        Compression::Zstd
    } else {
        debug!("zstd not available on both ends; fast-path streams will be uncompressed");
        Compression::None
    }
}

async fn has_zstd(host: Option<&str>) -> bool {
    let mut cmd = match host {
        None => {
            let mut c = tokio::process::Command::new("zstd");
            c.arg("-V");
            c
        }
        Some(host) => {
            let mut c = tokio::process::Command::new("ssh");
            c.arg(host).arg("command -v zstd");
            c
        }
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Attempt to sync the changes since `since` via the fast path.
///
/// `ignored_expr` is the rule set's watchman "ignored files" expression
/// (the caller has already confirmed it is translatable); we query for the
/// paths that changed since `since` and are *not* ignored, including
/// deletions.
pub async fn try_fast_path(
    repo_root: &Path,
    target: &Target,
    wm: &Watchman,
    ignored_expr: &Value,
    since: &Clock,
    compression: Compression,
) -> Result<Outcome> {
    let expr = wquery::not_ignored(ignored_expr);
    let result = wquery::since_query(wm, since, &expr).await?;
    if result.is_fresh_instance {
        // The clock belongs to a dead watchman instance: the result covers
        // the whole tree and we must resync everything.
        return Ok(Outcome::Fallback(
            "watchman reported a fresh instance".into(),
        ));
    }

    let mut sends: Vec<PathBuf> = Vec::new();
    let mut deletes: Vec<PathBuf> = Vec::new();
    for file in result.files {
        if !file.exists {
            // Deletions cover files and directories alike; `rm -rf` on the
            // remote handles either. (A deleted path that was ignored never
            // appears here — the expression excludes it — so remote-only
            // build artifacts are never deleted.)
            deletes.push(file.name);
        } else if file.file_type.as_deref() == Some("d") {
            // A directory: its files arrive as their own entries and tar
            // recreates parents on extract, so nothing to send for the
            // directory itself. (An empty directory is not preserved on the
            // fast path; the periodic full rsync reconciles it.)
        } else {
            // A regular file, symlink, etc. — send it verbatim.
            sends.push(file.name);
        }
    }

    if sends.is_empty() && deletes.is_empty() {
        // Only ignored files (or nothing) changed since the last sync.
        debug!("fast path: no syncable changes");
        return Ok(Outcome::Applied);
    }
    if sends.len() > MAX_FILES {
        return Ok(Outcome::Fallback(format!(
            "{} changed files exceed the fast-path limit of {MAX_FILES}",
            sends.len()
        )));
    }

    // Byte budget: stat the files to send (symlink_metadata, so a symlink is
    // measured rather than its target).
    let mut total_bytes = 0u64;
    for rel in &sends {
        match tokio::fs::symlink_metadata(repo_root.join(rel)).await {
            Ok(meta) => total_bytes += meta.len(),
            // The file vanished after the query (it lost a race with a
            // delete): fall back so the full rsync settles the true state.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Outcome::Fallback(format!(
                    "{} vanished mid-fast-path",
                    rel.display()
                )));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot stat {}", rel.display()));
            }
        }
        if total_bytes > MAX_BYTES {
            return Ok(Outcome::Fallback(format!(
                "changed files exceed the fast-path byte budget of {MAX_BYTES}"
            )));
        }
    }

    apply(repo_root, target, &sends, &deletes, compression).await?;
    debug!(
        sends = sends.len(),
        deletes = deletes.len(),
        "fast path applied"
    );
    Ok(Outcome::Applied)
}

/// Stream the `sends` (as a tar) and apply the `deletes` on the target.
async fn apply(
    repo_root: &Path,
    target: &Target,
    sends: &[PathBuf],
    deletes: &[PathBuf],
    compression: Compression,
) -> Result<()> {
    let payload = if sends.is_empty() {
        Vec::new()
    } else {
        build_tar(repo_root, sends, compression).await?
    };
    let script = unpacker_script(deletes, !sends.is_empty(), compression);

    let mut cmd = match target {
        // Local-path target: run the unpacker with the replica as its CWD.
        Target::Local(path) => {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(&script).current_dir(path);
            c
        }
        // Remote target: cd into the replica on the far side, then unpack.
        Target::Remote { host, path } => {
            let remote = format!("cd {} && {}", sh_quote(path), script);
            let mut c = tokio::process::Command::new("ssh");
            c.arg(host).arg(remote);
            c
        }
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("cannot spawn the fast-path unpacker")?;

    // Feed the tar payload to stdin and close it. When there are no sends
    // the script never reads stdin; writing an empty buffer and closing is
    // still correct.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    stdin
        .write_all(&payload)
        .await
        .context("writing the fast-path payload")?;
    drop(stdin);

    let out = child
        .wait_with_output()
        .await
        .context("the fast-path unpacker failed to run")?;
    if !out.status.success() {
        bail!(
            "fast-path unpacker exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Build a tar archive of `sends` (paths relative to `repo_root`), returning
/// its bytes. NUL-separated names are fed to `tar -T -` so any byte except
/// NUL is safe in a filename.
async fn build_tar(
    repo_root: &Path,
    sends: &[PathBuf],
    compression: Compression,
) -> Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("tar");
    cmd.arg("-c")
        .arg("-C")
        .arg(repo_root)
        .arg("--null")
        .arg("-T")
        .arg("-")
        .arg("-f")
        .arg("-");
    if compression == Compression::Zstd {
        cmd.arg("--zstd");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("cannot spawn tar (is it installed?)")?;

    let mut names = Vec::new();
    for rel in sends {
        names.extend_from_slice(rel.as_os_str().as_bytes());
        names.push(0);
    }
    let mut stdin = child.stdin.take().expect("stdin was piped");
    stdin
        .write_all(&names)
        .await
        .context("writing the tar file list")?;
    drop(stdin);

    let out = child
        .wait_with_output()
        .await
        .context("tar failed to run")?;
    if !out.status.success() {
        bail!(
            "tar exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The shell command the target runs: apply deletions (relative to the CWD,
/// which the caller has set to the replica), then extract the tar from
/// stdin. Returns a single line suitable for `sh -c` or as the argument to
/// `ssh host`.
fn unpacker_script(deletes: &[PathBuf], has_sends: bool, compression: Compression) -> String {
    let mut script = String::new();
    if !deletes.is_empty() {
        // `rm -rf` covers deleted files and directories; `--` guards paths
        // that begin with `-`. Missing paths are not an error for `rm -f`.
        script.push_str("rm -rf --");
        for path in deletes {
            script.push(' ');
            script.push_str(&sh_quote(&path.to_string_lossy()));
        }
        if has_sends {
            script.push_str(" && ");
        }
    }
    if has_sends {
        let zstd = if compression == Compression::Zstd {
            " --zstd"
        } else {
            ""
        };
        script.push_str(&format!("tar -x{zstd} -f -"));
    }
    debug_assert!(!script.is_empty(), "unpacker invoked with no work");
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(rels: &[&str]) -> Vec<PathBuf> {
        rels.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn unpacker_extracts_when_only_sends() {
        let script = unpacker_script(&[], true, Compression::None);
        assert_eq!(script, "tar -x -f -");
    }

    #[test]
    fn unpacker_compresses_when_requested() {
        let script = unpacker_script(&[], true, Compression::Zstd);
        assert_eq!(script, "tar -x --zstd -f -");
    }

    #[test]
    fn unpacker_deletes_then_extracts() {
        let script = unpacker_script(&paths(&["a.txt", "dir/b"]), true, Compression::None);
        assert_eq!(script, "rm -rf -- a.txt dir/b && tar -x -f -");
    }

    #[test]
    fn unpacker_deletes_only() {
        let script = unpacker_script(&paths(&["gone"]), false, Compression::None);
        assert_eq!(script, "rm -rf -- gone");
    }

    #[test]
    fn unpacker_quotes_tricky_deletions() {
        let script = unpacker_script(&paths(&["a file", "x'y"]), false, Compression::None);
        assert_eq!(script, r"rm -rf -- 'a file' 'x'\''y'");
    }
}
