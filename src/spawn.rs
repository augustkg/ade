//! Spawning new workstreams: the policy half.
//!
//! An orchestrator session can create further sessions, which means a bug or a
//! confused model can create them without bound. The decisions that stop that —
//! name validation, depth, and a live-session quota — live here as pure
//! functions so they can be tested exhaustively; `tmux::local::spawn_session`
//! performs the side effects.
//!
//! Lineage is recorded on the tmux session itself via user options
//! (`@ade-spawned-by`, `@ade-spawn-depth`), matching the existing `@ade-title` /
//! `@ade-parent` convention. That keeps the bookkeeping where the sessions are:
//! it survives ADE restarting, and `tmux kill-session` cleans it up for free.

/// How deep a spawn chain may go. An orchestrator (depth 0) may create workers
/// (depth 1), which may create fixers (depth 2), and there it stops. Deep
/// enough for "feature → blocker → sub-blocker", shallow enough that a runaway
/// loop is capped fast.
pub const MAX_SPAWN_DEPTH: u32 = 3;

/// Ceiling on simultaneously-live ADE-spawned sessions. Sessions a human made
/// are not counted — this bounds only what agents create.
pub const MAX_SPAWNED_ALIVE: usize = 12;

/// Longest acceptable session name.
pub const MAX_NAME_LEN: usize = 64;

/// One row of `tmux list-sessions -F '#{session_name}\t#{@ade-spawned-by}\t#{@ade-spawn-depth}'`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRecord {
    pub name: String,
    /// Session that created this one; empty for human-created sessions.
    pub spawned_by: String,
    pub depth: u32,
}

impl SpawnRecord {
    pub fn is_agent_spawned(&self) -> bool {
        !self.spawned_by.is_empty()
    }
}

/// Parse the lineage listing. Rows tmux couldn't fill are simply un-marked
/// sessions (a human's), which parse to `spawned_by: ""` and `depth: 0`.
pub fn parse_spawn_inventory(listing: &str) -> Vec<SpawnRecord> {
    listing
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut cols = line.split('\t');
            SpawnRecord {
                name: cols.next().unwrap_or("").trim().to_string(),
                spawned_by: cols.next().unwrap_or("").trim().to_string(),
                depth: cols
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or(0),
            }
        })
        .filter(|r| !r.name.is_empty())
        .collect()
}

/// Reject names that tmux can't address unambiguously or that would be unusable
/// as a mail address.
///
/// `/` is allowed because ADE uses it for folder grouping (`Archie/task-x`).
/// `.` and `:` are refused outright: tmux uses them as window/pane separators in
/// targets, so a name containing them cannot be exactly addressed.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name is empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "session name is {} characters; the limit is {}",
            name.len(),
            MAX_NAME_LEN
        ));
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("session name must not start or end with '/'".to_string());
    }
    if name.contains("//") {
        return Err("session name must not contain an empty path segment".to_string());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/')))
    {
        return Err(format!(
            "session name may only contain letters, digits, '-', '_' and '/' \
             (found {:?}); tmux treats '.' and ':' as target separators",
            bad
        ));
    }
    Ok(())
}

