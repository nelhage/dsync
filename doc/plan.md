# dsync implementation plan

This plan works toward the design in [dsync.md](dsync.md). Phases are ordered so
that each one produces something usable and testable; later phases build on
earlier ones. Design decisions made along the way are recorded at the end.

## Notes for implementation sessions

Status as of 2026-06-13: Phases 0–6 complete. Phase 6 wired `dsync-ignore`
into the sync loop (the integration deferred from Phase 5): the full-tree
rsync filters and the status/barrier since-queries now go through the rule
engine, and the small-change fast path streams just the changed files. The
interim Phase 1 dir-merge filters survive only as the fallback for the rare
rule set `dsync-ignore` cannot translate to rsync. Remaining phases: 7
(daemonization) and 8 (multi-replica).

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

**Status: done (2026-06-12).** `ds sync [HOST:]PATH` works end-to-end:
repo-root discovery (`repo.rs`), scp-style target parsing (`target.rs`), and
the watchman-driven sync loop (`sync.rs`). Integration tests
(`dsync/tests/sync.rs`) drive the real binary against temp git repos with
local-path targets. Notes for later phases:

- The rsync invocation is `rsync -a --delete-after --modify-window=-1` plus
  filter rules. Two non-obvious flags, both load-bearing:
  - `--delete-after`, not plain `--delete`: per-directory merge rules
    (`:- .gitignore`) only protect receiver-side files from deletion if the
    receiver has the merge files at deletion time, which delete-during does
    not guarantee on the first sync (see "PER-DIRECTORY RULES AND DELETE" in
    rsync(1)). Without it, a remote-only `target/artifact` is deleted by the
    first sync even though `target/` is gitignored. Still never
    `--delete-excluded`.
  - `--modify-window=-1` enables nanosecond mtime comparison in rsync's
    quick-check. Without it, a file rewritten with same-size contents within
    the same second as the synced copy is skipped *forever* (size and
    integer mtime never change again). Requires receiver rsync >= 3.1.3
    (2018); if very old remote rsyncs ever matter, probe and degrade.
- Settle window is 75ms; "at most one pending sync" falls out of a
  `tokio::sync::watch` latest-value channel between the watchman reader task
  and the sync runner. Failed syncs retry every 2s against the latest
  pending event.
