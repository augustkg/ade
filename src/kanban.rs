//! Kanban board over tmux sessions: column config, placement persistence
//! helpers, and the pure column-assignment logic.
//!
//! Columns come from `~/.config/ade/kanban.toml` (hand-edited, never
//! rewritten by ADE — comments survive). Two columns are *automatic*:
//! `auto-active` holds every session whose Claude is Working right now,
//! `auto-awaiting` holds everything that isn't manually placed (no Claude
//! running, or Claude idle at its prompt / waiting on an approval — all
//! "your move" states). The rest are *manual*: a session sits there only
//! because the user moved it there, and gets pulled back to auto-active
//! the moment Claude starts Working (see `App::reconcile_kanban`).
//!
//! Manual placements live in `state.toml` (see `state::KanbanState`),
//! keyed by `(host, session_name)` — tmux's `$N` session_id is only
//! stable per server lifetime so it can't be the persisted key.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::claude_status::ClaudeState;
use crate::model::{Machine, Session};
use crate::state::PlacementEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColumnKind {
    /// User-placed only.
    Manual,
    /// Automatic: default column for every session without a manual
    /// placement. Exactly one per config.
    AutoAwaiting,
    /// Automatic: Claude is Working right now. Exactly one per config.
    AutoActive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Stable key referenced by persisted placements; never shown.
    pub id: String,
    /// Display name — freely renamable, including for auto columns.
    pub name: String,
    pub kind: ColumnKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KanbanConfig {
    #[serde(default)]
    pub columns: Vec<Column>,
}

impl Default for KanbanConfig {
    fn default() -> Self {
        fn col(id: &str, name: &str, kind: ColumnKind) -> Column {
            Column {
                id: id.to_string(),
                name: name.to_string(),
                kind,
            }
        }
        KanbanConfig {
            columns: vec![
                col("idle", "Idle", ColumnKind::Manual),
                col("awaiting", "Awaiting human", ColumnKind::AutoAwaiting),
                col("active", "Active", ColumnKind::AutoActive),
                col("done", "Done", ColumnKind::Manual),
                col("verified", "Verified done", ColumnKind::Manual),
            ],
        }
    }
}

impl KanbanConfig {
    /// `~/.config/ade/kanban.toml` — same XDG resolution as `hosts.toml`.
    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        xdg.join("ade").join("kanban.toml")
    }

    /// Missing file → defaults silently. Parse or validation failure →
    /// defaults + a warning string for the UI. Placements that reference
    /// column ids the returned config doesn't know are *not* an error and
    /// must never be deleted by callers — they render in auto-awaiting and
    /// come back when the user fixes the file.
    pub fn load() -> (Self, Option<String>) {
        let path = Self::path();
        let body = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return (Self::default(), None),
        };
        match toml::from_str::<KanbanConfig>(&body) {
            Ok(cfg) => match cfg.validate() {
                Ok(()) => (cfg, None),
                Err(e) => (
                    Self::default(),
                    Some(format!(
                        "kanban.toml invalid ({}) — using default columns",
                        e
                    )),
                ),
            },
            Err(e) => (
                Self::default(),
                Some(format!(
                    "failed to parse {}: {} — using default columns",
                    path.display(),
                    e
                )),
            ),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.columns.is_empty() {
            return Err("no columns defined".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.id.trim().is_empty() {
                return Err("column id cannot be empty".to_string());
            }
            if c.name.trim().is_empty() {
                return Err(format!("column '{}' has an empty name", c.id));
            }
            if !seen.insert(c.id.as_str()) {
                return Err(format!("duplicate column id '{}'", c.id));
            }
        }
        let awaiting = self
            .columns
            .iter()
            .filter(|c| c.kind == ColumnKind::AutoAwaiting)
            .count();
        if awaiting != 1 {
            return Err(format!(
                "need exactly one 'auto-awaiting' column, found {}",
                awaiting
            ));
        }
        let active = self
            .columns
            .iter()
            .filter(|c| c.kind == ColumnKind::AutoActive)
            .count();
        if active != 1 {
            return Err(format!(
                "need exactly one 'auto-active' column, found {}",
                active
            ));
        }
        Ok(())
    }

    /// Safe after `validate()` — falls back to 0 defensively.
    pub fn auto_active_idx(&self) -> usize {
        self.columns
            .iter()
            .position(|c| c.kind == ColumnKind::AutoActive)
            .unwrap_or(0)
    }

    pub fn auto_awaiting_idx(&self) -> usize {
        self.columns
            .iter()
            .position(|c| c.kind == ColumnKind::AutoAwaiting)
            .unwrap_or(0)
    }

    pub fn idx_of_id(&self, id: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.id == id)
    }
}