/// Decide whether a spawn is allowed, given the caller's depth and what is
/// currently alive. Returns the depth to stamp on the new session.
///
/// Refusals are phrased for the model that will read them: they say what the
/// limit is and what to do instead, because the caller is an agent that will
/// otherwise retry blindly.
pub fn authorize_spawn(
    caller_depth: u32,
    inventory: &[SpawnRecord],
    new_name: &str,
) -> Result<u32, String> {
    validate_session_name(new_name)?;

    if inventory.iter().any(|r| r.name == new_name) {
        return Err(format!(
            "a session named '{}' already exists — pick another name, or \
             message that session instead of creating it",
            new_name
        ));
    }

    let child_depth = caller_depth + 1;
    if child_depth > MAX_SPAWN_DEPTH {
        return Err(format!(
            "spawn depth {} would exceed the limit of {} — this session is \
             already {} level(s) deep. Report back to whoever spawned you \
             instead of delegating further.",
            child_depth, MAX_SPAWN_DEPTH, caller_depth
        ));
    }

    let alive = inventory.iter().filter(|r| r.is_agent_spawned()).count();
    if alive >= MAX_SPAWNED_ALIVE {
        return Err(format!(
            "{} agent-spawned sessions are already alive (limit {}) — finish or \
             kill some before creating more",
            alive, MAX_SPAWNED_ALIVE
        ));
    }

    Ok(child_depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, by: &str, depth: u32) -> SpawnRecord {
        SpawnRecord {
            name: name.to_string(),
            spawned_by: by.to_string(),
            depth,
        }
    }

    // ── name validation ──

    #[test]
    fn accepts_the_names_ade_actually_uses() {
        for n in ["worker", "Archie/task-auth", "fix_thing", "a/b/c", "x-1"] {
            assert!(validate_session_name(n).is_ok(), "{:?} should be valid", n);
        }
    }

    #[test]
    fn rejects_tmux_target_separators() {
        // '.' and ':' would make the session impossible to address exactly.
        for n in ["has.dot", "has:colon", "a.b:c"] {
            assert!(validate_session_name(n).is_err(), "{:?} must be rejected", n);
        }
    }

    #[test]
    fn rejects_shell_and_whitespace_hazards() {
        for n in ["with space", "semi;colon", "dollar$sign", "quote'", "nl\nname", "back`tick"] {
            assert!(validate_session_name(n).is_err(), "{:?} must be rejected", n);
        }
    }

    #[test]
    fn rejects_malformed_paths_and_extremes() {
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("/leading").is_err());
        assert!(validate_session_name("trailing/").is_err());
        assert!(validate_session_name("double//seg").is_err());
        assert!(validate_session_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(validate_session_name(&"x".repeat(MAX_NAME_LEN)).is_ok());
    }

    // ── inventory parsing ──

    #[test]
    fn parses_lineage_and_distinguishes_human_sessions() {
        let listing = "orch\t\t\nworker-a\torch\t1\nworker-b\torch\t1\n";
        let inv = parse_spawn_inventory(listing);
        assert_eq!(inv.len(), 3);
        assert!(!inv[0].is_agent_spawned(), "unmarked session is the human's");
        assert_eq!(inv[0].depth, 0);
        assert!(inv[1].is_agent_spawned());
        assert_eq!(inv[1].spawned_by, "orch");
        assert_eq!(inv[1].depth, 1);
    }

    #[test]
    fn tolerates_ragged_rows() {
        // tmux omits trailing empty columns in some versions; a missing depth
        // must read as 0, not panic or drop the session.
        let inv = parse_spawn_inventory("solo\nmarked\tparent\n\n  \n");
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0].name, "solo");
        assert_eq!(inv[1].spawned_by, "parent");
        assert_eq!(inv[1].depth, 0);
    }

    // ── the spawn gate ──

    #[test]
    fn allows_a_first_level_spawn_and_stamps_depth_one() {
        let inv = vec![rec("orch", "", 0)];
        assert_eq!(authorize_spawn(0, &inv, "worker").unwrap(), 1);
    }

    #[test]
    fn refuses_a_duplicate_name_and_says_what_to_do() {
        let inv = vec![rec("orch", "", 0), rec("worker", "orch", 1)];
        let e = authorize_spawn(0, &inv, "worker").unwrap_err();
        assert!(e.contains("already exists"));
        assert!(e.contains("message that session instead"));
    }

    #[test]
    fn caps_the_spawn_chain_depth() {
        let inv = vec![rec("orch", "", 0)];
        // depth 0 -> 1 -> 2 -> 3 allowed; the one that would make 4 is not.
        assert!(authorize_spawn(MAX_SPAWN_DEPTH - 1, &inv, "ok").is_ok());
        let e = authorize_spawn(MAX_SPAWN_DEPTH, &inv, "too-deep").unwrap_err();
        assert!(e.contains("exceed the limit"));
        assert!(e.contains("Report back"), "must tell the agent what to do instead");
    }

    #[test]
    fn caps_simultaneously_live_spawned_sessions() {
        let mut inv: Vec<SpawnRecord> = (0..MAX_SPAWNED_ALIVE)
            .map(|i| rec(&format!("w{}", i), "orch", 1))
            .collect();
        let e = authorize_spawn(0, &inv, "one-more").unwrap_err();
        assert!(e.contains("already alive"));
        // Killing one frees a slot.
        inv.pop();
        assert!(authorize_spawn(0, &inv, "one-more").is_ok());
    }

    #[test]
    fn human_sessions_do_not_consume_the_agent_quota() {
        // A machine with many hand-made sessions must not block spawning.
        let inv: Vec<SpawnRecord> = (0..50)
            .map(|i| rec(&format!("mine-{}", i), "", 0))
            .collect();
        assert!(authorize_spawn(0, &inv, "worker").is_ok());
    }

    #[test]
    fn invalid_names_are_refused_before_any_quota_check() {
        let inv: Vec<SpawnRecord> = (0..MAX_SPAWNED_ALIVE)
            .map(|i| rec(&format!("w{}", i), "orch", 1))
            .collect();
        // Even at quota, the name error is the useful one to surface.
        let e = authorize_spawn(0, &inv, "bad name").unwrap_err();
        assert!(e.contains("may only contain"));
    }
}