- The subscription carries a coarse filter (`sync::subscription_expr`) that
  drops `.git`/`.dsync` churn — the always-excluded internal paths — so
  constant git activity no longer wakes the sync loop for no-op syncs. It
  mirrors `ignore::builtin_ignored_expr` in the typed `Expr` form
  `SubscribeRequest` requires (the query side uses raw JSON; see `wquery`).
  The filter is only a wakeup optimization and is deliberately *static*
  (`.git`/`.dsync` only, not the dynamic ignore rules, which the fixed
  subscription expression couldn't track across rule edits): the
  authoritative filtering stays in the fast-path since-query and in
  `pending_files`. Other gitignored-path churn can still trigger a wakeup
  that finds nothing to sync; that case returns `Outcome::NoChanges` and is
  silent at INFO (logged only at `trace`).
- `sync.rs` keeps the in-memory `SyncedClock { seq, clock, completed_at }`
  record (receipt-order seq per the clock-handling design); Phase 2 moves
  this into state shared with the IPC server.
- `ds sync` refuses a local target at/under the repo root (it would loop:
  every sync triggers watchman again and recursively copies the replica).
- Integration tests poll the destination with a deadline since `ds barrier`
  doesn't exist yet; switch the harness to barriers in Phase 3. Tests
  isolate the child from the developer's git config via
  `GIT_CONFIG_GLOBAL=/dev/null` (etc.) — keep doing that.

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

**Status: done (2026-06-12).** `.dsync/` + flock liveness (`server.rs:
ControlDir`), the single-socket JSON protocol (`protocol.rs` wire types,
`server.rs` dispatch, `client.rs` `IpcClient`), shared state (`state.rs:
ServerState`, updated by the sync runner), and `ds status` (`status.rs`)
all work as specced below. Notes for later phases:

- Clock invariants are enforced structurally: clocks live only in
  `state::SyncedClock` inside the server; the wire types in `protocol.rs`
  carry only seqs and unix-seconds times (an integration test pins that no
  "clock" key ever appears on the wire). Receipt-order seqs come from the
  central `ServerState::next_seq()` counter — Phase 3's barrier should
  assign its cookie-synchronized clock reads from the same counter.
- Phase 3 hooks: add a `RequestOp` variant (tagged enum, easy to extend);
  the runner records `SyncedClock` into `ServerState` after each completed
  sync — barrier wake-ups will want a notification there (e.g. a
  `tokio::sync::watch` of the last-synced seq) rather than polling.
- The status since-query excludes only `.git`/`.dsync`
  (`server.rs: exclude_internal_paths`); gitignored churn therefore counts
  as "pending" until the next (no-op) sync covers it. Phase 5's
  watchman-query translation should replace that expression.
- Error responses are in-band (`{"version":1,"error":"..."}`); a
  connection survives bad requests. Responses built via `serde_json::Value`
  serialize with sorted keys — don't assert on key order in tests.
- The test harness moved to `dsync/tests/common/mod.rs` (shared by
  `sync.rs`/`status.rs`), and grew `ds()`/`wait_for_socket()` helpers.
  It still waits by polling; switch it to `ds barrier` in Phase 3.

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

**Status: done (2026-06-12).** `ds barrier` (`barrier.rs` client,
`server.rs: handle_barrier`/`wait_for_coverage`) works as specced below;
the integration-test harness (`tests/common/mod.rs`) now waits via real
`ds barrier` runs, and `tests/barrier.rs` covers semantics, the timeout
exit code, and the wire protocol. Notes for later phases:

- The barrier's clock value is read (cookie-synchronized) and immediately
  discarded: only its receipt-order seq matters. Seqs for IPC-read clocks
  are granted by the watchman reader task (`server.rs: SeqAssigner`),
  whose `biased` select drains already-delivered subscription
  notifications before granting — that keeps seq order equal to clock
  receipt order even though notifications and command responses are
  delivered to different tasks. Phase 6's fast path must keep all seq
  assignment on these two paths (reader receipt + granted-after-read).
- Release needs *two* conditions, checked on each sync completion
  (`state.rs: record_synced` bumps a `tokio::sync::watch` generation that
  `subscribe_synced` waiters re-check): `synced.seq >= target_seq`
  (starvation-proof under churn), plus an empty cookie-synchronized
  since-query against the synced clock — without the latter, a barrier
  whose clock was bumped by non-file activity (e.g. its own sync cookie)
  parks forever in a quiet repo, since no notification (hence no sync)
  is coming.
- Timeouts ride in the request (`timeout` seconds, f64); on expiry the
  server replies with the current not-covered state ("state not flags")
  and the client judges via `BarrierResponse::is_covered`, exiting with
  the distinct code 3 (`barrier::TIMEOUT_EXIT_CODE`; 1 = generic error,
  2 = clap usage error). Phase 4's `ds exec` should reuse
  `cmd_barrier`/`Outcome` (and propagate exit 3 on `--timeout`, if exec
  grows one).
- Parked barriers hold their IPC connection; requests on one connection
  are served serially, so a client must not pipeline another request
  behind a barrier on the same connection (one-shot CLI clients don't).
- Harness note: `Harness::barrier()` retries "no ds sync is running"
  within the deadline (startup/stale-socket rebind race);
  `Harness::with_broken_rsync()` shadows rsync with a failing stub to
  force never-synced states for timeout tests.

- Client sends a bare `barrier` request (replica name, optional timeout).
- Server reads the current watchman clock with `sync_timeout` (cookie
  synchronization), assigns it sequence number N per the scheme above, and
  parks the request until a completed sync covers sequence ≥ N; then replies.
  If already covered, replies immediately.
- Waiting on a fixed sequence target (rather than "since-query returns
  empty") means barriers cannot starve under continuous file churn.
- `--timeout` flag; on expiry exit non-zero with a distinct exit code.

## Phase 4 — `ds exec`

**Status: done (2026-06-12).** `ds exec` (`exec.rs`) works as specced
below, plus a `--timeout` flag (mutually exclusive with `--no-wait`) that
exits 3 on expiry like `ds barrier`. Notes for later phases:

- Target discovery is a plain `status` request: the `target` string in
  `StatusResponse` round-trips through `Target::parse` (local targets are
  stored absolute, so scp-style parsing is unambiguous). Phase 8's named
  replicas just need to thread a replica name through `cmd_exec`.
- The command is `exec(2)`'d in place (never spawned), so exit status and
  signal disposition propagate inherently; exec failure exits 127/126 like
  a shell. Remote form: `ssh [-t] HOST 'cd PATH && exec WORDS...'` with
  each word quoted by `exec::sh_quote` (`-t` iff our stdin is a TTY) —
  reuse `sh_quote` for Phase 6's remote unpacker invocation.
- `--timeout` validation is shared via `barrier::validate_timeout`, and the
  barrier-timeout message is phrased for both callers.
- ssh-path integration tests are gated: set `DSYNC_TEST_SSH=localhost` (or
  any non-interactive-auth host that shares this filesystem) to run
  `tests/exec.rs: exec_over_ssh` via `Harness::with_ssh_host`; CI without
  ssh skips it. Everything else is covered with local-path targets, plus a
  unit test that round-trips quoted argv through a real `sh`.

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
`watchman query`; all pass at 10× default case counts. (Post-merge, the
property suite caught one more watchman-translation bug: for patterns
ending in `**`, the files-under term naively appended `/**`, and watchman
collapses `**/**` to a single `**` — weaker than gitignore's trailing
`**`. Fixed by substituting `*/**` for the trailing `**`; pinned by unit
and regression tests.) Notes for
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

**Status: done (2026-06-13).** The Phase 5 ignore engine is now wired into
the sync loop (the deferred integration): full-tree rsync filters and the
status/barrier since-queries both go through `dsync-ignore` (`ignore.rs`),
and a raw watchman since-query helper (`wquery.rs`) carries the
property-tested JSON expressions through `watchman_client::generic_request`
(the typed query API has no raw-expression escape hatch). The fast path
itself lives in `fastpath.rs`: a since-query against the last synced clock
yields the changed, non-ignored paths (deletions included); under the file
(64) and byte (8 MiB) thresholds they stream as a tar (zstd-compressed when
both ends have zstd, probed once at startup) plus a `rm -rf` deletion list to
a shell unpacker — over ssh for remote targets, locally for local-path ones.
The correctness valve in `sync_once` falls back to a full rsync on any
uncertainty (fresh instance, untranslatable rules, oversized payload, a file
that vanished mid-flight, an unpacker failure), and a periodic full rsync
(`HEAL_INTERVAL`, default 5 min; `DSYNC_HEAL_INTERVAL_MS` for tests)
self-heals drift. The recorded synced clock is unchanged (the triggering
event's), so the Phase 2/3 clock-ordering and barrier semantics are
untouched. Notes for later phases: the fast path does not preserve empty
directories (the periodic full rsync reconciles them); `core.excludesFile`
changes mid-session are not picked up (it lives outside the watch).

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
