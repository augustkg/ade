#!/bin/sh
# ade-claude-hook.sh — written by `ade install-hooks` to
# ~/.cache/ade/ade-claude-hook.sh on every install. Invoked by the
# Claude Code hooks ADE registers in ~/.claude/settings.json.
#
# Reads the Claude Code hook stdin JSON, optionally extracts context-window
# usage data from the transcript, and writes a per-pane status file at
# ~/.cache/ade/claude-status/<TMUX_PANE>.json that ADE polls every refresh.
#
# All stdout/stderr is suppressed: Claude Code may inject hook stdout
# from SessionStart / UserPromptSubmit as additional context for the
# model (https://code.claude.com/docs/en/hooks), so any noise here
# would silently pollute the user's session.
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

# Slurp stdin once. Hook payloads are typically a few hundred bytes; this
# is safe to hold in a shell variable.
INPUT=$(cat 2>/dev/null)

# Pull a JSON string field out of $INPUT by name. Awk regex extraction
# isn't fully JSON-safe (escapes, backslashes), but every field we care
# about — paths, model aliases, UUIDs — lacks those in practice. Parse
# failure is silent and we just skip that field.
extract_string() {
    printf '%s' "$INPUT" | awk -v field="$1" '
        {
            pat = "\"" field "\"[[:space:]]*:[[:space:]]*\"[^\"]*\"";
            if (match($0, pat)) {
                s = substr($0, RSTART, RLENGTH);
                sub("^.*:[[:space:]]*\"", "", s);
                sub("\"$", "", s);
                print s;
                exit;
            }
        }
    '
}

TRANSCRIPT=$(extract_string transcript_path)
MODEL_FROM_HOOK=$(extract_string model)
SESSION_FROM_HOOK=$(extract_string session_id)

# If the transcript is readable, find the latest assistant turn carrying
# a `usage` block and pluck the three input-token fields plus the model
# alias recorded on that turn.
CTX_TOKENS=""
MODEL_FROM_TX=""
if [ -n "$TRANSCRIPT" ] && [ -r "$TRANSCRIPT" ]; then
    USAGE_LINE=$(awk '/"type":"assistant"/ && /"usage"/ { last = $0 } END { if (last) print last }' "$TRANSCRIPT")
    if [ -n "$USAGE_LINE" ]; then
        extract_int() {
            printf '%s' "$USAGE_LINE" | awk -v field="$1" '
                {
                    pat = "\"" field "\"[[:space:]]*:[[:space:]]*[0-9]+";
                    if (match($0, pat)) {
                        s = substr($0, RSTART, RLENGTH);
                        sub("^.*:[[:space:]]*", "", s);
                        print s;
                        exit;
                    }
                }
            '
        }
        IT=$(extract_int input_tokens)
        CC=$(extract_int cache_creation_input_tokens)
        CR=$(extract_int cache_read_input_tokens)
        CTX_TOKENS=$(( ${IT:-0} + ${CC:-0} + ${CR:-0} ))
        MODEL_FROM_TX=$(printf '%s' "$USAGE_LINE" | awk '
            {
                if (match($0, "\"model\"[[:space:]]*:[[:space:]]*\"[^\"]*\"")) {
                    s = substr($0, RSTART, RLENGTH);
                    sub("^.*:[[:space:]]*\"", "", s);
                    sub("\"$", "", s);
                    print s;
                    exit;
                }
            }
        ')
    fi
fi

# Prefer SessionStart's model field over the transcript's. The hook payload
# carries the [1m] suffix faithfully; the transcript records the bare alias
# (claude-opus-4-7), so 1M sessions are only distinguishable from the hook.
MODEL="${MODEL_FROM_HOOK:-$MODEL_FROM_TX}"
SESSION_ID="$SESSION_FROM_HOOK"

# Monotonic-ish seq for race tiebreaking when two concurrent hook firings
# write the same pane's file. Linux `date` supports %N (nanoseconds); BSD
# `date` doesn't — it emits the literal "N", which we detect and fall back
# to seconds-since-epoch plus the shell's pid (good enough as a tiebreak
# token; consumer treats the number as opaque).
SEQ=$(date +%s%N 2>/dev/null)
case "$SEQ" in
    *N|"") SEQ="$(date +%s 2>/dev/null)$$" ;;
esac

AT=$(date -u +%FT%TZ 2>/dev/null)

# Build the JSON payload. Single line, hand-rolled — no jq dependency.
# Usage fields are emitted only when all three (tokens, model, session_id)
# are present, matching the parser's "partial usage → drop the block"
# contract.
TMP="$DIR/$PANE.json.tmp.$$"
{
    printf '{"state":"%s","at":"%s","seq":%s' "$STATE" "$AT" "$SEQ"
    if [ -n "$CTX_TOKENS" ] && [ "$CTX_TOKENS" -gt 0 ] 2>/dev/null && [ -n "$MODEL" ] && [ -n "$SESSION_ID" ]; then
        printf ',"ctx_tokens":%s,"model":"%s","session_id":"%s"' "$CTX_TOKENS" "$MODEL" "$SESSION_ID"
    fi
    printf '}'
} > "$TMP" 2>/dev/null || exit 0

# Same-directory rename is atomic on POSIX local filesystems, so ADE's
# `cat` loop never observes a half-written file.
mv "$TMP" "$DIR/$PANE.json" 2>/dev/null || rm -f "$TMP" 2>/dev/null
