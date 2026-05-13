#!/bin/sh
# ade-claude-hook.sh — written by `ade install-hooks` to
# ~/.cache/ade/ade-claude-hook.sh on every install. Invoked by the
# Claude Code hooks ADE registers in ~/.claude/settings.json.
#
# Reads the Claude Code hook stdin JSON, optionally extracts context-window
# usage data from the transcript, and writes a per-pane status file at
# ~/.cache/ade/claude-status/<TMUX_PANE>.json that ADE polls every refresh.
#
# Two correctness contracts the v3.1 revision pins:
#
#   1. **Model alias is sticky.** The `[1m]` suffix on a 1M-context model
#      only appears in `SessionStart` hook stdin — UserPromptSubmit / Stop
#      / Notification stdin does NOT carry `model`, and the transcript
#      records only the bare alias. So once SessionStart has captured the
#      suffix, subsequent hooks fall back to the existing file's `model`
#      rather than overwriting it with the bare transcript value.
#
#   2. **Stale hooks cannot clobber newer state.** `seq` is captured at
#      hook-entry time (before the transcript scan), and the final `mv`
#      is gated on a fresh read of the existing file's seq — if a later
#      event's hook has already landed a higher seq while we were
#      scanning, we abandon our write. Without this, a slow
#      UserPromptSubmit scan racing a fast Stop could leave the chip
#      stuck on `working` until the next event or TTL.
#
# All stdout/stderr is suppressed: Claude Code may inject hook stdout from
# SessionStart / UserPromptSubmit as additional context for the model
# (https://code.claude.com/docs/en/hooks), so any noise here would
# silently pollute the user's session.
exec >/dev/null 2>&1

STATE="$1"
case "$STATE" in
    working|idle|awaiting_approval) ;;
    *) exit 0 ;;
esac

PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -n "$PANE" ] || exit 0

DIR="$HOME/.cache/ade/claude-status"
mkdir -p "$DIR" || exit 0
CURRENT_FILE="$DIR/$PANE.json"

# ── Step 1: capture SEQ before any expensive I/O ──────────────────────────
# SEQ reflects HOOK ENTRY TIME, not completion time. Linux `date` supports
# %N (nanoseconds); BSD `date` doesn't and emits the literal "N", which we
# detect and fall back to seconds-since-epoch plus the shell pid. The
# consumer treats the value as an opaque ordering token.
SEQ=$(date +%s%N 2>/dev/null)
case "$SEQ" in
    *N|"") SEQ="$(date +%s 2>/dev/null)$$" ;;
esac

# Compare two non-negative integer strings ($1 = candidate, $2 = ours)
# via awk: returns 0 (true) if candidate > ours. We avoid POSIX `test
# -gt` here because nanosecond-epoch values (~10^18) overflow 32-bit
# integer arithmetic on shells like dash, and we want this to work on
# any host where Claude Code runs.
seq_greater() {
    awk -v a="$1" -v b="$2" '
        BEGIN {
            if (a == "") exit 1;
            if (length(a) != length(b)) {
                exit (length(a) > length(b)) ? 0 : 1;
            }
            exit (a > b) ? 0 : 1;
        }
    '
}

# Pull a JSON string field out of a one-liner JSON document. Used for
# both the hook stdin and the existing status file. Not fully JSON-safe
# (escapes, backslashes) but every field we touch — paths, model
# aliases, UUIDs — lacks those in practice. Parse failure prints
# nothing.
extract_string_from() {
    # $1 = JSON content, $2 = field name
    printf '%s' "$1" | awk -v field="$2" '
        {
            pat = "\"" field "\"[[:space:]]*:[[:space:]]*\"[^\"]*\"";
            if (match($0, pat)) {
                s = substr($0, RSTART, RLENGTH);
                sub("^.*:[[:space:]]*\"", "", s);
                sub("\"$", "", s);
                print s; exit;
            }
        }
    '
}

# Pull a JSON integer field out of a one-liner. Numbers aren't quoted.
extract_int_from() {
    printf '%s' "$1" | awk -v field="$2" '
        {
            pat = "\"" field "\"[[:space:]]*:[[:space:]]*[0-9]+";
            if (match($0, pat)) {
                s = substr($0, RSTART, RLENGTH);
                sub("^.*:[[:space:]]*", "", s);
                print s; exit;
            }
        }
    '
}

