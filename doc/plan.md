# dsync implementation plan

This plan works toward the design in [dsync.md](dsync.md). Phases are ordered so
that each one produces something usable and testable; later phases build on
earlier ones. Design decisions made along the way are recorded at the end.

## Notes for implementation sessions

Status as of 2026-06-12: Phase 0 complete; Phases 1+ not started. Next up:
Phase 1 (and Phase 5 may proceed in parallel — the `dsync-ignore` stub crate
exists).

- [dsync.md](dsync.md) is the authoritative behavior spec; this file covers
  sequencing and recorded decisions. The Decisions section at the bottom was
  agreed with the design owner — don't relitigate it without asking.
- Suggested execution: Phases 0–4 in order in the main session; Phase 5
  (`dsync-ignore` sub-crate) in parallel via a background agent once the
  workspace skeleton exists, since it has no dependency on the sync/IPC code.
  Land each phase as one or more separate commits, and update the phase
  status in this file as work completes.
- Workspace layout: a cargo workspace with members `dsync` (binary `ds`) and
  `dsync-ignore` (Phase 5).

## Phase 0 — Project scaffolding

**Status: done (2026-06-12).** Workspace with `dsync` (binary `ds`) and a
stub `dsync-ignore` lib crate; clap CLI with `sync`/`status`/`barrier`/`exec`
subcommands (aliases `stat`+`s`/`b`/`x`) that exit non-zero with "not
implemented"; tracing logging to stderr (`RUST_LOG`, default `info`);
integration tests in `dsync/tests/cli.rs` drive the real binary via
`CARGO_BIN_EXE_ds`. CI (`.github/workflows/ci.yml`) runs build/test/clippy/fmt
inside the repo's nix dev shell, which provides watchman and rsync for
integration tests. Notes for later phases: subcommand args are minimal stubs
(sync TARGET, barrier `--timeout`, exec `--no-wait` + trailing argv) — extend
as phases land, and update `dsync/tests/cli.rs` when stubs become real.

- Cargo project: binary named `ds`, crate named `dsync`.
- Dependencies: `tokio` (async runtime), `clap` (CLI, with subcommand aliases
  like `s`/`b`/`x`), `watchman_client`, `serde`/`serde_json`, `anyhow` +
  `thiserror`, `tracing` for logging.
- CLI skeleton with `sync`, `status`, `barrier`, `exec` subcommands stubbed.
- CI: build, test, clippy, rustfmt. Watchman installed in CI for integration
  tests.

## Phase 1 — Core sync loop (MVP)

The goal: `ds sync [HOST:]PATH` works end-to-end with full-tree rsync on every
change. No IPC, no fast path.

- Find the git repo root (walk up for `.git`); refuse to run outside a repo.
- Parse the `[HOST:]PATH` target; support both remote (ssh) and local-path
  destinations.
- Connect to watchman, register the repo root as a watch, and subscribe to
  changes. Debounce/settle: coalesce events for a short window (~50–100ms)
  before triggering a sync; changes arriving mid-sync queue a follow-up sync
  (at most one pending).
- On each trigger (and once at startup), run `rsync -a --delete` with exclude
  rules. No `--delete-excluded`: ignored paths are neither sent nor deleted,
  so remote-only build artifacts (e.g. a gitignored `target/`) survive syncs.
- `.git/` and `.dsync/` are always excluded. Because `--delete-excluded` is
  off, a deliberately-created remote `.git` is never deleted.
- Ignore handling, simple version: use rsync's native
  `--filter=':- .gitignore'` (dir-merge of per-directory `.gitignore` files)
  plus excludes derived from `.git/info/exclude` and `core.excludesFile`.
  This is approximately right; exact git-compatible translation is Phase 5.
- Status display: plain line-oriented output via `tracing` (sync started,
  files changed, sync finished, durations, errors).
- Record, in memory, the watchman clock each completed sync corresponds to —
  the foundation for `status`/`barrier` (see clock-handling design below).

Deliverable: usable tool for the basic workflow. Integration tests use
local-path targets with temp git repos.

## Phase 2 — IPC server and `ds status`

- `ds sync` creates `.dsync/` at the repo root and listens on a single UNIX
  socket, `.dsync/dsync.sock`. Replica multiplexing is in-band: requests carry
  a replica name (default `default`), and a `list` request enumerates live
  replicas. One server process per repo.
