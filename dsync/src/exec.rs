//! `ds exec`: run a command on the sync target with the replica directory
//! as its CWD, after a barrier ensures the replica is up-to-date.
//!
//! The running session is discovered via the `.dsync` socket: a `status`
//! request reports the target `[HOST:]PATH`. For a remote target we run
//! `ssh HOST 'cd PATH && exec CMD ARGS...'` with each word shell-quoted
//! (and `-t` when our stdin is a TTY); for a local-path target we run the
//! command directly with the replica as its working directory. Either way
//! we `exec(2)` the command in place, so its exit status — and signal
//! disposition — is inherently our own.

use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::barrier::{self, Outcome};
use crate::client::IpcClient;
use crate::protocol::{DEFAULT_REPLICA, RequestOp, StatusResponse};
use crate::repo;
use crate::target::Target;

pub async fn cmd_exec(no_wait: bool, timeout: Option<f64>, argv: &[String]) -> Result<()> {
    barrier::validate_timeout(timeout)?;
    let repo_root = repo::find_repo_root(&std::env::current_dir()?)?;

    // Discover the running session's target. Connecting also surfaces the
    // "no ds sync is running in this repository" error before any waiting.
    let mut client = IpcClient::connect(&repo_root).await?;
    let status: StatusResponse = client
        .request(RequestOp::Status {
            replica: DEFAULT_REPLICA.to_string(),
        })
        .await?;
    drop(client);
    let target = Target::parse(&status.target).with_context(|| {
        format!(
            "cannot parse sync target {:?} reported by ds sync",
            status.target
        )
    })?;

    if !no_wait {
        match barrier::cmd_barrier(timeout).await? {
            Outcome::Synced => {}
            Outcome::TimedOut => std::process::exit(barrier::TIMEOUT_EXIT_CODE),
        }
    }

    // A clearer error than exec's ENOENT (which would read as "command
    // not found") when the local replica directory is missing.
    if let Target::Local(path) = &target
        && !path.is_dir()
    {
        bail!(
            "replica directory {} does not exist (has a sync completed?)",
            path.display()
        );
    }

    let err = build_command(&target, argv, std::io::stdin().is_terminal()).exec();
    // exec(2) only returns on failure.
    let what = match &target {
        Target::Local(_) => argv[0].as_str(),
        Target::Remote { .. } => "ssh",
    };
    eprintln!("ds exec: cannot execute {what}: {err}");
    std::process::exit(match err.kind() {
        std::io::ErrorKind::NotFound => 127,
        std::io::ErrorKind::PermissionDenied => 126,
        _ => 1,
    });
}

/// Build the command to exec: the argv itself (CWD = replica) for a local
/// target, or an `ssh` invocation for a remote one. `tty` requests remote
/// TTY allocation (`ssh -t`); pass whether our own stdin is a TTY.
fn build_command(target: &Target, argv: &[String], tty: bool) -> Command {
    match target {
        Target::Local(path) => {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(path);
            cmd
        }
        Target::Remote { host, path } => {
            let mut cmd = Command::new("ssh");
            if tty {
                cmd.arg("-t");
            }
            cmd.arg(host).arg(remote_command(path, argv));
            cmd
        }
    }
}

/// The single shell command ssh runs on the remote: cd into the replica
/// and exec the argv, every word quoted for POSIX sh.
fn remote_command(path: &str, argv: &[String]) -> String {
    let mut s = format!("cd {} && exec", sh_quote(path));
    for arg in argv {
        s.push(' ');
        s.push_str(&sh_quote(arg));
    }
    s
}

/// Quote one word for POSIX sh: pass obviously-safe words through, wrap
/// everything else in single quotes (with embedded `'` as `'\''`).
pub(crate) fn sh_quote(s: &str) -> String {
    let safe = |b: u8| b.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&b);
    if !s.is_empty() && s.bytes().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sh_quote("simple"), "simple");
        assert_eq!(sh_quote("a/b._-c=d:e,f@g%h+1"), "a/b._-c=d:e,f@g%h+1");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("two words"), "'two words'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(sh_quote("$HOME"), "'$HOME'");
        assert_eq!(sh_quote("a;b&c|d"), "'a;b&c|d'");
        assert_eq!(sh_quote("*"), "'*'");
        assert_eq!(sh_quote("new\nline"), "'new\nline'");
    }

    #[test]
    fn builds_the_remote_command() {
        assert_eq!(
            remote_command("/srv/replica", &argv(&["make", "-j4"])),
            "cd /srv/replica && exec make -j4"
        );
        assert_eq!(
            remote_command("dir with spaces", &argv(&["echo", "it's a test"])),
            r"cd 'dir with spaces' && exec echo 'it'\''s a test'"
        );
    }

    #[test]
    fn local_targets_run_in_the_replica() {
        let target = Target::Local(PathBuf::from("/srv/replica"));
        let cmd = build_command(&target, &argv(&["make", "-j4"]), true);
        assert_eq!(cmd.get_program(), "make");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-j4"]);
        assert_eq!(
            cmd.get_current_dir(),
            Some(PathBuf::from("/srv/replica")).as_deref()
        );
    }

    #[test]
    fn remote_targets_run_via_ssh() {
        let target = Target::Remote {
            host: "user@host".into(),
            path: "replica".into(),
        };
        let cmd = build_command(&target, &argv(&["true"]), false);
        assert_eq!(cmd.get_program(), "ssh");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["user@host", "cd replica && exec true"]
        );
        // No CWD override: the `cd` happens on the remote side.
        assert_eq!(cmd.get_current_dir(), None);

        // A TTY on our stdin requests one for the remote command.
        let cmd = build_command(&target, &argv(&["true"]), true);
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["-t", "user@host", "cd replica && exec true"]
        );
    }

    #[test]
    fn quoting_round_trips_through_sh() {
        // The quoted form must reproduce the original words exactly when
        // evaluated by a real shell.
        let words = argv(&[
            "printf",
            "%s\\n",
            "",
            "two words",
            "it's",
            "$HOME",
            "`cmd`",
            "a;b&c|d>e<f",
            "*",
            "\\",
            "\"quoted\"",
            "tab\tand\nnewline",
        ]);
        let quoted = words.iter().map(|w| sh_quote(w)).collect::<Vec<_>>();
        let script = format!(
            "for a in {}; do printf '%s\\0' \"$a\"; done",
            quoted.join(" ")
        );
        let out = Command::new("sh")
            .args([OsStr::new("-c"), OsStr::new(&script)])
            .output()
            .expect("run sh");
        assert!(out.status.success());
        // Words come back NUL-terminated; the split after the final NUL is
        // an empty artifact, but the empty *word* in the middle survives.
        let mut got: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
        assert_eq!(got.pop(), Some(&b""[..]));
        let got: Vec<String> = got
            .into_iter()
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .collect();
        assert_eq!(got, words);
    }
}
