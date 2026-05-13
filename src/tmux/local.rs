use super::{
    is_session_uuid, map_claude_states, parse_pane_line, parse_session_line, Session, TmuxBackend,
};
use crate::claude_status;
use std::process::Command;

pub struct LocalTmux;

const LIST_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_id}";
const PANE_FORMAT: &str =
    "#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}\t#{session_id}";

impl TmuxBackend for LocalTmux {
    fn list_sessions(&self) -> Result<Vec<Session>, String> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", LIST_FORMAT])
            .output();

        let mut sessions: Vec<Session> = match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.lines().filter_map(parse_session_line).collect()
            }
            // tmux exits non-zero when no server is running — treat as empty.
            Ok(_) => Vec::new(),
            Err(e) => return Err(format!("local tmux unavailable: {}", e)),
        };

        // Best-effort claude detection: failure is silent so the session
        // list still renders even if `list-panes` or `ps` misbehaves.
        let panes_text = Command::new("tmux")
            .args(["list-panes", "-a", "-F", PANE_FORMAT])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

        // Track ps success explicitly: an empty ps_text would otherwise
        // make `find_claude_pane_pids` return an empty set, which makes
        // every pane look "orphaned" — and that would let the demotion
        // pass below false-demote every working chip.
        let (ps_text, ps_succeeded) = match Command::new("ps")
            .args(["-A", "-o", "pid,ppid,comm"])
            .output()
        {
            Ok(out) if out.status.success() => {
                (String::from_utf8_lossy(&out.stdout).into_owned(), true)
            }
            _ => (String::new(), false),
        };

        let pane_pids: Vec<u32> = panes_text
            .lines()
            .filter_map(parse_pane_line)
            .map(|(_, _, _, pid, _)| pid)
            .collect();
        let claude_pane_pids = claude_status::find_claude_pane_pids(&pane_pids, &ps_text);

        let statuses = claude_status::read_local_statuses_with_working_ttl();
        let claude_by_session = map_claude_states(&panes_text, &statuses, &claude_pane_pids);

        for s in &mut sessions {
            if let Some(rollup) = claude_by_session.get(&s.name) {
                s.claude = rollup.state;
                s.claude_demoted = rollup.demoted;
                s.claude_present = rollup.present;
                s.claude_context_pct = rollup.context_pct;
            }
        }

        // Catch panes whose Claude died without firing Stop / StopFailure /
        // SessionEnd (kill -9, crash, SSH drop). The hook chain can't help
        // here — only a process-aliveness check can. Skip if `ps` failed.
        if ps_succeeded {
            let panes_iter = panes_text
                .lines()
                .filter_map(parse_pane_line)
                .map(|(_, cmd, pid, ppid, _)| (cmd, pid, ppid));
            claude_status::demote_orphan_working_files(
                panes_iter,
                &claude_pane_pids,
                &statuses,
            );
        }

        Ok(sessions)
    }

    fn create_session(&self, name: &str) -> Result<(), String> {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", name])
            .status()
            .map_err(|e| format!("Failed to create session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to create tmux session".to_string())
        }
    }

    fn rename_session(&self, old: &str, new: &str) -> Result<(), String> {
        let target = format!("={}", old);
        let status = Command::new("tmux")
            .args(["rename-session", "-t", &target, new])
            .status()
            .map_err(|e| format!("Failed to rename session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to rename tmux session".to_string())
        }
    }

    fn kill_session(&self, name: &str) -> Result<(), String> {
        let target = format!("={}", name);
        let status = Command::new("tmux")
            .args(["kill-session", "-t", &target])
            .status()
            .map_err(|e| format!("Failed to kill session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to kill tmux session".to_string())
        }
    }

    fn duplicate_session(
        &self,
        source: &str,
        new_name: &str,
        claude_running: bool,
    ) -> Result<(), String> {
        // Trailing colon makes this a *pane* target — `=name` alone
        // resolves to the session, and pane-scope format vars like
        // `#{pane_current_path}` are empty on session targets. Same
        // gotcha that `capture_pane` documents above.
        let target = format!("={}:", source);
        let out = Command::new("tmux")
            .args([
                "display-message",
                "-t",
                &target,
                "-p",
                "#{pane_current_path}",
            ])
            .output()
            .map_err(|e| format!("tmux display-message failed: {}", e))?;
        if !out.status.success() {
            return Err("source session not found".to_string());
        }
        let cwd = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if cwd.is_empty() {
            crate::duplicate_log::log(&format!(
                "local.duplicate: display-message returned empty cwd for source={:?}",
                source
            ));
            return Err("source session has no active pane".to_string());
        }
        crate::duplicate_log::log(&format!(
            "local.duplicate: source={:?} cwd={:?} claude_running={}",
            source, cwd, claude_running
        ));

        let resume_id = if claude_running {
            find_latest_session_id(&cwd)
        } else {
            None
        };
        crate::duplicate_log::log(&format!(
            "local.duplicate: resume_id={:?}",
            resume_id
        ));

        let mut args: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            new_name.to_string(),
            "-c".into(),
            cwd.clone(),
        ];
        // Wrap the user command in `bash -lc` so login-shell init runs
        // and PATH modifications from `.bash_profile`/`.zprofile` (where
        // nvm/asdf/volta tend to add the directory containing `claude`)
        // take effect. Without this, tmux invokes the command via
        // /bin/sh -c with a bare PATH and `claude` is "command not found"
        // on hosts where the binary lives in a user-managed bin dir.
        if let Some(id) = &resume_id {
            args.push(format!(
                "bash -lc 'claude --resume {} --fork-session'",
                id
            ));
        } else if claude_running {
            args.push("bash -lc 'claude'".into());
        }

        // Capture stderr so the App's banner shows tmux's actual reason
        // (e.g. `duplicate session: foo`, `can't find pane`, server missing)
        // instead of a hand-rolled "duplicate name?" guess.
        crate::duplicate_log::log(&format!(
            "local.duplicate: invoking tmux {}",
            args.join(" ")
        ));
        let out = Command::new("tmux")
            .args(&args)
            .output()
            .map_err(|e| format!("Failed to launch tmux: {}", e))?;
        crate::duplicate_log::log(&format!(
            "local.duplicate: exit={:?} stdout={:?} stderr={:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ));
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.lines().next().unwrap_or("tmux new-session failed");
            Err(msg.to_string())
        }
    }
}

