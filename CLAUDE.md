# dsync

`dsync` supports editing code locally while building/running it on a remote
node: a `ds sync` daemon watches a git repo with watchman and propagates
changes to a remote (or local-path) replica via rsync, with `ds barrier` /
`ds exec` for synchronization-aware remote execution. Written in async Rust;
the binary is named `ds`.

## Documents — read before significant work

- `doc/dsync.md` — the design document. Authoritative for behavior.
- `doc/plan.md` — phased implementation plan, **decisions log**, and
  environment notes. The decisions were agreed with the project owner; don't
  relitigate them without asking. Update phase status there as work lands.

## Key invariants

- `.git/` is never synced; `.dsync/` is never synced.
- rsync runs with `--delete` but never `--delete-excluded`: ignored paths are
  neither sent nor deleted, so remote-only build artifacts survive.
- Watchman clocks are opaque: never parse or compare clock strings. All clock
  handling lives in the server, ordered by receipt-order sequence numbers;
  clients never observe clocks.
- One server process per repo, one UNIX socket (`.dsync/dsync.sock`),
  newline-delimited JSON protocol, replica names multiplexed in-band.
- IPC reports state ("synced as-of clock X at time T"), never transient
  boolean flags.
- watchman is required: hard error if unavailable, and any uncertainty
  (recrawl, fresh instance, query failure) falls back to a full rsync.

## Layout

Cargo workspace: `dsync` (the `ds` binary) and `dsync-ignore` (ignore-rule
parsing/translation: gitignore semantics → rsync filters / watchman queries;
independent of the sync and IPC code).

## Environment & testing

- `watchman`, `cargo` (and the entire Rust toolchain), and `rsync` are installed via a nix flake, and activated via `direnv`. If you run into version problems or don't have any of those dependencies, verify that you're picking up the `direnv` environment variables.
- Integration tests drive the real binary against temp git repos with
  local-path sync targets — no ssh required. Wait via `ds barrier`, not
  sleeps. Property tests check ignore-rule translations directly against
  `git check-ignore`, rsync list-only mode, and `watchman query`.
