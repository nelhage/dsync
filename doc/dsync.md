# dsync -- developer code syncing

`dsync` is designed to support the use case of editing code locally, but building/running it on a remote node. See [my blog post on Stripe's development environment](https://blog.nelhage.com/post/stripe-dev-environment/#architecture-of-the-developer-environment) for some additional context.

## Usage

The basic usage is:

```
ds sync HOST:PATH
```

`ds sync` must be run in a `git` repository[^git]. It runs forever, watching the local repository for changes using [watchman]. When it detects a file change, it propagates the changes to `HOST:PATH` using `rsync`. It displays sync status on its terminal.

While a `ds sync` process is running, it listens on a UNIX domain socket under `.dsync/dsync.sock`. Other commands can talk to the socket. Those include:

### `ds status` (`ds stat` or `ds s` for short)

Shows the status of all running `dsync sync` processes. PID, whether or not they are up-to-date, whether or not they are currently syncing.

### `ds barrier` (`ds b`)

Blocks until `ds sync` is up-to-date as-of the `ds barrier` invocation. Reads a watchman timestamp, and performs an IPC to the sync process using that timestamp.

### `ds exec` (`ds x`)

Runs a command over ssh on the remote host, with the remote replica as its CWD. Before launching the command, it performs the equivalent of a `ds barrier` to ensure the remote host is up-to-date. Accepts a `--no-wait` flag to skip the barrier.

(Optional future enhancement: we could pipeline the `ssh` connection with the barrier, but if we're already using `ControlMaster` that may be unnecessary? Experimentation is be required)

## Technical details

### Watchman clock

`ds status` fetches the current `watchman` clock, and then performs an IPC call to talk to each server. The server's response indicates:

- The server's "up-to-date-as-of" watchman clock, and the wall-clock time at which that sync completed.
- (optional) `currently_syncing`, the watchman clock for which a sync is currently running.

The client can then compare those two clock values, plus the one it observed, to deduce the state of the world. Note that we always send "state" information which describes the current state of the world, not transient "am I up to date?" flags. Working in this way allows for more unambiguous reconstruction of state, and for interpreting state across time in a clearer way.

## Future ideas
### Ignore/exclude lists
### `ssh` session management
### Small-change fast-path
### multi-session / session names
### IPC protocol




[^git]: We may relax this requirement later, but it will simplify things, for now.
[watchman]: https://facebook.github.io/watchman/