- Liveness/takeover: hold `flock` on `.dsync/lock` for the process lifetime;
  on startup, if the lock is held, error ("ds sync already running"); if the
  lock is free but a stale socket exists, unlink and bind. (Future: a second
  `ds sync` invocation can instead attach a new replica to the running
  process via the socket.)
- Protocol: newline-delimited JSON request/response over the UDS. Versioned
  with a simple `{"version": 1}` handshake field so we can evolve it.
- Server state, per the design doc's "state not flags" principle:
  - `synced_clock`: watchman clock of the last completed sync + wall-clock
    completion time.
  - `syncing_clock` (optional): clock for an in-flight sync.
  - Target host/path, PID, start time.
- `ds status`: query the server. Up-to-dateness is computed server-side via a
  cookie-synchronized watchman since-query against `synced_clock` (see clock
  handling below), which also yields a count of files pending sync. Render
  PID / target / up-to-date (or N files pending) / syncing, per replica.

### Clock handling (design)

Watchman clocks are documented as opaque, and watchman provides no
compare-two-clocks primitive — but a `since` query given a clock from a dead
watchman instance does not error; it returns `is_fresh_instance: true` plus
the full file list, i.e. exactly the "resync everything" signal. We therefore
never parse or compare clock strings:

- **Only the server touches clocks.** Clients never read or transmit watchman
  clocks; the server reads the clock (with `sync_timeout`, i.e.
  cookie-synchronized) when a request requires it. Reading after the request
  arrives preserves "as-of invocation" semantics.
- **Receipt order is clock order.** All clocks the server holds arrive
  serially over its single watchman connection, and within a watchman
  instance, send order is clock order. The server tags each received clock
  with a local monotonic sequence number and does all ordering on those.
- **Instance restarts** surface as `is_fresh_instance` on the subscription;
  the server responds with a full resync, after which all state is
  new-epoch.

## Phase 3 — `ds barrier`

- Client sends a bare `barrier` request (replica name, optional timeout).
- Server reads the current watchman clock with `sync_timeout` (cookie
  synchronization), assigns it sequence number N per the scheme above, and
  parks the request until a completed sync covers sequence ≥ N; then replies.
  If already covered, replies immediately.
- Waiting on a fixed sequence target (rather than "since-query returns
  empty") means barriers cannot starve under continuous file churn.
- `--timeout` flag; on expiry exit non-zero with a distinct exit code.

## Phase 4 — `ds exec`

- Discover the running sync session (via the `.dsync` socket) to learn
  HOST:PATH; error clearly if no sync is running.
- Perform the barrier (unless `--no-wait`), then `ssh HOST 'cd PATH && exec
  ...'` with proper shell quoting of the argv.
- Allocate a TTY when stdin is a TTY (`ssh -t` behavior); propagate the remote
  exit code as our own.
- For local-path targets, run the command locally with CWD set to the replica.

## Phase 5 — Ignore-rule engine and test suite

**Status: done (2026-06-12).** `dsync-ignore` implements the gitignore
pattern parser/matcher, the layered `IgnoreSet` evaluator (global excludes →
`.git/info/exclude` → `.gitignore` walk → `.dsyncexclude`, with built-in
non-overridable root `.git`/`.dsync` excludes), `load_repo()`, and both
translations: `rsync_filter_rules()` (supports negation; expands each
non-trailing `**` into two variants because rsync's `**` can't match zero
components) and `watchman_ignored_files_expr()` /
`watchman_synced_files_expr()` (wholename `match` terms with
`includedotfiles`; negated patterns return
`TranslateError::UnsupportedNegation`). Property tests compare the evaluator
against `git ls-files -o -i --exclude-standard`, the rsync translation
against `rsync --list-only`, and the watchman translation against
`watchman query`; all pass at 10× default case counts. Notes for
integration (Phases 1/6): treat any `TranslateError` as uncertainty → full
rsync; the `.dsyncexclude` name is now concrete
(`dsync_ignore::DSYNC_EXCLUDE_FILE`); accepted divergences are documented in
the crate docs (`dsync-ignore/src/lib.rs`) — notably no watchman translation
for `!` patterns, and `.dsyncexclude` re-includes don't resurrect
`.gitignore` files inside git-ignored dirs. `load_repo` takes the
`core.excludesFile` *contents* as a parameter; resolving the config is the
caller's job.

This is the highest-risk component; it gets its own phase. It is also largely
independent of Phases 1–4: it lives in its own workspace sub-crate (e.g.
`dsync-ignore`) exposing generic APIs for parsing and translating ignore
patterns, with no dependency on the sync loop or IPC layer. It can be built
concurrently with Phases 1–4 (e.g. by a parallel agent) and integrated when
both are ready.

- Implement a gitignore evaluator (or adopt the `ignore` crate's gitignore
  semantics) as the single source of truth for "is this path synced?".
- Translate rules into the three consumers:
  1. rsync exclude/filter rules (full-tree syncs),
  2. watchman query expressions (fast path file lists),
  3. direct evaluation (sanity checks, `.dsyncexclude`).
- Add `.dsyncexclude` (final name TBD) supporting include (`!pattern`) and
  exclude rules layered after git's rules.
- Property-based tests (`proptest`): generate random ignore files + file
  trees, and compare each translation against the underlying tool's own rule
  engine directly:
  - our evaluator vs. `git check-ignore` (and/or `git ls-files
    --others --ignored`) as ground truth;
  - the rsync translation vs. rsync's list-only mode (rsync invoked with a
    source but no destination enumerates the files it would transfer,
    honoring filters) — no sync-and-diff needed;
  - the watchman translation vs. `watchman query` over the same tree.

  Document (and test) any known, accepted divergences.