/// Column index for one session. Precedence:
///   1. Working right now → auto-active. The manual placement is ignored
///      here and *cleared* by `App::reconcile_kanban` on the same refresh.
///   2. Manual placement resolving to a Manual column → that column. Note
///      this holds through AwaitingApproval too — only Working clears.
///   3. Everything else → auto-awaiting: no Claude in the session, Claude
///      idle at its prompt (arrives as `claude == None`; the rollup in
///      `tmux::map_claude_states` never stores `Idle` — handled here
///      defensively anyway), AwaitingApproval, or a placement whose id is
///      dangling / names an auto column.
///
/// `claude_present` is deliberately not an input: absent and
/// present-but-idle both land in auto-awaiting by design.
pub fn assign_column(
    cfg: &KanbanConfig,
    claude: Option<ClaudeState>,
    manual: Option<&str>,
) -> usize {
    if claude == Some(ClaudeState::Working) {
        return cfg.auto_active_idx();
    }
    if let Some(id) = manual {
        if let Some(idx) = cfg.idx_of_id(id) {
            if cfg.columns[idx].kind == ColumnKind::Manual {
                return idx;
            }
        }
    }
    cfg.auto_awaiting_idx()
}

/// Per-column lists of indices into `sessions`, each sorted by
/// `(machine, raw_name)` for stable ordering across refreshes. Cheap for
/// tens of sessions — recomputed per frame / keypress rather than cached.
pub fn build_board(
    cfg: &KanbanConfig,
    sessions: &[Session],
    placements: &HashMap<(Machine, String), String>,
) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by(|&a, &b| {
        (&sessions[a].machine, &sessions[a].raw_name)
            .cmp(&(&sessions[b].machine, &sessions[b].raw_name))
    });
    let mut board: Vec<Vec<usize>> = vec![Vec::new(); cfg.columns.len()];
    for si in order {
        let s = &sessions[si];
        let manual = placements
            .get(&(s.machine.clone(), s.raw_name.clone()))
            .map(|c| c.as_str());
        let col = assign_column(cfg, s.claude, manual);
        board[col].push(si);
    }
    board
}

/// `Machine` → the `host` string persisted in placement entries.
/// `Machine::Local ↔ "local"` — hence `"local"` is a reserved host name
/// (enforced in `hosts::Config::upsert`).
pub fn machine_to_host(m: &Machine) -> String {
    m.title_label().to_string()
}

fn host_to_machine(host: &str) -> Machine {
    if host == "local" {
        Machine::Local
    } else {
        Machine::Remote(host.to_string())
    }
}

/// Canonicalize persisted entries into the in-memory map: drop blank
/// entries, dedup duplicate `(host, session)` keys last-wins (state.toml
/// can be hand-edited). Field values are preserved *exactly* — tmux
/// session names may legitimately carry surrounding whitespace, and
/// trimming here would silently re-key the placement on reload so it no
/// longer matches the live session (and then gets pruned).
pub fn entries_to_map(entries: &[PlacementEntry]) -> HashMap<(Machine, String), String> {
    let mut out = HashMap::new();
    for e in entries {
        if e.host.trim().is_empty()
            || e.session.trim().is_empty()
            || e.column.trim().is_empty()
        {
            continue;
        }
        out.insert(
            (host_to_machine(&e.host), e.session.clone()),
            e.column.clone(),
        );
    }
    out
}

