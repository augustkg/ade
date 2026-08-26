use super::{map_claude_states, parse_pane_line, parse_session_line, Session, TmuxBackend};
use crate::claude_status;
use crate::hosts::Host;
use std::process::Command;

pub struct RemoteTmux {
    pub host: Host,
}

/// Result of one remote refresh: sessions plus a separate signal of whether
/// the ADE hooks are installed on that host (used by the Hosts UI).
#[derive(Debug, Clone)]
pub struct RemoteRefresh {
    pub sessions: Vec<Session>,
    pub hooks_installed: Option<bool>,
    /// True only when the remote `tmux list-sessions` positively succeeded
    /// (exit 0, or the no-server-running case which is a confirmed-empty
    /// list). False when tmux is missing/broken on the host — the SSH
    /// probe itself still exits 0 in that case, so without this flag a
    /// broken remote tmux would read as "observed: zero sessions" and let
    /// `App::reconcile_kanban` prune every placement on that host.
    pub sessions_observed: bool,
}

const SSH_OPTS: &[&str] = &[
    "-o",
    "ConnectTimeout=2",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// Combined query: sessions, then panes (with pane_id + pane_pid), then a
/// `ps` snapshot for descendant-walk Claude detection, then per-pane status
/// files, then the hooks-installed marker check — each section separated by
/// a sentinel. One SSH round-trip total.
const PANE_SENTINEL: &str = "---ADE-PANES---";
const PS_SENTINEL: &str = "---ADE-PS---";
const STATUS_SENTINEL: &str = "---ADE-STATUS---";
const HOOKS_SENTINEL: &str = "---ADE-HOOKS---";

const LS_OK_SENTINEL: &str = "---ADE-LS-OK---";
const LS_FAIL_SENTINEL: &str = "---ADE-LS-FAIL---";

const REMOTE_LIST_CMD: &str = concat!(
    // list-sessions runs first with an explicit disposition marker: OK for
    // exit 0 or the no-server case (confirmed empty), FAIL for anything
    // else (tmux missing → exit 127 "command not found", socket permission
    // error, …). The probe's own exit code can't carry this — it's the
    // exit of the final `if`. The no-server predicate mirrors
    // `tmux::is_no_server_error`: `no server running`, or `error
    // connecting to` TOGETHER WITH `No such file or directory` —
    // `error connecting to` alone also covers Permission denied /
    // Connection refused, which are real failures.
    "ls_out=$(tmux list-sessions -F '#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_id}\t#{session_activity}' 2>&1); ls_code=$?; ",
    "if [ \"$ls_code\" -eq 0 ]; then echo '---ADE-LS-OK---'; printf '%s\\n' \"$ls_out\"; ",
    "elif printf '%s' \"$ls_out\" | grep -q 'no server running'; then echo '---ADE-LS-OK---'; ",
    "elif printf '%s' \"$ls_out\" | grep -q 'error connecting to' && printf '%s' \"$ls_out\" | grep -q 'No such file or directory'; then echo '---ADE-LS-OK---'; ",
    "else echo '---ADE-LS-FAIL---'; fi; ",
    "echo '---ADE-PANES---'; ",
    "tmux list-panes -a -F '#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}\t#{session_id}' 2>/dev/null; ",
    "echo '---ADE-PS---'; ",
    "ps -A -o pid,ppid,comm 2>/dev/null; ",
    "echo '---ADE-STATUS---'; ",
    "for f in \"$HOME\"/.cache/ade/claude-status/*.json; do ",
    "  [ -f \"$f\" ] || continue; ",
    "  printf '%s\\n' \"$(basename \"$f\" .json)\"; ",
    "  cat \"$f\"; ",
    "  printf '\\n---ADE-STATUS-END---\\n'; ",
    "done; ",
    "echo '---ADE-HOOKS---'; ",
    // Hook marker check: report installed when the CURRENT marker
    // (`ade-status-marker-v3`) is present. Older v2 installs report
    // MISSING so the Hosts UI nudges the user to re-run
    // `ade install-hooks` — that's what ships the v3 hook script and
    // wires the new context-window plumbing.
    "if grep -q ade-status-marker-v3 \"$HOME\"/.claude/settings.json 2>/dev/null; then echo OK; else echo MISSING; fi"
);

/// Split the sessions block of the combined probe output into
/// `(list_sessions_succeeded, session_lines)` based on the disposition
/// sentinel the remote script prepends. The sentinel must be the entire
/// FIRST line — an unanchored substring match could be spoofed by
/// session names or shell banners that happen to contain the marker
/// text. A missing/mismatched first line (unexpected remote shell)
/// parses whatever is there for display but reports not-observed — the
/// safe default for placement pruning.
fn split_ls_disposition(block: &str) -> (bool, &str) {
    let mut parts = block.splitn(2, '\n');
    let first = parts.next().unwrap_or("").trim_end_matches('\r');
    let rest = parts.next().unwrap_or("");
    match first {
        LS_OK_SENTINEL => (true, rest),
        LS_FAIL_SENTINEL => (false, ""),
        _ => (false, block),
    }
}

fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/'))
}

