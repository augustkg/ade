# ADE

**Agentic Development Environment** — a fast, keyboard-driven TUI for managing tmux sessions across local and remote machines.

Browse, create, rename, and attach to tmux sessions on your laptop and any SSH/Mosh-reachable host from a single tree.

## Features

- **Folders** — sessions sharing a `prefix/` (e.g. `work/api`, `work/db`) auto-group under a collapsible `work/` folder. Toggle with `o`/`␣`/Enter; collapsed state persists across launches. Folder-level rename cascades to every child; dissolve strips the prefix from each child without killing them.
- **Tmux clipboard, fixed** — `ade install-tmux-config` ships the canonical tmux config that makes drag-select-to-copy work end-to-end, including through mosh (where the default tmux `Ms` capability silently drops). Idempotent; local and remote.
- **Cross-machine** — local plus every configured SSH/Mosh host in one tree. Manage hosts in-app with `H` or in `~/.config/ade/hosts.toml`.
- **Live Claude status** — sessions running Claude Code show a `claude` chip with the live context-window percentage (e.g. `claude · 72%`), so you can spot a near-compact session on any remote at a glance. Working sessions render bright peach; idle sessions with context data render a dim chip; sessions awaiting a permission prompt render red `claude · approve`. Powered by `ade install-hooks`; detects wrapped/nested Claude via a process-tree walk.
- **Smart attach** — handles the awkward edges: same-session no-op, `switch-client` when launched from inside tmux, exec-replace into ssh/mosh for remote sessions. Just press Enter.
- **Non-blocking refresh** — the tree updates every 2s in the background; manual `r` is instant. SSH per host runs in parallel, bounded by `ConnectTimeout`.

## Build

```sh
cargo build --release
```

Symlink `target/release/ade` into a directory on your `PATH`:

```sh
ln -s "$PWD/target/release/ade" ~/.local/bin/ade
```

## Configure hosts

ADE reads `~/.config/ade/hosts.toml`. Add entries from the in-app `H` view, or by hand:

```toml
[[hosts]]
name = "hetzner-admin"
kind = "ssh"
target = "hetzner-admin"

[[hosts]]
name = "web"
kind = "mosh"
target = "user@web.example.com"
```

`target` is what you'd type after `ssh`/`mosh`; `ssh_args` are the flags that come before it. `~/.ssh/config` aliases work.

## One-time setup (recommended)

Run these once on every machine you want full ADE on. Both are idempotent and reversible.

```sh
ade install-hooks                # live Claude status + context % (local)
ade install-hooks --host H       # same, on a configured remote host
ade install-hooks --all          # local + every host in hosts.toml

ade install-tmux-config          # drag-select-to-copy through mosh (local)
ade install-tmux-config --host H # same, on a remote host
ade install-tmux-config --all    # local + every host in hosts.toml
```

`install-hooks` registers Claude Code hooks under `~/.claude/settings.json`
and ships a small POSIX shell helper to `~/.cache/ade/ade-claude-hook.sh`.
On each Claude turn the helper reads the transcript, extracts the latest
assistant turn's input + cache tokens, and writes a per-pane status file
that ADE polls every 2 s. Re-running is safe and idempotent; upgrades from
older hook versions (v1/v2) auto-migrate without duplicating entries or
touching any user-owned hooks that happen to share an entry.

ADE also surfaces a one-time peach `Tip` banner inside the TUI when it notices the tmux config is missing — dismiss with `x` if you don't want it.

## Keys (Tree view)

| Key | Action |
|---|---|
| `j` / `k` / `↑↓` | Navigate |
| `o` / `␣` | Expand / collapse folder |
| `Enter` | Attach to session (or toggle folder) |
| `n` | New session |
| `R` | Rename session or folder |
| `d` | Delete session or dissolve folder |
| `H` | Hosts list |
| `r` | Refresh |
| `x` | Dismiss the tmux-config nudge |
| `q` / `Esc` | Quit |

## Commands

| Command | Description |
|---|---|
| `ade` | Launch the TUI |
| `ade install-hooks [--host H]` | Install Claude Code status + context-window hooks |
| `ade install-hooks --all` | Install hooks on local + every host in `hosts.toml` |
| `ade install-tmux-config [--host H]` | Install tmux clipboard config |
| `ade install-tmux-config --all` | Install tmux config on local + every host |
| `ade install-tmux-config --uninstall` | Remove the tmux clipboard config |
| `ade debug claude [--host H]` | Diagnose Claude detection per pane (shows `· NN%` per session) |
| `ade help` | Show usage |

## Config files

- `~/.config/ade/hosts.toml` — host list (managed in-app or by hand)
- `~/.config/ade/tmux.conf` — managed tmux clipboard snippet (written by `install-tmux-config`)
- `~/.config/ade/state.toml` — persisted UI prefs (collapsed folders, dismissed nudges)