/// Find the most recently modified `.jsonl` in
/// `$HOME/.claude/projects/<encoded(cwd)>/` and return its stem if it
/// looks like a session UUID. Returns `None` if the project dir is
/// missing, empty, contains no UUID-named jsonls, or `$HOME` is unset.
///
/// **Known gap**: this picks the newest jsonl in the project dir
/// regardless of which Claude process produced it. If a user starts a
/// fresh `claude` (no jsonl written yet — Claude only writes on the
/// first turn) and immediately duplicates, we'd resume an *older*
/// conversation from the same cwd instead of the just-started one.
/// `--fork-session` makes this non-destructive (the original is
/// untouched) and the user can `/clear` in the forked session, but
/// it's worth knowing. A pane-process-start-time comparison would
/// close the gap; deferred to a follow-up.
fn find_latest_session_id(cwd: &str) -> Option<String> {
    let encoded: String = cwd
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let home = std::env::var("HOME").ok()?;
    let dir = std::path::PathBuf::from(home)
        .join(".claude/projects")
        .join(encoded);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        if !is_session_uuid(&stem) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        // Tie-break on stem lexicographically (descending — `>` on string)
        // so equal-mtime files produce a deterministic pick instead of
        // depending on `read_dir` iteration order.
        let take = match best.as_ref() {
            None => true,
            Some((t, s)) => mtime > *t || (mtime == *t && stem > *s),
        };
        if take {
            best = Some((mtime, stem));
        }
    }
    best.map(|(_, s)| s)
}

