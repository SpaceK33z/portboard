# Portboard

**Your dev servers, ports, and URLs. One Git worktree at a time.**

Stop guessing which port belongs to which worktree. Portboard helps humans and coding agents find the right dev server, open its URL, and start or stop it without mixing up projects.

Use the CLI, local dashboard, or optional [Herdr](https://herdr.dev) popup.

```text
┌ Portboard · /path/to/repo ────────────┐
│ targets            │ Dev server       │
│ ▸ dev      running │ state running    │
│   worker  stopped  │ argv pnpm dev    │
│                    │ endpoints        │
│                    │ ▸ web http://…   │
├────────────────────┴──────────────────┤
│ enter open/start · o open url · y copy │
│ s stop · r refresh · tab endpoints · q │
└───────────────────────────────────────┘
```

- **Find that port.** List worktree processes and TCP listeners—even without configuration.
- **Skip duplicate starts.** Inspect existing runs; start stopped targets. Conflicting matches are flagged.
- **Keep worktrees separate.** Only see processes belonging to the current worktree.
- **Give agents visible servers.** Start in a dedicated Herdr tab without stealing focus.

## Try it

Requires **Linux with procfs**, **Git**, and **Rust 1.88+** to build. Log reading uses **GNU coreutils `tail`**. Herdr is optional.

```bash
# From this checkout
cargo install --path .

# From any Git worktree—no config needed
portboard ps
```

See every worktree process, its command, and its listening ports.

## Manage your dev stack

Add `portboard.toml` at your repository root. Adapt the command and process signature to your project:

```toml
version = 1

[[launch_targets]]
id = "dev"
label = "Dev server"
argv = ["pnpm", "dev"]
process_match = ["scripts/dev.mjs"]
```

This example expects `pnpm dev` to run `scripts/dev.mjs`. `process_match` must match an argument in the long-lived process.

```bash
portboard open dev    # Start, focus its Herdr tab, or inspect an existing run
portboard status      # Show targets, processes, and endpoints
portboard url dev     # Print the primary browser URL, when available
portboard stop dev    # Stop the target
```

Without Herdr, a newly started command stays attached to your terminal. Use another terminal for status and stop.

Need named `web` / `api` URLs, log files, or multiple targets? See [project configuration](docs/reference.md#project-configuration).

## Pick your interface

### Local dashboard

```bash
portboard serve
```

Open **http://127.0.0.1:9777** to inspect, start, and stop targets. Loopback-only by design.

### Herdr popup

Requires **Herdr 0.7.4+**. From this checkout:

```bash
cargo build --release
herdr plugin link "$PWD"
```

Add to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "portboard.open"
description = "portboard: current workspace"
```

```bash
herdr config check
herdr server reload-config
```

Press **`prefix+p`** in a workspace:

| Key | Action |
| --- | --- |
| `enter` | Open or start a target |
| `o` / `y` | Open / copy a URL |
| `s` | Stop the selected target |
| `tab` | Cycle targets, ports, and endpoints |
| `r` / `q` | Refresh / close |

### Coding agents

```bash
portboard ensure dev --herdr --wait
portboard status --json
```

Create a dedicated server tab without changing focus. `--wait` checks the matching process and primary HTTP endpoint for up to 30 seconds; `ensure --herdr` refuses duplicate, manually launched, or background runs rather than silently starting another. Readiness supports plain HTTP to IP literals or `localhost`; HTTPS and other DNS names remain usable as browser links, but are not supported readiness probes.

## Go deeper

- [Command reference](docs/reference.md#standalone-use)—URLs, logs, JSON output, and stop behavior
- [Configuration](docs/reference.md#project-configuration)—process matching, named endpoints, and duplicate detection
- [Herdr details](docs/reference.md#herdr-integration)—popup controls and remote localhost links
- [Discovery and limits](docs/reference.md#process-and-endpoint-discovery)—Linux process and TCP discovery; not Docker orchestration or general health monitoring
- [Architecture](docs/reference.md#architecture)

## Alternatives

[herdr-vitals](https://github.com/ericcparsons/herdr-vitals) ·
[herdr-switchboard](https://github.com/crafts69guy/herdr-switchboard) ·
[herdr-portfwd](https://github.com/miko-misa/herdr-portfwd) ·
[herdr-browser](https://github.com/ogulcancelik/herdr-browser) ·
[herdr-devup](https://github.com/alon-z/herdr-devup)

## License

[MIT](LICENSE)