## Phase 6 — Small-change fast path

- On a watchman notification below a threshold (N files, M bytes), skip rsync:
  - Build the changed-file list from watchman, filtered through the Phase 5
    rule engine (via the watchman-query translation).
  - Stream a tarball (zstd-compressed when available on both ends — detect
    once at startup and cache) over ssh to a small shell unpacker that also
    applies deletions.
- Correctness valve: any uncertainty (watchman recrawl, query error, rule we
  can't translate, unpacker failure) falls back to a full rsync.
- Periodic and/or on-demand full rsync as a self-healing measure remains
  available regardless.

## Phase 7 — Daemonization

- `ds sync --background`: daemonize (double-fork or re-exec with a flag), log
  to `.dsync/dsync.log`, write a pidfile.
- `ds stop` to terminate; `ds status` already covers monitoring; perhaps
  `ds logs` to tail the log file.

## Phase 8 — Multi-replica support

- Multiple named replicas within the single `ds sync` process, per the
  single-socket design: `status`/`barrier`/`exec`/`stop` already carry a
  replica name from Phase 2.
- Possible UX: a second `ds sync HOST:PATH --name foo` invocation attaches a
  new replica to the running process via the socket instead of erroring.

## Testing strategy (cross-cutting)

- Unit tests per module; integration tests drive the real binary against temp
  git repos with local-path targets (no ssh needed).
- A test harness that waits via `ds barrier` rather than sleeps, which
  dogfoods the barrier mechanism.
- ssh-dependent paths (exec, fast-path unpacker) tested against
  `localhost` ssh where available, behind a feature/env gate so CI can skip.
- Property tests for the rule engine (Phase 5).

## Decisions

Resolved with the design owner (2026-06-12):

1. **`.git/` is not synced.** Always excluded. A possible future extension
   synchronizes git state *via git itself* (pushing objects, managing remote
   `HEAD`) so as not to clobber existing objects and refs — never via rsync.
2. **No `--delete-excluded`.** Default deletion behavior is plain `--delete`.
   Ignored paths are neither sent nor deleted remotely, preserving remote
   build artifacts. `--delete-excluded` may never be supported; its semantics
   interact subtly with our own include/exclude layer.
3. **Single socket, in-band multiplexing.** One `.dsync/dsync.sock` per repo,
   one server process; replica names in the protocol; `list` for discovery.
4. **No first-sync guard.** No marker files, no confirmation prompts; we
   trust the user with `--delete` on first sync. No persistent state where
   avoidable.
5. **Watchman is required.** Hard error at startup if unavailable.
6. **ssh config is the user's.** No managed ControlMaster in v1; revisit as
   an early enhancement since connection reuse strongly affects sync latency.
7. **Clocks are opaque.** No parsing of clock strings; all ordering via
   server-side receipt-order sequence numbers and watchman since-queries
   (see Phase 2 clock-handling design). Barrier clients send a bare request
   and the server reads the clock on their behalf. If a pipelining use case
   ever needs clients to observe "a point in time" (snapshot at T0, work,
   then barrier as-of T0), we'll layer our own logical clock on top of the
   RPC rather than expose watchman clocks.
8. **Plain log-line output first**; TUI later if wanted.
9. **No persisted config in v1.** Target is given on the CLI.
