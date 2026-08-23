# ADE

**Agentic Development Environment** — a fast, keyboard-driven TUI for managing tmux sessions across local and remote machines.

Browse, create, rename, and attach to tmux sessions on your laptop and any SSH/Mosh-reachable host from a single tree.

## Features

- **Folders** — sessions sharing a `prefix/` (e.g. `work/api`, `work/db`) auto-group under a collapsible `work/` folder. Toggle with `o`/`␣`/Enter; collapsed state persists across launches. Folder-level rename cascades to every child; `d` opens a 3-way prompt to delete the folder and kill every session inside it (default), or dissolve it (strip the prefix; sessions keep running).
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

`install-tmux-config` also ships two keybindings to jump back to ADE from inside any attached tmux session — see **Tmux keybindings** below.

## Keys (Tree view)

| Key | Action |
|---|---|
| `j` / `k` / `↑↓` | Navigate |
| `o` / `␣` | Expand / collapse folder |
| `Enter` | Attach to session (or toggle folder) |
| `n` | New session |
| `R` | Rename session or folder |
| `d` | Delete session, or on a folder open a confirm with: Enter = delete + kill all sessions, `s` = dissolve (keep sessions), Esc = cancel |
| `K` | Kanban board |
| `H` | Hosts list |
| `p` | Toggle the read-only session preview; drag selected text to copy it |
| `Tab` | Open the focused session interactively in the preview pane |
| `r` | Refresh |
| `x` | Dismiss the tmux-config nudge |
| `q` / `Esc` | Quit |

## Kanban board

Press `K` in the tree to see every session as a card in a workflow column:

| Column (default) | Kind | Meaning |
|---|---|---|
| Idle | manual | Backlog — started work you parked again |
| Awaiting human | auto | Claude Code isn't running in the session, or it's running but waiting on you (idle prompt / permission approval). Default for unplaced sessions. |
| Active | auto | Claude is working right now |
| Done | manual | You moved it here |
| Verified done | manual | You checked the work and moved it here |

The two auto columns are driven by the same hooks that power the `claude`
chips — no manual bookkeeping. The moment Claude starts working in a
session, its card jumps to Active and any manual placement (even Done) is
cleared; when it stops, the card falls to Awaiting human until you place it
again. A working card is pinned — it can't be moved out of Active by hand.

| Key | Action |
|---|---|
| `h` / `l` / `←→` | Focus column |
| `j` / `k` / `↑↓` | Focus card |
| `H` / `L` (or `Shift+←→`) | Move focused card left / right (skips Active; moving into Awaiting human clears the manual placement) |
| `g` / `G` | Top / bottom of column |
| `Enter` | Attach to session |
| `p` / `Tab` | Open the focused card's session in a modal — immediately interactive, type straight into it; `Ctrl+Space Space` returns to the board (same chord as the tree's embedded pane) |
| `f` | Folder filter picker — tick which groups to show: `(no folder)` and/or any folder prefixes (e.g. loose sessions + the `archie/` folder). Nothing ticked = show everything. Persisted; a summary bar appears above the board while active |
| `c` | Clear the folder filter (also available inside the picker) |
| `r` | Refresh |
| `K` / `q` / `Esc` | Back to tree |

### Customizing columns — `~/.config/ade/kanban.toml`

Columns are renamable, reorderable, and extra **manual** columns can be
added. The file is optional; missing means the defaults above. ADE never
rewrites it, so comments survive. The defaults, spelled out:

```toml
[[columns]]
id = "idle"          # stable key — placements reference it; don't change after use
name = "Idle"        # display name — rename freely
kind = "manual"

[[columns]]
id = "awaiting"
name = "Awaiting human"
kind = "auto-awaiting"   # exactly one required

[[columns]]
id = "active"
name = "Active"
kind = "auto-active"     # exactly one required

[[columns]]
id = "done"
name = "Done"
kind = "manual"

[[columns]]
id = "verified"
name = "Verified done"
kind = "manual"
```