/// Serialize the in-memory map back to entries, sorted by
/// `(host, session)` for deterministic state.toml output.
pub fn map_to_entries(map: &HashMap<(Machine, String), String>) -> Vec<PlacementEntry> {
    let mut out: Vec<PlacementEntry> = map
        .iter()
        .map(|((m, s), c)| PlacementEntry {
            host: machine_to_host(m),
            session: s.clone(),
            column: c.clone(),
        })
        .collect();
    out.sort_by(|a, b| {
        (a.host.as_str(), a.session.as_str()).cmp(&(b.host.as_str(), b.session.as_str()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_status::ClaudeState::*;

    fn cfg() -> KanbanConfig {
        KanbanConfig::default()
    }

    fn session(machine: Machine, name: &str, claude: Option<ClaudeState>) -> Session {
        Session {
            raw_name: name.to_string(),
            session_id: format!("${}", name.len()),
            prefix: None,
            leaf: name.to_string(),
            windows: 1,
            attached: false,
            machine,
            claude,
            claude_demoted: false,
            claude_present: claude.is_some(),
            claude_context_pct: None,
        }
    }

    use crate::claude_status::ClaudeState;

    // ── assign_column precedence matrix ──

    #[test]
    fn working_goes_to_auto_active_even_with_manual_done() {
        let c = cfg();
        assert_eq!(assign_column(&c, Some(Working), Some("done")), c.auto_active_idx());
        assert_eq!(assign_column(&c, Some(Working), None), c.auto_active_idx());
    }

    #[test]
    fn manual_done_holds_while_idle_and_awaiting_approval() {
        let c = cfg();
        let done = c.idx_of_id("done").unwrap();
        // No Claude at all.
        assert_eq!(assign_column(&c, None, Some("done")), done);
        // Defensive: rollup never stores Idle, but handle it anyway.
        assert_eq!(assign_column(&c, Some(Idle), Some("done")), done);
        // Only Working clears manual placement — an approval prompt in a
        // Done session shows its red chip but stays put.
        assert_eq!(assign_column(&c, Some(AwaitingApproval), Some("done")), done);
    }

    #[test]
    fn unplaced_lands_in_auto_awaiting() {
        let c = cfg();
        let awaiting = c.auto_awaiting_idx();
        assert_eq!(assign_column(&c, None, None), awaiting);
        assert_eq!(assign_column(&c, Some(Idle), None), awaiting);
        assert_eq!(assign_column(&c, Some(AwaitingApproval), None), awaiting);
    }

    #[test]
    fn dangling_or_auto_placement_ids_fall_to_awaiting() {
        let c = cfg();
        let awaiting = c.auto_awaiting_idx();
        // Column id that doesn't exist (e.g. user broke kanban.toml —
        // placement preserved elsewhere, rendered in awaiting).
        assert_eq!(assign_column(&c, None, Some("blocked")), awaiting);
        // Placement naming an auto column id is not a valid manual target.
        assert_eq!(assign_column(&c, None, Some("active")), awaiting);
        assert_eq!(assign_column(&c, None, Some("awaiting")), awaiting);
    }

    #[test]
    fn manual_idle_column_is_a_normal_manual_target() {
        let c = cfg();
        assert_eq!(
            assign_column(&c, None, Some("idle")),
            c.idx_of_id("idle").unwrap()
        );
    }

    // ── build_board ──

    #[test]
    fn board_sorts_columns_by_machine_then_name() {
        let c = cfg();
        let sessions = vec![
            session(Machine::Remote("zeta".into()), "b", None),
            session(Machine::Local, "z", None),
            session(Machine::Local, "a", None),
        ];
        let board = build_board(&c, &sessions, &HashMap::new());
        let awaiting = &board[c.auto_awaiting_idx()];
        // Local < Remote, then by name.
        assert_eq!(awaiting, &vec![2, 1, 0]);
    }

    #[test]
    fn board_routes_working_and_placed_sessions() {
        let c = cfg();
        let sessions = vec![
            session(Machine::Local, "busy", Some(Working)),
            session(Machine::Local, "parked", None),
            session(Machine::Local, "fresh", None),
        ];
        let mut placements = HashMap::new();
        placements.insert((Machine::Local, "parked".to_string()), "done".to_string());
        // Working session with a stale manual placement — assignment
        // ignores it (reconcile clears it separately).
        placements.insert((Machine::Local, "busy".to_string()), "verified".to_string());
        let board = build_board(&c, &sessions, &placements);
        assert_eq!(board[c.auto_active_idx()], vec![0]);
        assert_eq!(board[c.idx_of_id("done").unwrap()], vec![1]);
        assert_eq!(board[c.auto_awaiting_idx()], vec![2]);
        assert!(board[c.idx_of_id("idle").unwrap()].is_empty());
    }

    // ── config validation ──

    #[test]
    fn default_config_validates_and_round_trips() {
        let c = cfg();
        c.validate().expect("default config valid");
        let toml_s = toml::to_string_pretty(&c).unwrap();
        let back: KanbanConfig = toml::from_str(&toml_s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn validation_rejects_bad_configs() {
        let mut c = cfg();
        c.columns.retain(|col| col.kind != ColumnKind::AutoActive);
        assert!(c.validate().is_err(), "zero auto-active");

        let mut c = cfg();
        c.columns.push(Column {
            id: "active2".into(),
            name: "Active 2".into(),
            kind: ColumnKind::AutoActive,
        });
        assert!(c.validate().is_err(), "two auto-active");

        let mut c = cfg();
        c.columns[0].id = "done".into();
        assert!(c.validate().is_err(), "duplicate ids");

        let mut c = cfg();
        c.columns[0].name = "  ".into();
        assert!(c.validate().is_err(), "blank name");

        let c = KanbanConfig { columns: vec![] };
        assert!(c.validate().is_err(), "empty");
    }

    #[test]
    fn reordered_renamed_and_extended_config_validates() {
        let toml_s = r#"
            [[columns]]
            id = "done"
            name = "Shipped"
            kind = "manual"

            [[columns]]
            id = "active"
            name = "Cooking"
            kind = "auto-active"

            [[columns]]
            id = "blocked"
            name = "Blocked"
            kind = "manual"

            [[columns]]
            id = "awaiting"
            name = "My move"
            kind = "auto-awaiting"
        "#;
        let c: KanbanConfig = toml::from_str(toml_s).unwrap();
        c.validate().expect("custom config valid");
        assert_eq!(c.auto_active_idx(), 1);
        assert_eq!(c.auto_awaiting_idx(), 3);
        assert_eq!(assign_column(&c, None, Some("blocked")), 2);
    }

    #[test]
    fn empty_toml_fails_validation_not_parse() {
        let c: KanbanConfig = toml::from_str("").unwrap();
        assert!(c.validate().is_err());
    }

    // ── placement entry canonicalization ──

    #[test]
    fn entries_dedup_last_wins_and_drop_blank() {
        let entries = vec![
            PlacementEntry {
                host: "local".into(),
                session: "work/api".into(),
                column: "done".into(),
            },
            PlacementEntry {
                host: "local".into(),
                session: "work/api".into(),
                column: "verified".into(),
            },
            PlacementEntry {
                host: "".into(),
                session: "x".into(),
                column: "done".into(),
            },
        ];
        let map = entries_to_map(&entries);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get(&(Machine::Local, "work/api".to_string())).unwrap(),
            "verified"
        );
    }

    #[test]
    fn session_names_with_surrounding_whitespace_round_trip_exactly() {
        // tmux allows session names like " work " — the persisted key must
        // come back byte-identical or the placement silently detaches from
        // the live session and gets pruned (Codex implementation review).
        let mut map = HashMap::new();
        map.insert((Machine::Local, " work ".to_string()), "done".to_string());
        let entries = map_to_entries(&map);
        assert_eq!(entries[0].session, " work ");
        assert_eq!(entries_to_map(&entries), map);
    }

    #[test]
    fn map_round_trips_through_entries_sorted() {
        let mut map = HashMap::new();
        map.insert((Machine::Remote("srv".into()), "b".to_string()), "done".to_string());
        map.insert((Machine::Local, "a/x".to_string()), "idle".to_string());
        let entries = map_to_entries(&map);
        assert_eq!(entries[0].host, "local");
        assert_eq!(entries[0].session, "a/x");
        assert_eq!(entries[1].host, "srv");
        assert_eq!(entries_to_map(&entries), map);
    }
}
