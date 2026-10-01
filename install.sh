#!/usr/bin/env bash
# install.sh — build ADE from this checkout and set it up for the CURRENT user.
#
#   ./install.sh               build, install ~/.local/bin/ade, install hooks + tmux config
#   ./install.sh --no-setup    build and install the binary only
#
# Clone it into your own home first: the build writes target/ inside the checkout.
# Everything else it writes is under $HOME too: the binary, ~/.claude/settings.json hooks
# (`ade install-hooks`) and the tmux snippet (`ade install-tmux-config`). It never
# touches another user's files, never uses sudo, and only configures this machine
# (no --all: remote hosts are yours to opt into later).
#
# On a machine shared by several people, each person runs this in their own
# account. ADE then talks to that account's own tmux server only — see
# "Several users on one machine" in README.md.
set -euo pipefail

SETUP=true
case "${1:-}" in
    "") ;;
    --no-setup) SETUP=false ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
esac

cd "$(dirname "$(readlink -f "$0")")"
BIN_DIR="${ADE_BIN_DIR:-$HOME/.local/bin}"

say()  { printf '== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- prerequisites ------------------------------------------------------------------------
# Prefer the rustup toolchain: a distro cargo can be too old for this lockfile.
CARGO="$(command -v "$HOME/.cargo/bin/cargo" || command -v cargo || true)"
[ -n "$CARGO" ] || die "cargo not found. Install Rust for your user (no sudo needed):
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
then open a new shell (or: . \"\$HOME/.cargo/env\") and re-run $0"

# Cargo.lock is version 4, which cargo reads from 1.78 on.
ver="$("$CARGO" --version | awk '{print $2}')"
IFS=. read -r major minor _ <<<"$ver"
if [ "${major:-0}" -lt 1 ] || { [ "$major" -eq 1 ] && [ "${minor:-0}" -lt 78 ]; }; then
    die "$CARGO is $ver; ADE needs cargo 1.78 or newer (rustup update stable)"
fi

command -v tmux >/dev/null || die "tmux is not installed (ask the machine's admin: apt install tmux)"

# --- build + install ----------------------------------------------------------------------
say "building ADE with $CARGO ($ver)"
"$CARGO" build --locked --release

say "installing $BIN_DIR/ade"
mkdir -p "$BIN_DIR"
# Copy, not symlink: a later `cargo clean` or checkout move must not break the command.
install -m 0755 target/release/ade "$BIN_DIR/ade.tmp"
mv -f "$BIN_DIR/ade.tmp" "$BIN_DIR/ade"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on your PATH. Add to ~/.profile:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if $SETUP; then
    say "ade install-hooks (live Claude status in ADE)"
    "$BIN_DIR/ade" install-hooks
    say "ade install-tmux-config (copy/paste + back-to-ADE keys)"
    "$BIN_DIR/ade" install-tmux-config
fi

# ADE's "new session with Claude" runs `bash -lc 'claude'`, i.e. a LOGIN shell: claude must be
# on the PATH your ~/.profile sets up, not only in ~/.bashrc.
if ! bash -lc 'command -v claude' >/dev/null 2>&1; then
    warn "'claude' is not on your login-shell PATH, so ADE can create sessions but not start
Claude in them. Install Claude Code for your own user and sign in with your own account:
    curl -fsSL https://claude.ai/install.sh | bash
then make sure ~/.local/bin is on the PATH in ~/.profile and run: claude"
fi

say "done — run: ade"
