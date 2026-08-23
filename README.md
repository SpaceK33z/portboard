# Portboard

Portboard finds the development processes and named HTTP or listening TCP endpoints that belong to the current Git worktree. It can ensure a run lives in a visible Herdr tab, inspect or start a configured launch target, print its browser URL, and stop it from the popup or local dashboard.

Portboard works as a standalone command and local web dashboard. Its optional [Herdr](https://herdr.dev) plugin adds a current-workspace popup and dedicated server tabs without making Herdr a requirement.

## The current workspace is the scope

Portboard never asks you to choose a repository or worktree from its Herdr plugin. The Herdr action always receives the current workspace, resolves its Git worktree, and shows only that worktree's launch targets and runs.

```text
┌ Portboard · /path/to/repo ────────────┐
│ targets            │ Dev server       │
│ ▸ dev-full running │ state running    │
│   worker  stopped  │ argv pnpm dev    │
│                    │ endpoints        │
│                    │ ▸ web http://…   │
├────────────────────┴──────────────────┤
│ enter open/start · o open url · y copy │
│ s stop · r refresh · tab endpoints · q │
└───────────────────────────────────────┘
```

Repository and worktree navigation remain Herdr's responsibility. Portboard answers a narrower question: what can I run or inspect here?

## Launch targets, runs, and endpoints

A project declares the commands a developer may start as **launch targets**. A matching live process is a **run**. Portboard discovers listening TCP **endpoints** owned by that process or its descendants. A target can also publish browser-ready named endpoints through a versioned runtime metadata file.

For example, one `pnpm dev:full` launch target can be represented by a parent process plus Vite and API child processes. Its metadata names the primary `web` URL and direct `api` URL while generic listener discovery verifies the process tree's ports.

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
runtime_file = "logs/dev-instance.json"
log_file = "logs/dev.log"

[[launch_targets]]
id = "dev-worker"
label = "Worker"
argv = ["pnpm", "dev:worker"]
process_match = ["scripts/dev_worker.py"]
```

`argv` is an argument vector, not a shell command. Portboard runs it from the selected worktree root.

`process_match` identifies a manually started or already-running process. Every value must match one argument exactly or as a path suffix, and the process cwd must be inside the current worktree. Matching arguments individually prevents a shell command that merely mentions a target signature from being mistaken for the target. When `process_match` is omitted, every `argv` value is used as the signature. Choose a signature that remains present in the long-lived process argument vector; this is what prevents duplicate starts.

`runtime_file` is an optional relative path inside the worktree. Portboard accepts its contents only when its `pid` is one of the target's revalidated matching processes. The project writes this control-plane JSON:

```json
{
  "version": 1,
  "targetId": "dev-full",
  "pid": 1201368,
  "startedAt": "2026-08-21T12:37:22.695Z",
  "endpoints": [
    { "id": "web", "url": "http://localhost:14715", "primary": true },
    { "id": "api", "url": "http://localhost:11100", "primary": false }
  ]
}
```

`log_file` is an optional project-owned log path relative to the worktree. `portboard logs` reads that file directly, so it works for Herdr-hosted runs and survives Portboard process restarts.

Unknown manifest or runtime fields, duplicate IDs, blank values, unsafe relative paths, invalid URLs, and control characters in labels are rejected instead of being silently ignored.

Portboard uses an advisory per-worktree/target launch lock and records the launched root PID plus its Linux start time. Until the configured signature becomes visible or that exact launcher exits, another Portboard process reports the target as starting instead of launching a duplicate. This covers delayed startup across CLI processes and multiple dashboard servers without confusing a reused numeric PID for the original launcher.

Duplicates are surfaced, never merged into a healthy state. When more than one live process matches a target's signature, text status reports `running · DUPLICATE (N instances)`, JSON status sets `"duplicate": true`, the popup shows `duplicate!`, and the dashboard highlights the row in red; `portboard open` refuses to focus or start anything and lists every matching PID with its argument vector. A process whose command line matches two launch targets is reported as a warning in status output, the popup header, and the dashboard API so overlapping `process_match` signatures get fixed in the manifest.

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

Text status prefers named endpoint URLs and falls back to discovered listener addresses. JSON status includes the target argument vector, revalidated processes, raw `endpoints`, and browser-ready `named_endpoints`.

List every process running inside the current worktree, with or without a manifest:

```bash
portboard ps
portboard ps --json
```

Each row shows the PID, the launch target that claims it (`—` when none), every listening TCP port attributed to that process (`—` when it has none), and the command line. This answers "what is running here and what ports does it hold" even in repositories without `portboard.toml`, including processes that never listen on a port. Add a manifest to manage those processes as launch targets; the Herdr popup lists the same process inventory when no manifest exists yet.

Ensure an agent-visible launch target without changing the current pane's focus:

```bash
portboard ensure dev-full --herdr --wait
portboard url dev-full
portboard url dev-full api
portboard url dev-full --open
portboard logs dev-full --lines 200
portboard logs dev-full --follow
portboard stop dev-full
```

`ensure --herdr` requires a current Herdr workspace and a Portboard-owned target process. It creates a dedicated tab when stopped, refuses a matching background or manually launched process, and never falls back to detached execution. `--wait` waits up to 30 seconds for the matching process and the primary HTTP endpoint to return a 2xx or 3xx response. Even without `--wait`, when another start holds an active launch reservation, `ensure` waits up to 30 seconds for that process signature to appear and fails if it never does. The command prints the primary URL after readiness.

`logs` dispatches by run host. A target that declares `log_file` is read directly from that file, so it works for Herdr-hosted runs and survives Portboard process restarts. Otherwise a Herdr-hosted run is focused in its owning tab, a dashboard-owned run is tailed from the newest log below the Portboard state directory (even after the run exits, so crashes stay inspectable), and a manually started run reports its PIDs with a note that its output is attached to the starting terminal. The last 200 lines print by default; `--lines N` changes the bound and `--follow` keeps streaming new output.

`stop` revalidates matching PIDs before sending `SIGTERM`, waits up to five seconds for them to exit, then escalates to `SIGKILL` for survivors so a hung process cannot linger as `running`. When the run has Portboard identity and belongs to the current Herdr workspace, Portboard waits for shutdown and closes its dedicated tab.

`url` prints the primary endpoint URL, or a named endpoint when given. On a terminal it emits the URL as an OSC 8 hyperlink, so ctrl-clicking the output opens it. `--open` additionally launches a browser through `xdg-open` when Portboard runs on a local desktop. Over SSH — including panes on a remote Herdr server reached with `herdr --remote` — Portboard never starts a browser on the server; it prints the clickable URL, and ctrl-clicking is handled by the local Herdr client, which opens it in your local browser.

Open and focus a launch target for interactive use:

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

The dashboard polls `GET /api/status` and can start and stop targets. State-changing requests require the per-server `X-Portboard-Token` embedded in the dashboard page. Commands started by the dashboard are detached into their own process groups; stdout and stderr are written to timestamped per-worktree logs below the Portboard state directory. Each start creates a new `{target}.{timestamp}.log` file, a stable `{target}.log` symlink always points at the newest run, and older logs are pruned to the newest five so previous runs remain inspectable after a crash or restart. Stop sends `SIGTERM` to the complete dashboard-owned process group. For a manually started matching target, stop sends `SIGTERM` to each revalidated matching PID and escalates to `SIGKILL` after the grace period.

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

The plugin also registers a link handler for loopback URLs: ctrl-clicking any `http://localhost`, `127.0.0.1`, or `[::1]` URL in any Herdr pane routes it to the `open-url` action, which opens it in a browser on a local desktop and defers to Herdr's client-side ctrl-click over SSH. No keybinding is needed for this; it comes from the plugin manifest.

### Open-or-start behavior

Selecting a launch target performs the human-oriented `open` operation with context-sensitive behavior:

1. If a matching process is owned by a pane in the current Herdr workspace, Portboard focuses that tab.
2. If it is running outside the current Herdr workspace, Portboard prints inspection details and does not start a duplicate.
3. If it is stopped, Portboard creates a dedicated Herdr tab in the current workspace, starts the command there, and focuses it.

Runs started through the Herdr adapter survive laptop disconnects because the remote Herdr server owns their terminals. Coding agents use `ensure --herdr --wait`, which creates the same dedicated tab but deliberately leaves their working pane focused.

### The popup panel

`portboard panel` renders a small terminal UI (ratatui) in the Herdr popup pane instead of an fzf picker, giving each target's endpoints untruncated room of their own:

- The **left column** lists launch targets with status (`running` / `stopped` / `duplicate!`) and PIDs, above a **ports** list of every TCP listener discovered in the worktree — whether or not a manifest claims its process. Each port row shows its URL, owning PID, and claiming target when one exists.
- The **right column** details the selected target — state, argument vector, and an **endpoints** list where every named or discovered URL is its own row.
- **`enter`** opens or starts the selected target (the `open` behavior above) and closes the panel; on a port row it opens that URL.
- **`o`** opens the selected endpoint or port URL (or the target's primary URL) with `xdg-open` on a local desktop; **`y`** copies it to the clipboard via OSC 52; **`s`** stops the selected target; **`r`** refreshes now (status also polls every two seconds); **`tab`** / **←** / **→** cycles focus between the targets, ports, and endpoints lists; **`q`** closes the panel and **`esc`** steps focus back one list first.
- **Click** a target row to select it, or click an endpoint or port row to open that URL directly.
- Endpoint URLs are shown as plain text, so Herdr's client-side ctrl-click opens them and terminal text selection can copy them. This still works through `herdr --remote`: the click is handled by the local Herdr client, so the URL opens in your local browser even though the run lives on the remote server. Keep in mind the URL still says `localhost`, so the port must be reachable locally (for example through an SSH tunnel) for the page to load.

Without a `portboard.toml`, the panel lists the live process inventory in the worktree instead of launch targets, with each process's listening ports inline. The worktree-wide ports list is always visible either way: Portboard shows every listener it can attribute to a worktree process even when nothing is declared in a manifest.

The popup is backed by the same current-worktree configuration, process discovery, endpoint discovery, duplicate-prevention, and stop code as the standalone interfaces. It does not add a worktree picker or global process browser.

## Process and endpoint discovery

Portboard's current Linux discovery path is deliberately small and deterministic:

1. Resolve and canonicalize the current Git worktree.
2. Scan numeric `/proc` entries for command-line signatures whose cwd is inside that worktree.
3. Re-read each process start time before accepting it, preventing stale PID reuse from becoming a live run.
4. Accept configured runtime metadata only when its owner PID is a current matching process.
5. Walk matching process descendants, map their socket file descriptors to `/proc/net/tcp` and `/proc/net/tcp6`, and report listening TCP addresses.
6. For `ensure --wait`, request the primary HTTP endpoint until it returns a 2xx or 3xx response.

The same discovery powers the worktree-wide ports view: every inventoried worktree process gets one grouped `/proc/net/tcp` scan that attributes each listening socket to the process whose tree owns it, so undeclared dev servers show up next to configured ones.

### Clicked localhost links

The Herdr plugin registers a link handler for loopback URLs (`localhost`, `127.0.0.1`, `[::1]`). Ctrl-clicking any such URL in any Herdr pane invokes `portboard open-url`, which opens it with `xdg-open` on a local desktop and defers to Herdr's client-side ctrl-click over SSH. The command revalidates that the URL is http(s) before opening, so it is safe to invoke by hand as well.

A process that exits during inspection is discarded. Generic endpoint discovery is limited to the current Linux network namespace and TCP listeners visible through procfs. Portboard does not currently inspect Docker/Compose metadata, manage Tailscale Serve, or expose raw TCP services.

## Architecture

The implemented adapters share one Rust core:

```text
Portboard core
├── current-worktree resolution
├── strict launch-target configuration
├── launch locking
├── /proc process revalidation
├── named runtime metadata and HTTP readiness
├── descendant TCP listener discovery
└── ensure, start, inspect, and stop operations

Run hosts
├── Herdr tabs and panes
├── standalone attached processes
└── dashboard-owned detached process groups

Presentations
├── command line and JSON status
├── terminal UI popup (ratatui)
└── loopback web dashboard and JSON API
```

## Requirements

- Linux with procfs
- Git
- Rust 1.87 or newer when building from source
- Optional: Herdr 0.7.4 or newer

## Alternatives

- [herdr-vitals](https://github.com/ericcparsons/herdr-vitals)
- [herdr-switchboard](https://github.com/crafts69guy/herdr-switchboard)
- [herdr-portfwd](https://github.com/miko-misa/herdr-portfwd)
- [herdr-browser](https://github.com/ogulcancelik/herdr-browser)
- [herdr-devup](https://github.com/alon-z/herdr-devup)

## License

MIT