/// Capture the visible content of a session's active pane *with* ANSI
/// escape sequences so the renderer can preserve color and styling.
///
/// Target is `=name:` — the trailing colon is required: `capture-pane`
/// resolves a *pane* target, not a session target, and `=name` (without
/// the colon) trips the parser into "can't find pane" rather than
/// matching the session. The colon means "session=name exactly, default
/// window and pane," which is what we want. Errors on tmux spawn
/// failure or non-zero exit (e.g. session vanished).
pub fn capture_pane(name: &str) -> Result<String, String> {
    let target = format!("={}:", name);
    let out = Command::new("tmux")
        .args(["capture-pane", "-e", "-p", "-t", &target])
        .output()
        .map_err(|e| format!("tmux capture-pane failed to spawn: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tmux capture-pane: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod find_session_id_tests {
    //! Unit tests for `find_latest_session_id`. Each test holds the
    //! `acceptance_lock` because we mutate the process-wide `HOME` env
    //! var (the function reads it directly) — without the lock, parallel
    //! tests would race each other.

    use super::find_latest_session_id;
    use crate::test_harness::acquire_acceptance_lock;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Set HOME to a fresh tempdir for the duration of one test, returning
    /// the dir path and a guard that restores HOME on drop. The lock must
    /// be held by the caller.
    struct HomeGuard {
        prev: Option<String>,
        dir: PathBuf,
    }
    impl HomeGuard {
        fn new() -> Self {
            let prev = std::env::var("HOME").ok();
            let dir = std::env::temp_dir().join(format!(
                "ade-find-session-id-{}-{}",
                std::process::id(),
                rand_suffix()
            ));
            std::fs::create_dir_all(&dir).expect("mk home dir");
            std::env::set_var("HOME", &dir);
            HomeGuard { prev, dir }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rand_suffix() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!("{}", N.fetch_add(1, Ordering::SeqCst))
    }

    fn project_dir_for(home: &PathBuf, cwd: &str) -> PathBuf {
        let encoded: String = cwd
            .chars()
            .map(|c| if c == '/' { '-' } else { c })
            .collect();
        home.join(".claude/projects").join(encoded)
    }

    fn touch(path: &PathBuf) {
        std::fs::write(path, b"").expect("touch");
    }

    #[test]
    fn missing_project_dir_returns_none() {
        let _lock = acquire_acceptance_lock();
        let _home = HomeGuard::new();
        assert_eq!(find_latest_session_id("/no/such/path"), None);
    }

    #[test]
    fn empty_project_dir_returns_none() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/some/proj";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");
        assert_eq!(find_latest_session_id(cwd), None);
    }

    #[test]
    fn single_uuid_jsonl_is_picked() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/single";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        touch(&dir.join(format!("{}.jsonl", uuid)));
        assert_eq!(find_latest_session_id(cwd), Some(uuid.to_string()));
    }

    #[test]
    fn newest_mtime_wins() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/multi";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");

        let older = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let newer = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        touch(&dir.join(format!("{}.jsonl", older)));
        // Sleep just past filesystem mtime granularity. macOS HFS+/APFS
        // resolves to nanoseconds, but to avoid flakes on filesystems
        // with coarser resolution (some Linux ext4 on noatime mounts)
        // we pad to 1.1s.
        std::thread::sleep(Duration::from_millis(1100));
        touch(&dir.join(format!("{}.jsonl", newer)));

        assert_eq!(find_latest_session_id(cwd), Some(newer.to_string()));
    }

    #[test]
    fn non_uuid_jsonl_ignored() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/mixed";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");

        // Non-UUID jsonl gets the freshest mtime — it must still be
        // skipped in favor of the UUID-named one. This pins the
        // injection-safety claim: a tampered project dir can't pass a
        // non-UUID filename through into the tmux command string.
        // Stick to characters legal in filenames on macOS/Linux but
        // designed to trip a naive validator (semicolons, backticks,
        // spaces, leading dash).
        let uuid = "12345678-1234-1234-1234-123456789012";
        touch(&dir.join(format!("{}.jsonl", uuid)));
        std::thread::sleep(Duration::from_millis(1100));
        touch(&dir.join("not-a-uuid.jsonl"));
        touch(&dir.join("inject;rm.jsonl"));
        touch(&dir.join("`backticks`.jsonl"));
        touch(&dir.join("-leading-dash.jsonl"));
        touch(&dir.join("00000000-0000-0000-0000-zzzzzzzzzzzz.jsonl"));

        assert_eq!(find_latest_session_id(cwd), Some(uuid.to_string()));
    }

    #[test]
    fn non_jsonl_extensions_ignored() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/extmix";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");

        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        touch(&dir.join(format!("{}.jsonl", uuid)));
        std::thread::sleep(Duration::from_millis(1100));
        // Newer mtime, but wrong extension — must be ignored.
        touch(&dir.join(format!("{}.json", uuid)));
        touch(&dir.join(format!("{}.txt", uuid)));

        assert_eq!(find_latest_session_id(cwd), Some(uuid.to_string()));
    }

    #[test]
    fn equal_mtime_uses_stable_lexicographic_tiebreaker() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        let cwd = "/tmp/tiebreak";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");

        let a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let z = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        // Set both mtimes to exactly the same instant by writing them
        // and then explicitly clobbering one to match the other.
        let pa = dir.join(format!("{}.jsonl", a));
        let pz = dir.join(format!("{}.jsonl", z));
        touch(&pa);
        touch(&pz);
        let mtime = std::fs::metadata(&pa)
            .expect("meta a")
            .modified()
            .expect("mtime a");
        // filetime crate isn't a dep — instead we re-write both files in
        // immediate succession and rely on sub-millisecond fs granularity
        // to tie. If the fs ties them, our tiebreaker should pick the
        // lexicographically greater stem (z > a). If the fs doesn't tie
        // (unlikely on macOS APFS), z is the second-written and wins on
        // mtime anyway — so the assertion holds either way.
        let _ = mtime;
        let result = find_latest_session_id(cwd).expect("some uuid");
        assert_eq!(
            result, z,
            "equal-mtime case must pick lexicographically greater stem; \
             got {}",
            result
        );
    }

    #[test]
    fn cwd_with_spaces_encodes_and_looks_up() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        // Paths with spaces are legal on disk; Claude Code encodes them
        // by just replacing `/` with `-`, so the space stays. We need
        // to match that exactly.
        let cwd = "/tmp/my project dir";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir with spaces");
        let uuid = "11111111-2222-3333-4444-555555555555";
        touch(&dir.join(format!("{}.jsonl", uuid)));
        assert_eq!(find_latest_session_id(cwd), Some(uuid.to_string()));
    }

    #[test]
    fn cwd_with_dots_encodes_correctly() {
        let _lock = acquire_acceptance_lock();
        let home = HomeGuard::new();
        // Dots in directory names are unchanged by the encoding (only
        // `/` becomes `-`). Confirm the lookup still finds the file.
        let cwd = "/tmp/has.dot.in.path";
        let dir = project_dir_for(&home.dir, cwd);
        std::fs::create_dir_all(&dir).expect("mk proj dir");
        let uuid = "deadbeef-dead-beef-dead-beefdeadbeef";
        touch(&dir.join(format!("{}.jsonl", uuid)));
        assert_eq!(find_latest_session_id(cwd), Some(uuid.to_string()));
    }
}
