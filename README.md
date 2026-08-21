# Portboard

Portboard finds the development processes and listening TCP endpoints that belong to the current Git worktree. It can inspect an existing run, start a configured launch target, focus the Herdr tab that owns it, and stop it from the popup or local dashboard.

Portboard works as a standalone command and local web dashboard. Its optional [Herdr](https://herdr.dev) plugin adds a current-workspace popup and dedicated server tabs without making Herdr a requirement.

## The current workspace is the scope

Portboard never asks you to choose a repository or worktree from its Herdr plugin. The Herdr action always receives the current workspace, resolves its Git worktree, and shows only that worktree's launch targets and runs.

```text
Portboard >

▸ Full dev stack    running :17640
  Worker            stopped

enter: open or start · ctrl-s: stop · ctrl-r: refresh · esc: close
```

Repository and worktree navigation remain Herdr's responsibility. Portboard answers a narrower question: what can I run or inspect here?

## Launch targets, runs, and endpoints

A project declares the commands a developer may start as **launch targets**. A matching live process is a **run**. Portboard discovers listening TCP **endpoints** owned by that process or its descendants.

For example, one `pnpm dev:full` launch target can be represented by a parent process plus Vite and API child processes. If those children listen on ports, the ports appear in text status, JSON status, the popup, and the dashboard.

The generic runtime state is `stopped` or `running`. `running` means that a matching process was revalidated in `/proc`; it is not an application-health assertion. Portboard does not currently infer HTTP health, worker readiness, Docker state, or richer `starting`/`ready`/`degraded` lifecycle states.

## Project configuration

Add `portboard.toml` at the root of the repository:

```toml
version = 1

[[launch_targets]]
id = "dev-full"
label = "Full dev stack"
argv = ["pnpm", "dev:full"]
process_match = ["scripts/dev-full.mjs"]

[[launch_targets]]
id = "dev-worker"
label = "Worker"
argv = ["pnpm", "dev:worker"]
process_match = ["scripts/dev_worker.py"]
```

`argv` is an argument vector, not a shell command. Portboard runs it from the selected worktree root.

`process_match` identifies a manually started or already-running process. Every value must occur in the process command line, and the process cwd must be inside the current worktree. When `process_match` is omitted, every `argv` value is used as the signature. Choose a signature that remains present in the long-lived process command line; this is what prevents duplicate starts.

Unknown manifest fields, duplicate IDs, blank values, and control characters in labels are rejected instead of being silently ignored.

### Command approval

A repository manifest is executable configuration. Before Portboard starts a target for the first time, it displays the exact cwd and JSON-quoted argument vector and asks for approval. Approval is recorded per worktree, exact manifest contents, and target. Changing `portboard.toml` invalidates prior approvals.

Approvals are stored in `$XDG_STATE_HOME/portboard/approvals.json`, or `~/.local/state/portboard/approvals.json` when `XDG_STATE_HOME` is unset. `PORTBOARD_STATE_DIR` overrides the state directory for isolated automation. A non-interactive invocation must opt in explicitly with `PORTBOARD_APPROVE=1`; otherwise it fails without running the command.

Portboard uses an advisory per-worktree/target launch lock and records the launched root PID plus its Linux start time. Until the configured signature becomes visible or that exact launcher exits, another Portboard process reports the target as starting instead of launching a duplicate. This covers delayed startup across CLI processes and multiple dashboard servers without confusing a reused numeric PID for the original launcher.

## Standalone use

Install the binary from a checkout:

```bash
cargo install --path .
```

Inspect the current worktree:

```bash
portboard status
portboard status --json
```

Text status shows matching PIDs and discovered listener addresses. JSON status includes the target argument vector, revalidated processes, and endpoint objects with `protocol`, `address`, and `port` fields.

Open a launch target:

```bash
portboard open dev-full
```

`open` is idempotent for a target whose `process_match` continues to identify its live process:

- If the target is stopped, Portboard starts it.
- If it is running in the current Herdr workspace, Portboard focuses its tab.
- If it was started manually or runs outside Herdr, Portboard prints its cwd, PIDs, and argument vectors without starting a duplicate.

Without a Herdr workspace environment, a newly started command remains attached to the current terminal.

### Local dashboard

Start the loopback-only dashboard:

```bash
portboard serve
```

The default address is `http://127.0.0.1:9777`. Use `--bind 127.0.0.1:PORT` to select another loopback port and `--cwd PATH` to select a worktree. Non-loopback bind addresses are rejected.

The dashboard polls `GET /api/status` and can approve, start, and stop targets. State-changing requests require the per-server `X-Portboard-Token` embedded in the dashboard page. Commands started by the dashboard are detached into their own process groups; stdout and stderr are written to per-worktree logs below the Portboard state directory. A target's log is replaced when a new run starts. Stop sends `SIGTERM` to the complete dashboard-owned process group. For a manually started matching target, stop sends `SIGTERM` to each revalidated matching PID.

The dashboard supervisor and its in-memory child handles exist for the lifetime of `portboard serve`. Runs remain discoverable through `/proc` if the dashboard exits, but a restarted dashboard does not regain the original `Child` handle or log stream.

## Herdr integration

Build and link the plugin from a local checkout:

```bash
cargo build --release
herdr plugin link ~/dev/portboard
```

Bind its action in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "portboard.open"
description = "portboard: current workspace"
```

Then validate and reload Herdr:

```bash
herdr config check
herdr server reload-config
```

Press `prefix+p` in a workspace to open Portboard for that workspace only.

### Open-or-start behavior

Selecting a launch target performs one operation with context-sensitive behavior:

1. If a matching process is owned by a pane in the current Herdr workspace, Portboard focuses that tab.
2. If it is running outside the current Herdr workspace, Portboard prints inspection details and does not start a duplicate.
3. If it is stopped, Portboard creates a dedicated Herdr tab in the current workspace, starts the command there, and focuses it.

Runs started through the Herdr adapter survive laptop disconnects because the remote Herdr server owns their terminals.

The popup is backed by the same current-worktree configuration, process discovery, endpoint discovery, approval, duplicate-prevention, and stop code as the standalone interfaces. It does not add a worktree picker or global process browser.

## Process and endpoint discovery

Portboard's current Linux discovery path is deliberately small and deterministic:

1. Resolve and canonicalize the current Git worktree.
2. Scan numeric `/proc` entries for command-line signatures whose cwd is inside that worktree.
3. Re-read each process start time before accepting it, preventing stale PID reuse from becoming a live run.
4. Walk matching process descendants, map their socket file descriptors to `/proc/net/tcp` and `/proc/net/tcp6`, and report listening TCP addresses.

A process that exits during inspection is discarded. Endpoint discovery is limited to the current Linux network namespace and TCP listeners visible through procfs.

Portboard does **not** currently inspect Docker/Compose metadata, consume project-specific runtime registration files, perform health checks, manage Tailscale Serve, expose raw TCP services, or publish browser-ready URLs. Those require explicit configuration and ownership/reconciliation semantics that are not part of this release.

## Architecture

The implemented adapters share one Rust core:

```text
Portboard core
├── current-worktree resolution
├── strict launch-target configuration
├── manifest approval and launch locking
├── /proc process revalidation
├── descendant TCP listener discovery
└── start, inspect, and stop operations

Run hosts
├── Herdr tabs and panes
├── standalone attached processes
└── dashboard-owned detached process groups

Presentations
├── command line and JSON status
├── fzf current-workspace popup
└── loopback web dashboard and JSON API
```

## Requirements

- Linux with procfs
- Git
- Rust 1.87 or newer when building from source
- `fzf` 0.71 or newer for the Herdr popup
- Optional: Herdr 0.7.4 or newer

## License

MIT