Rules: exactly one `auto-awaiting` and one `auto-active` column (renamable
and reorderable, but their hook-bound semantics can't be remapped); any
number of `manual` columns. An invalid file falls back to the defaults
with a warning banner — manual placements referencing your custom columns
are preserved (shown under Awaiting human) until the file is fixed.

Manual placements are stored in `state.toml` keyed by `(host, session
name)`; renaming a session or host inside ADE migrates them, renaming a
session outside ADE resets it to the automatic columns.

### Moving cards from the command line (and from Claude)

`ade kanban move <column>` moves the card of the tmux session it runs
inside — so a Claude Code session can mark itself done when you tell it
to ("mark this as done" → `ade kanban move done`). `--session <name>`
targets another local session; `ade kanban clear` returns a card to the
automatic columns. Columns resolve by id or display name; only manual
columns are valid targets.

The CLI never writes state directly — it drops an intent file in
`~/.cache/ade/kanban-inbox/` which the running ADE consumes within ~2s
(or at the next launch). A user-level Claude Code skill
(`~/.claude/skills/ade-kanban/SKILL.md`) teaches Claude to call this
when you ask. Local sessions only for now.

## Tmux keybindings (installed by `ade install-tmux-config`)

Press these from inside any tmux session that has ADE's tmux config sourced:

| Chord | Action |
|---|---|
| `<prefix> B` | Back to ADE — detaches when ADE attached this session itself, otherwise switches to the pane where ADE is running. |
| `<prefix> Space` | Same as `<prefix> B`. Aliased for discoverability and finger comfort. |

`<prefix> Space` overrides tmux's default `next-layout` binding. If you actively use multiple pane layouts, rebind it in your own `~/.tmux.conf` *after* the `source-file ~/.config/ade/tmux.conf` line.

## Commands

| Command | Description |
|---|---|
| `ade` | Launch the TUI |
| `ade install-hooks [--host H]` | Install Claude Code status + context-window hooks |
| `ade install-hooks --all` | Install hooks on local + every host in `hosts.toml` |
| `ade install-tmux-config [--host H]` | Install tmux clipboard config |
| `ade install-tmux-config --all` | Install tmux config on local + every host |
| `ade install-tmux-config --uninstall` | Remove the tmux clipboard config |
| `ade kanban move <column> [--session S]` | Move a session's kanban card to a manual column (defaults to the tmux session it runs inside) |
| `ade kanban clear [--session S]` | Return a session's card to the automatic columns |
| `ade debug claude [--host H]` | Diagnose Claude detection per pane (shows `· NN%` per session) |
| `ade sessions --json [--local]` | Print the full cross-host session inventory as JSON (stdout) |
| `ade status --json [--local]` | Print a Claude monitor summary (totals, worst context %, sessions needing you) as JSON |
| `ade help` | Show usage |

### JSON output (`--json`)

`ade sessions --json` and `ade status --json` are non-interactive: they reuse the
same cross-host collection the TUI uses (local + every configured host; add
`--local` to skip remote polling), print JSON to **stdout**, send any logs or
per-host errors to **stderr**, and exit 0 on success. They're the stable
contract other tools build on — see the module docs in `src/json_out.rs` for the
full schema.

- `sessions --json` → `{ sessions: [ { machine, name, prefix, leaf, session_id, windows, attached, claude: { state, context_pct, model, ctx_tokens, session_id } | null } ], errors: [ { machine, error } ] }`
- `status --json` → `{ totals: { sessions, working, idle, awaiting_approval }, worst_context_pct, needs_you: [ { machine, name, session_id } ], sessions: [ { machine, name, session_id, state, context_pct } ], errors }`

## Config files

- `~/.config/ade/hosts.toml` — host list (managed in-app or by hand)
- `~/.config/ade/kanban.toml` — kanban column layout (by hand; optional, defaults apply)
- `~/.config/ade/tmux.conf` — managed tmux clipboard snippet (written by `install-tmux-config`)
- `~/.config/ade/state.toml` — persisted UI prefs (collapsed folders, dismissed nudges, kanban placements)
