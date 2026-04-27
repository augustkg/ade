# ADE

Agentic Development Environment — a TUI for managing tmux sessions across local and remote machines.

Browse, create, rename, and attach to tmux sessions on your laptop and any SSH/Mosh-reachable host from a single keyboard-driven view.

## Build

```sh
cargo build --release
```

Symlink `target/release/ade` into a directory on your `PATH`.

## Configure hosts

ADE reads `~/.config/ade/hosts.toml`. Add entries from the in-app `H` view, or by hand:

```toml
[[hosts]]
name = "hetzner-admin"
kind = "ssh"
target = "hetzner-admin"
ssh_args = ["-p", "2226"]

[[hosts]]
name = "hume"
kind = "mosh"
target = "hume@100.112.161.65"
```

`target` is what you'd type after `ssh`/`mosh`; `ssh_args` are the flags that come before it. `~/.ssh/config` aliases work.

## Keys

`j`/`k` navigate · `Enter` attach · `n` new · `R` rename · `d` delete · `H` hosts · `r` refresh · `q` quit

## License

MIT
