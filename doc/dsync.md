# dsync -- developer code syncing

`dsync` is designed to support the use case of editing code locally, but building/running it on a remote node. See [my blog post on Stripe's development environment](https://blog.nelhage.com/post/stripe-dev-environment/#architecture-of-the-developer-environment) for some additional context.

## Usage

The basic usage is:

```
ds sync [HOST:]PATH
```

`ds sync` must be run in a `git` repository[^git]. It runs forever, watching the local repository for changes using [watchman]. When it detects a file change, it propagates the changes to `HOST:PATH` using `rsync`. It displays sync status on its terminal.

[^git]: We may relax this requirement later, but it will simplify things, for now.
[watchman]: https://facebook.github.io/watchman/

The `HOST:` component is optional; `dsync` supports synchronizing to an another local filesystem path. This will be very useful for testing `dsync` itself, but can also be useful for syncing into or out of a filesystem path mounted in a container, on a slower (or network) disk, or other circumstances.

While a `ds sync` process is running, it listens on a UNIX domain socket under `.dsync/dsync.sock`. Other commands can talk to the socket. Those include:

### `ds status` (`ds stat` or `ds s` for short)

Shows the status of all running `dsync sync` processes. PID, whether or not they are up-to-date, whether or not they are currently syncing.

### `ds barrier` (`ds b`)

Blocks until `ds sync` is up-to-date as-of the `ds barrier` invocation. Performs an RPC to the sync process, which reads a [watchman timestamp][clock] (using a [synchronization cookie][cookie]) on the client's behalf, and replies once it has synced as-of that timestamp.

(Optional future enhancement: a client might want to observe a clock at time T0, do unrelated work, and then barrier as-of T0, pipelining synchronization against that work. If a concrete use case appears, we can implement this by layering our own logical clock on top of the RPC, without exposing watchman clocks to clients.)

[clock]: https://facebook.github.io/watchman/docs/cmd/clock
[cookie]: https://facebook.github.io/watchman/docs/cookies

Supports a timeout flag; if synchronization is not up-to-date within that interval, it will exit with an error code.

### `ds exec` (`ds x`)

Runs a command over ssh on the remote host, with the remote replica as its CWD. Before launching the command, it performs the equivalent of a `ds barrier` to ensure the remote host is up-to-date. Accepts a `--no-wait` flag to skip the barrier.

(Optional future enhancement: we could pipeline the `ssh` connection with the barrier, but if we're already using `ControlMaster` that may be unnecessary? Experimentation is be required)

## Technical details and future features

### Watchman clock

All watchman clock handling lives in the `ds sync` server. Clocks are treated as fully opaque -- never parsed or compared as strings -- and clients never observe them. Internally, the server orders clocks by receipt: all clocks arrive serially over its single watchman connection, and within a watchman instance, send order is clock order, so a local monotonic sequence number suffices for ordering. A watchman instance restart surfaces as `is_fresh_instance` on the subscription, which triggers a full resync.

`ds status` performs an IPC call to talk to each server. The server's response indicates:

- The server's "up-to-date-as-of" watchman clock, and the wall-clock time at which that sync completed.
- (optional) `currently_syncing`, the watchman clock for which a sync is currently running.
- Up-to-dateness, computed server-side via a cookie-synchronized watchman `since` query against the up-to-date-as-of clock -- which also yields a count of files pending sync.

Note that we always send "state" information which describes the current state of the world, not transient "am I up to date?" flags. Working in this way allows for more unambiguous reconstruction of state, and for interpreting state across time in a clearer way.

### Ignore/exclude lists

`ds sync`, by default, respects git's ignore lists. It reads them in and converts them to `rsync` exclude rules when doing the sync. It runs rsync with `--delete`, although it will likely provide flags to toggle that behavior. `--delete-excluded` is **not** the default (and may not be supported at all): ignored paths are neither sent nor deleted, so remote-only build artifacts (e.g. a gitignored `target/`) survive syncs.

`.git` itself is never synced. As an optional future extension, we may add support for synchronizing git state **via git itself** -- e.g. pushing over objects, managing the remote `HEAD` -- but we would do so via `git`, so as to not clobber existing objects and refs.

It will supports an additional include/exclude list, to override behavior relative to `git`; this may take the form of a `.dsyncexclude` file or something; details to be determined later.

### Small-change fast-path

For small changes -- O(1) file modified -- the cost of synchronization is dominated by `rsync` scanning and comparing the filesystem trees, instead of sending over the data. If `watchman` is healthy, we can skip that work, since we know (from `watchman`) precisely which files have changed.

`dsync` will support a fast-path for small changes, where it directly sends the entire changed file(s) over, likely in the form of zstd-compressed tarball and/or a deletion list, with a thin shell script invoked over `ssh` on the receiving end to unpack. The presence of `zstd` (on each end) will be auto-detected, and disabled if needed.

This feature requires an accurate list of files to-be-synced, which must incorporate any include/exclude rules. `dsync` will probably implement this by additionally translating our include/include rules into a `watchman` query, so that `watchman` can do the evaluation. We will ship an extensive test suite (including property-based tests) that make sure our include/exclude-rule translation matches the behavior of the underlying tools.

### Daemonization

`ds sync` by default runs forever in the foreground, but we will support daemonizing via `ds sync --background` or similar. In that case, the `ds` client command can be used to monitor, including a `ds stop` to terminate the daemon.

### multi-session / session names

Initially we will only support one remote replica, but future work will add support for multiple replicas, run out of the same process. Replicas will have names (with a `default` name like `default` for the initial replica), which can be used alongside commands like `ds barrier` to specify the desired replica.

## Implementation notes

- In `async` Rust
- [watchman_client](https://docs.rs/watchman_client/0.9.0/watchman_client/) looks probably good enough