/// Make `tmux` findable on the far side.
///
/// Remote commands run through the login shell NON-interactively. On macOS that
/// means zsh reads `.zshenv` but not `.zshrc`, so PATH is the bare system
/// default and Homebrew's `/opt/homebrew/bin` (Apple silicon) or
/// `/usr/local/bin` (Intel) is missing — `tmux` is simply not found. Linux
/// hosts keep tmux in `/usr/bin`, which is why this never surfaced while every
/// remote ADE drove was Linux.
///
/// `export` rather than a one-shot `VAR=x cmd` prefix, because some remote
/// commands are compound scripts and a prefix would only apply to the first.
///
/// Prepending to PATH rather than wrapping in `zsh -lc "..."` is deliberate: a
/// login-shell wrapper adds another quoting layer around everything it carries,
/// and keeping the message body out of the command string is the whole basis of
/// the injection safety in `send_text`.
pub(crate) fn with_tmux_path(remote_cmd: &str) -> String {
    format!(
        "export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin\"; {}",
        remote_cmd
    )
}

impl RemoteTmux {
    fn ssh(&self, remote_cmd: &str) -> Result<std::process::Output, String> {
        let mut cmd = Command::new("ssh");
        cmd.args(SSH_OPTS);
        // User-supplied ssh args (e.g. ["-p", "22"]) are applied to all
        // non-interactive ssh invocations regardless of host kind.
        for a in &self.host.ssh_args {
            cmd.arg(a);
        }
        cmd.arg(&self.host.target);
        cmd.arg(with_tmux_path(remote_cmd));
        cmd.output()
            .map_err(|e| format!("ssh failed to launch: {}", e))
    }

    /// Run the combined query and return both the session list and the hook
    /// marker check result. This is the entry point the App refresh fan-out
    /// actually uses; the trait method `list_sessions` just calls it and
    /// drops the hook info.
    pub fn refresh(&self) -> Result<RemoteRefresh, String> {
        let out = self.ssh(REMOTE_LIST_CMD)?;

        match out.status.code() {
            Some(255) | None => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                Err(if stderr.is_empty() {
                    format!("{} unreachable", self.host.name)
                } else {
                    stderr.lines().next().unwrap_or("unreachable").to_string()
                })
            }
            _ => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let (sessions_block, rest) = stdout
                    .split_once(PANE_SENTINEL)
                    .unwrap_or((stdout.as_ref(), ""));
                let (sessions_observed, sessions_part) =
                    split_ls_disposition(sessions_block);
                let (panes_part, rest) = rest
                    .split_once(PS_SENTINEL)
                    .unwrap_or((rest, ""));
                let (ps_part, rest) = rest
                    .split_once(STATUS_SENTINEL)
                    .unwrap_or((rest, ""));
                let (status_part, hooks_part) = rest
                    .split_once(HOOKS_SENTINEL)
                    .unwrap_or((rest, ""));

                let mut sessions: Vec<Session> = sessions_part
                    .lines()
                    .filter_map(parse_session_line)
                    .collect();
                let statuses = claude_status::parse_remote_statuses(status_part);
                let pane_pids: Vec<u32> = panes_part
                    .lines()
                    .filter_map(parse_pane_line)
                    .map(|(_, _, _, pid, _)| pid)
                    .collect();
                let claude_pane_pids =
                    claude_status::find_claude_pane_pids(&pane_pids, ps_part);
                let claude_by_session =
                    map_claude_states(panes_part, &statuses, &claude_pane_pids);
                for s in &mut sessions {
                    if let Some(rollup) = claude_by_session.get(&s.name) {
                        s.claude = rollup.state;
                        s.claude_demoted = rollup.demoted;
                        s.claude_present = rollup.present;
                        s.claude_context_pct = rollup.context_pct;
                        s.claude_usage = rollup.context_usage.clone();
                        // Remote readings carry no file mtime, so this is
                        // derived from the status file's `seq`
                        // (`claude_status::seq_to_activity`) — lets remote
                        // folders float by recency in `Tree::group`, same as
                        // local ones. `None` (alphabetical fallback) only when
                        // no pane had a usable `seq`.
                        s.claude_last_activity = rollup.last_activity;
                    }
                }

                let hooks_installed = match hooks_part.trim() {
                    "OK" => Some(true),
                    "MISSING" => Some(false),
                    "" => None,
                    _ => None,
                };

                Ok(RemoteRefresh {
                    sessions,
                    hooks_installed,
                    sessions_observed,
                })
            }
        }
    }
}

