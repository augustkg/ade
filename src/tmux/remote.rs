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

const REMOTE_LIST_CMD: &str = concat!(
    "tmux list-sessions -F '#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_id}' 2>/dev/null; ",
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

fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/'))
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
        cmd.arg(remote_cmd);
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
                let (sessions_part, rest) = stdout
                    .split_once(PANE_SENTINEL)
                    .unwrap_or((stdout.as_ref(), ""));
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