# ── Step 2: read existing file to inherit sticky fields ───────────────────
# Read ONCE up front to extract the previously-persisted model (so the
# `[1m]` alias once written by SessionStart survives every Stop /
# UserPromptSubmit firing) and session_id (defensive). seq is also
# fetched but we'll re-fetch it right before the final mv — see step 6.
EXISTING=""
EXISTING_MODEL=""
EXISTING_SESSION_ID=""
if [ -r "$CURRENT_FILE" ]; then
    EXISTING=$(cat "$CURRENT_FILE" 2>/dev/null)
    EXISTING_MODEL=$(extract_string_from "$EXISTING" model)
    EXISTING_SESSION_ID=$(extract_string_from "$EXISTING" session_id)
fi

# ── Step 3: slurp hook stdin ──────────────────────────────────────────────
INPUT=$(cat 2>/dev/null)

TRANSCRIPT=$(extract_string_from "$INPUT" transcript_path)
MODEL_FROM_HOOK=$(extract_string_from "$INPUT" model)
SESSION_FROM_HOOK=$(extract_string_from "$INPUT" session_id)

# ── Step 4: walk the transcript for the latest assistant turn's usage ─────
CTX_TOKENS=""
MODEL_FROM_TX=""
if [ -n "$TRANSCRIPT" ] && [ -r "$TRANSCRIPT" ]; then
    USAGE_LINE=$(awk '/"type":"assistant"/ && /"usage"/ { last = $0 } END { if (last) print last }' "$TRANSCRIPT")
    if [ -n "$USAGE_LINE" ]; then
        IT=$(extract_int_from "$USAGE_LINE" input_tokens)
        CC=$(extract_int_from "$USAGE_LINE" cache_creation_input_tokens)
        CR=$(extract_int_from "$USAGE_LINE" cache_read_input_tokens)
        CTX_TOKENS=$(( ${IT:-0} + ${CC:-0} + ${CR:-0} ))
        MODEL_FROM_TX=$(extract_string_from "$USAGE_LINE" model)
    fi
fi

# ── Step 5: resolve final MODEL with sticky-alias precedence ──────────────
# Order: SessionStart hook (only place [1m] appears) > existing file's
# persisted model > transcript bare alias. This is the fix for the v3.0
# regression where every non-SessionStart hook would overwrite the [1m]
# suffix with the bare alias from the transcript.
MODEL="$MODEL_FROM_HOOK"
[ -z "$MODEL" ] && MODEL="$EXISTING_MODEL"
[ -z "$MODEL" ] && MODEL="$MODEL_FROM_TX"

SESSION_ID="$SESSION_FROM_HOOK"
[ -z "$SESSION_ID" ] && SESSION_ID="$EXISTING_SESSION_ID"

AT=$(date -u +%FT%TZ 2>/dev/null)

# ── Step 6: stage write, then last-second seq-race check ──────────────────
# Build the payload first so the check window between "is our seq still
# the newest?" and the `mv` stays as small as possible. Same-directory
# rename is atomic on POSIX local filesystems so ADE's `cat` loop never
# sees a partial file.
TMP="$DIR/$PANE.json.tmp.$$"
{
    printf '{"state":"%s","at":"%s","seq":%s' "$STATE" "$AT" "$SEQ"
    if [ -n "$CTX_TOKENS" ] && [ "$CTX_TOKENS" -gt 0 ] 2>/dev/null && [ -n "$MODEL" ] && [ -n "$SESSION_ID" ]; then
        printf ',"ctx_tokens":%s,"model":"%s","session_id":"%s"' "$CTX_TOKENS" "$MODEL" "$SESSION_ID"
    fi
    printf '}'
} > "$TMP" 2>/dev/null || exit 0

# Fresh re-read: between step 2 and now we did the slow transcript scan,
# during which a faster concurrent hook may have already landed a write
# with a higher seq. If so, abandon ours rather than clobber theirs.
FRESH_SEQ=""
if [ -r "$CURRENT_FILE" ]; then
    FRESH_SEQ=$(extract_int_from "$(cat "$CURRENT_FILE" 2>/dev/null)" seq)
fi
if seq_greater "$FRESH_SEQ" "$SEQ"; then
    rm -f "$TMP" 2>/dev/null
    exit 0
fi

mv "$TMP" "$CURRENT_FILE" 2>/dev/null || rm -f "$TMP" 2>/dev/null