impl TmuxBackend for RemoteTmux {
    fn list_sessions(&self) -> Result<Vec<Session>, String> {
        self.refresh().map(|r| r.sessions)
    }

    fn create_session(&self, name: &str) -> Result<(), String> {
        if !shell_safe(name) {
            return Err("invalid session name".to_string());
        }
        let cmd = format!("tmux new-session -d -s {}", name);
        self.ssh(&cmd).and_then(check_status)
    }

    fn rename_session(&self, old: &str, new: &str) -> Result<(), String> {
        if !shell_safe(old) || !shell_safe(new) {
            return Err("invalid session name".to_string());
        }
        let cmd = format!("tmux rename-session -t '={}' {}", old, new);
        self.ssh(&cmd).and_then(check_status)
    }

    fn kill_session(&self, name: &str) -> Result<(), String> {
        if !shell_safe(name) {
            return Err("invalid session name".to_string());
        }
        let cmd = format!("tmux kill-session -t '={}'", name);
        self.ssh(&cmd).and_then(check_status)
    }

    fn duplicate_session(
        &self,
        source: &str,
        new_name: &str,
        claude_running: bool,
    ) -> Result<(), String> {
        if !shell_safe(source) || !shell_safe(new_name) {
            crate::duplicate_log::log(&format!(
                "remote.duplicate: rejected by shell_safe (source={:?} new_name={:?})",
                source, new_name
            ));
            return Err("invalid session name".to_string());
        }
        let cmd = self.build_duplicate_cmd(source, new_name, claude_running);
        crate::duplicate_log::log(&format!(
            "remote.duplicate: host={} target={} cmd={}",
            self.host.name, self.host.target, cmd
        ));
        let out = self.ssh(&cmd)?;
        crate::duplicate_log::log(&format!(
            "remote.duplicate: exit={:?} stdout={:?} stderr={:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ));
        check_status(out)
    }

    /// Type `text` into an exact REMOTE pane and submit it.
    ///
    /// The hard part is that the body crosses two parsers — the remote login
    /// shell, then tmux — and it is arbitrary printable text. Quoting it into
    /// the command string is what made this unsafe to attempt before: a body
    /// containing `$(...)`, backticks or quotes would execute on the far side.
    ///
    /// So the body never appears in the command string at all. It travels on
    /// STDIN and the remote command reads it with `"$(cat)"`. Command
    /// substitution output is not re-scanned for metacharacters, and the
    /// expansion is double-quoted, so there is no word splitting either. This
    /// was verified against a body containing `$(echo PWNED)`, backticks,
    /// quotes, globs, pipes and semicolons: the pane received it byte for byte.
    ///
    /// The only thing interpolated is the pane id, which is validated to
    /// `%<digits>` first — tmux's own format, and nothing a shell reacts to.
    fn send_text(
        &self,
        target: &str,
        text: &str,
    ) -> Result<(), super::SendTextError> {
        // Injection-boundary revalidation, exactly as the local backend does:
        // this is where text enters a live pane, and a forged inbox file must
        // never get a control character or ESC through.
        if let Err(e) = crate::mail::validate_injection(text) {
            return Err(super::SendTextError {
                message: e,
                progress: super::SendProgress::NotStarted,
            });
        }
        if !is_pane_id(target) {
            return Err(super::SendTextError {
                message: format!("refusing to send to '{}': not a tmux pane id", target),
                progress: super::SendProgress::NotStarted,
            });
        }

        // TWO separate ssh invocations, mirroring the local backend's two tmux
        // invocations. The reason is the same and it is load-bearing: Claude's
        // TUI only treats a carriage return as *submit* when it arrives as its
        // own pty read. Chaining both into one remote command would let tmux
        // coalesce them into a single write, and the message would sit unsent
        // in the composer. The extra round trip is the price of it submitting.
        let body_cmd = with_tmux_path(&format!(
            "tmux send-keys -t '{}' -l -- \"$(cat)\"",
            target
        ));
        if let Err(e) = crate::ssh_io::run_with_stdin(&self.host, &body_cmd, text.as_bytes()) {
            return Err(super::SendTextError {
                message: format!("remote send-keys (body) failed: {}", e),
                progress: super::SendProgress::NotStarted,
            });
        }

        // From here the body IS in the recipient's composer, so a failure is a
        // PARTIAL send: the caller must not retry or the body is typed twice.
        let enter_cmd = with_tmux_path(&format!("tmux send-keys -t '{}' Enter", target));
        if let Err(e) = crate::ssh_io::run(&self.host, &enter_cmd) {
            return Err(super::SendTextError {
                message: format!("remote send-keys (Enter) failed: {}", e),
                progress: super::SendProgress::MayHaveTyped,
            });
        }
        Ok(())
    }
}

/// A tmux pane id: `%` followed by digits. Deliberately strict — this is the
/// only caller-influenced value interpolated into a remote command string.
pub(crate) fn is_pane_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('%') else { return false };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

impl RemoteTmux {
    /// Assemble the shell script the remote backend would run for a
    /// duplicate. Extracted so unit tests can pin the substitution
    /// without round-tripping through SSH. Callers must ensure `source`
    /// and `new_name` are `shell_safe` — this function does not
    /// re-validate.
    pub(crate) fn build_duplicate_cmd(
        &self,
        source: &str,
        new_name: &str,
        claude_running: bool,
    ) -> String {
        // `CLAUDE` is sourced from the App layer's `Session.claude_present`,
        // which the refresh loop populated using the same descendant-PID walk
        // we use locally — so detection is symmetric across machines. We
        // use `claude_present` (any Claude pane) rather than `claude.is_some()`
        // (active state only) so idle Claude sessions still get forked.
        let claude_flag = if claude_running { 1 } else { 0 };
        // Trailing colon on `=name` makes the target resolve to a
        // pane, not a session — required for `#{{pane_current_path}}`
        // to be populated. Same gotcha as `capture_pane`.
        format!(
            "SRC='={src}:'; NEW='{new}'; CLAUDE={cl_var}; \
             CWD=$(tmux display-message -t \"$SRC\" -p '#{{pane_current_path}}' 2>/dev/null) || exit 1; \
             [ -n \"$CWD\" ] || {{ echo 'source pane has no cwd' >&2; exit 1; }}; \
             SID=''; \
             if [ \"$CLAUDE\" -eq 1 ]; then \
               ENC=$(printf '%s' \"$CWD\" | sed 's|/|-|g'); \
               PROJ=\"$HOME/.claude/projects/$ENC\"; \
               if [ -d \"$PROJ\" ]; then \
                 __OLDIFS=\"$IFS\"; IFS='\n'; \
                 for f in $(ls -1t \"$PROJ\"/*.jsonl 2>/dev/null); do \
                   [ -f \"$f\" ] || continue; \
                   base=\"${{f##*/}}\"; \
                   stem=\"${{base%.jsonl}}\"; \
                   case \"$stem\" in \
                     [0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]) \
                       SID=\"$stem\"; break ;; \
                   esac; \
                 done; \
                 IFS=\"$__OLDIFS\"; \
               fi; \
             fi; \
             if [ -n \"$SID\" ]; then \
               INNER=\"claude --resume $SID --fork-session\"; \
             elif [ \"$CLAUDE\" -eq 1 ]; then \
               INNER=\"claude\"; \
             else \
               INNER=\"\"; \
             fi; \
             if [ -n \"$INNER\" ]; then \
               CMD=\"bash -lc '$INNER'\"; \
               tmux new-session -d -s \"$NEW\" -c \"$CWD\" \"$CMD\"; \
             else \
               tmux new-session -d -s \"$NEW\" -c \"$CWD\"; \
             fi",
            src = source,
            new = new_name,
            cl_var = claude_flag,
        )
    }

    /// Capture the active pane of a remote session with ANSI escapes,
    /// for the ambient preview pane. One SSH round-trip; expected to be
    /// called at most every few hundred ms per host.
    ///
    /// Target is `=name:` (trailing colon required) — `capture-pane`
    /// resolves a pane target, and `=name` without the colon trips
    /// tmux's parser into "can't find pane" rather than matching the
    /// session. The colon means "session=name exactly, default window
    /// and pane".
    pub fn capture_pane(&self, name: &str) -> Result<String, String> {
        if !shell_safe(name) {
            return Err("invalid session name".to_string());
        }
        let cmd = format!("tmux capture-pane -e -p -t '={}:'", name);
        let out = self.ssh(&cmd)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(stderr
                .lines()
                .next()
                .unwrap_or("remote capture-pane failed")
                .to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl RemoteTmux {
    /// Remote twin of `app::TmuxDelivery::live_session`: one query returning
    /// the session's address and every pane across ALL its windows.
    ///
    /// Session scope (`-s`) is as essential here as locally — without it
    /// `list-panes` reports only the active window, so a second window could
    /// hide a shell we would then type into.
    pub fn live_session(&self, name: &str) -> Option<(String, Vec<String>)> {
        if !shell_safe(name) {
            return None;
        }
        let cmd = format!(
            "tmux list-panes -s -t '={}' -F '#{{pid}}:#{{session_id}}\t#{{pane_id}}'",
            name
        );
        let out = self.ssh(&cmd).ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut addr = String::new();
        let mut panes = Vec::new();
        for line in text.lines() {
            let mut cols = line.split('\t');
            let a = cols.next().unwrap_or("").trim();
            let p = cols.next().unwrap_or("").trim();
            if a.is_empty() || p.is_empty() {
                continue;
            }
            if addr.is_empty() {
                addr = a.to_string();
            }
            panes.push(p.to_string());
        }
        if addr.is_empty() || panes.is_empty() {
            return None;
        }
        Some((addr, panes))
    }

    /// Capture one exact remote PANE (not the session's active pane), which is
    /// what composer classification needs.
    pub fn capture_pane_id(&self, pane_id: &str) -> Result<String, String> {
        if !is_pane_id(pane_id) {
            return Err("invalid pane id".to_string());
        }
        let cmd = format!("tmux capture-pane -e -p -t '{}'", pane_id);
        let out = self.ssh(&cmd)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(stderr
                .lines()
                .next()
                .unwrap_or("remote capture-pane failed")
                .to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn check_status(out: std::process::Output) -> Result<(), String> {
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr
        .lines()
        .next()
        .unwrap_or("remote tmux command failed")
        .to_string())
}

#[cfg(test)]
mod ls_disposition_tests {
    //! Pins the observed-vs-failed semantics of the list-sessions
    //! disposition sentinel (Codex implementation review): a broken
    //! remote tmux must NOT read as "observed: zero sessions", or kanban
    //! placement pruning would wipe that host's entries.

    use super::split_ls_disposition;

    #[test]
    fn ok_marker_observes_and_yields_lines() {
        let block = "---ADE-LS-OK---\nwork\t1\t0\t$1\n";
        let (observed, lines) = split_ls_disposition(block);
        assert!(observed);
        assert!(lines.contains("work\t1\t0\t$1"));
    }

    #[test]
    fn ok_marker_with_no_sessions_is_confirmed_empty() {
        let (observed, lines) = split_ls_disposition("---ADE-LS-OK---\n");
        assert!(observed);
        assert!(lines.trim().is_empty());
    }

    #[test]
    fn fail_marker_is_not_observed_and_drops_output() {
        let (observed, lines) =
            split_ls_disposition("---ADE-LS-FAIL---\ngarbage\n");
        assert!(!observed);
        assert!(lines.is_empty());
    }

    #[test]
    fn missing_marker_parses_for_display_but_is_not_observed() {
        let block = "work\t1\t0\t$1\n";
        let (observed, lines) = split_ls_disposition(block);
        assert!(!observed);
        assert_eq!(lines, block);
    }

    #[test]
    fn sentinel_must_be_the_entire_first_line() {
        // A banner or session name containing the marker text must not
        // count as observed — the sentinel is anchored to line 1.
        let spoofed = "motd: ---ADE-LS-OK--- welcome\nwork\t1\t0\t$1\n";
        let (observed, _) = split_ls_disposition(spoofed);
        assert!(!observed);

        // A session literally named like the sentinel appears as
        // `---ADE-LS-OK---\t1\t0\t$1` — a prefix, not the whole line.
        let prefix_only = "---ADE-LS-OK---\t1\t0\t$1\n";
        let (observed, _) = split_ls_disposition(prefix_only);
        assert!(!observed);
    }

    #[test]
    fn crlf_first_line_still_matches() {
        let (observed, lines) = split_ls_disposition("---ADE-LS-OK---\r\nwork\t1\t0\t$1\n");
        assert!(observed);
        assert!(lines.contains("work"));
    }
}
