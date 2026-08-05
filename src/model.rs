use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::SystemTime;

use crate::claude_status::ClaudeState;
use crate::tmux;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Machine {
    Local,
    Remote(String),
}

impl Machine {
    pub fn label(&self) -> &str {
        match self {
            Machine::Local => "Local",
            Machine::Remote(name) => name.as_str(),
        }
    }

    /// Lowercase variant used in the terminal-tab title (`folder/session | host`).
    /// Distinct from `label()` so UI capitalization isn't coupled to title style.
    pub fn title_label(&self) -> &str {
        match self {
            Machine::Local => "local",
            Machine::Remote(name) => name.as_str(),
        }
    }
}

impl Ord for Machine {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Machine::Local, Machine::Local) => Ordering::Equal,
            (Machine::Local, Machine::Remote(_)) => Ordering::Less,
            (Machine::Remote(_), Machine::Local) => Ordering::Greater,
            (Machine::Remote(a), Machine::Remote(b)) => a.cmp(b),
        }
    }
}
impl PartialOrd for Machine {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub raw_name: String,
    /// tmux's stable `#{session_id}` (e.g. `$3`). Used by
    /// `App::apply_refresh_result` as the diff key for notification
    /// dispatch — survives `rename-session`, distinct from
    /// `kill+recreate-same-name`.
    pub session_id: String,
    pub prefix: Option<String>,
    pub leaf: String,
    pub windows: u32,
    pub attached: bool,
    pub machine: Machine,
    pub claude: Option<ClaudeState>,
    /// Mirrors `tmux::Session::claude_demoted`. Carried through to
    /// `App::apply_refresh_result` for notification suppression rule 5
    /// (TTL-driven demotions are not "Claude finished a turn").
    pub claude_demoted: bool,
    /// Mirrors `tmux::Session::claude_present`. True when any pane in
    /// this session is running Claude, regardless of state. The Duplicate
    /// action reads this to decide whether to fork the Claude session.
    pub claude_present: bool,
    /// Context-window percentage (0..=100) for this session's Claude pane,
    /// surfaced verbatim from `tmux::Session::claude_context_pct`. Rendered
    /// as `claude · NN%` by the UI. `None` when the v3 hook hasn't written
    /// usage data yet (legacy v2 install on the host, or no assistant turn
    /// has happened in the session).
    pub claude_context_pct: Option<u8>,
    /// Most-recent Claude activity in this session, carried from
    /// `tmux::Session::claude_last_activity` (local: file mtime; remote:
    /// `seq`-derived). `Tree::group` rolls these up per folder and sorts
    /// folders by recency so the session you just typed into floats to the
    /// top. `None` for sessions with no usable activity signal yet.
    pub claude_last_activity: Option<SystemTime>,
}

/// Split a raw tmux session name into `(folder_prefix, leaf)` on the first
/// `/`. Folder/leaf separator is `/`: tmux silently rewrites `:` and `.` in
/// session names, so `:` is unusable as a grouping convention; `/` passes
/// through untouched. A name with no `/` (or an empty side) is all-leaf.
///
/// Shared by `Session::from_tmux` (TUI tree grouping) and `json_out` (the
/// `prefix`/`leaf` fields of `ade sessions --json`) so both split identically.
pub fn split_prefix_leaf(name: &str) -> (Option<String>, String) {
    match name.split_once('/') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() => {
            (Some(p.to_string()), l.to_string())
        }
        _ => (None, name.to_string()),
    }
}

impl Session {
    pub fn from_tmux(s: tmux::Session, machine: Machine) -> Self {
        let (prefix, leaf) = split_prefix_leaf(&s.name);
        Self {
            raw_name: s.name,
            session_id: s.session_id,
            prefix,
            leaf,
            windows: s.windows,
            attached: s.attached,
            machine,
            claude: s.claude,
            claude_demoted: s.claude_demoted,
            claude_present: s.claude_present,
            claude_context_pct: s.claude_context_pct,
            claude_last_activity: s.claude_last_activity,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub prefix: String,
    pub expanded: bool,
    pub sessions: Vec<usize>,
    pub machines: BTreeSet<Machine>,
    /// Rolled-up Claude state across child sessions. Working > Idle > None.
    pub claude: Option<ClaudeState>,
    /// Most-recent Claude activity across this folder's sessions (max of
    /// their `claude_last_activity`, local or remote). `Tree::group` sorts
    /// folders by this descending — most-recently-active first — so the folder
    /// holding the session you just typed into rises to the top. `None` when
    /// no child session has an activity timestamp (none has written a status
    /// file yet, or only unusable `seq`s); such folders sort alphabetically
    /// below the active ones.
    pub last_activity: Option<SystemTime>,
}

impl Folder {
    pub fn machines_label(&self) -> String {
        let names: Vec<&str> = self.machines.iter().map(|m| m.label()).collect();
        if names.is_empty() {
            String::new()
        } else if names.len() <= 2 {
            names.join(" · ")
        } else {
            format!("{} · {} · +{}", names[0], names[1], names.len() - 2)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Row {
    Folder(usize),
    Session(usize),
    NewSession,
}

/// Compare two activity timestamps for recency ordering: newest first,
/// timestamped-before-untimestamped, equal otherwise. The `Equal` on the
/// `(None, None)` case lets callers layer their own tiebreak (folders use
/// prefix; within a folder a stable sort keeps backend order).
///
/// Timestamps come from mixed sources — local status-file mtime and
/// remote `seq`-derived wall-clock (see `claude_status::seq_to_activity`) —
/// so ordering across machines assumes roughly synchronized (NTP) clocks.
/// This is best-effort recency for a UI hint, not a strict total order.
fn recency_cmp(a: Option<SystemTime>, b: Option<SystemTime>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Stable identity of a visible tree row. The tree cursor is stored
/// positionally (`App::selected_index`), but recency sorting reorders rows on
/// every refresh — so `App::apply_refresh_result` captures the selected row's
/// `RowKey` before the rebuild and re-resolves it after, keeping the cursor on
/// the same workstream rather than the same screen line. Mirrors the kanban
/// board's identity-first focus (`KanbanView::focused_key`). A `Session` is
/// keyed by `(machine, session_id)` — the tmux `#{session_id}` is stable
/// across rename, and pairing it with the machine avoids `$N` collisions
/// between hosts. This is the same identity the notification dispatch keys
/// on. Caveat: `$N` ids are only unique within one tmux *server* lifetime, so
/// across a server restart a reused `$0` could match a different session; the
/// worst case is the cursor landing on that row instead of clamping — no
/// worse than the pre-identity positional behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    Folder(String),
    Session(Machine, String),
    NewSession,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub sessions: Vec<Session>,
    pub folders: Vec<Folder>,
    pub loose: Vec<usize>,
    pub errors: Vec<(Machine, String)>,
    /// When ADE is launched from inside tmux, the name of the local session
    /// the user is currently in. Used by the UI to mark that row with a
    /// subtle ` · here ` chip so the user can see at a glance which session
    /// they're already attached to. `None` outside tmux.
    pub current_session: Option<String>,
}

impl Tree {
    pub fn build(
        per_machine: Vec<(Machine, Vec<tmux::Session>)>,
        errors: Vec<(Machine, String)>,
        prev_expanded: &HashMap<String, bool>,
    ) -> Self {
        let mut sessions: Vec<Session> = Vec::new();
        for (machine, list) in per_machine {
            for s in list {
                sessions.push(Session::from_tmux(s, machine.clone()));
            }
        }
        Self::group(sessions, errors, prev_expanded)
    }

    fn group(
        sessions: Vec<Session>,
        errors: Vec<(Machine, String)>,
        prev_expanded: &HashMap<String, bool>,
    ) -> Self {
        let mut by_prefix: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut loose: Vec<usize> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            match &s.prefix {
                Some(p) => by_prefix.entry(p.clone()).or_default().push(i),
                None => loose.push(i),
            }
        }
        let mut folders: Vec<Folder> = by_prefix
            .into_iter()
            .map(|(prefix, idxs)| {
                let mut machines = BTreeSet::new();
                let mut claude: Option<ClaudeState> = None;
                let mut last_activity: Option<SystemTime> = None;
                for &i in &idxs {
                    machines.insert(sessions[i].machine.clone());
                    if let Some(state) = sessions[i].claude {
                        claude = Some(match claude {
                            Some(cur) => cur.max(state),
                            None => state,
                        });
                    }
                    if let Some(ts) = sessions[i].claude_last_activity {
                        last_activity = Some(match last_activity {
                            Some(cur) => cur.max(ts),
                            None => ts,
                        });
                    }
                }
                // Float the most-recently-active session to the top *within*
                // the folder too. Stable recency sort: active sessions newest
                // first, inactive ones keep their backend order below.
                let mut idxs = idxs;
                idxs.sort_by(|&a, &b| {
                    recency_cmp(sessions[a].claude_last_activity, sessions[b].claude_last_activity)
                });
                let expanded = *prev_expanded.get(&prefix).unwrap_or(&true);
                Folder {
                    prefix,
                    expanded,
                    sessions: idxs,
                    machines,
                    claude,
                    last_activity,
                }
            })
            .collect();
        // Order folders by most-recent Claude activity, newest first, so the
        // session you just typed into floats its folder to the top. Folders
        // with no activity timestamp (all-remote, or no status file yet) fall
        // below the active ones, tie-broken alphabetically by prefix (the
        // `BTreeMap` already produced alphabetical order, but the explicit
        // tiebreak keeps that guarantee independent of the sort's stability).
        folders.sort_by(|a, b| {
            recency_cmp(a.last_activity, b.last_activity)
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        // Same recency float for folderless (loose) sessions.
        loose.sort_by(|&a, &b| {
            recency_cmp(sessions[a].claude_last_activity, sessions[b].claude_last_activity)
        });
        Tree {
            sessions,
            folders,
            loose,
            errors,
            current_session: None, // populated by App after build
        }
    }

    pub fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (i, folder) in self.folders.iter().enumerate() {
            rows.push(Row::Folder(i));
            if folder.expanded {
                for &session_idx in &folder.sessions {
                    rows.push(Row::Session(session_idx));
                }
            }
        }
        for &loose_idx in &self.loose {
            rows.push(Row::Session(loose_idx));
        }
        rows.push(Row::NewSession);
        rows
    }

    pub fn expanded_snapshot(&self) -> HashMap<String, bool> {
        self.folders
            .iter()
            .map(|f| (f.prefix.clone(), f.expanded))
            .collect()
    }

    pub fn toggle_folder(&mut self, folder_idx: usize) {
        if let Some(f) = self.folders.get_mut(folder_idx) {
            f.expanded = !f.expanded;
        }
    }

    pub fn session(&self, idx: usize) -> Option<&Session> {
        self.sessions.get(idx)
    }

    /// Stable identity of the row at `index` in `visible_rows()`, or `None` if
    /// the index is out of range. Captured before a refresh rebuild so the
    /// cursor can follow its workstream through a recency reorder.
    pub fn key_at(&self, index: usize) -> Option<RowKey> {
        match self.visible_rows().get(index)? {
            Row::Folder(i) => self.folders.get(*i).map(|f| RowKey::Folder(f.prefix.clone())),
            Row::Session(i) => self
                .sessions
                .get(*i)
                .map(|s| RowKey::Session(s.machine.clone(), s.session_id.clone())),
            Row::NewSession => Some(RowKey::NewSession),
        }
    }

    /// Index in `visible_rows()` of the row matching `key`, if it's still
    /// present after a rebuild. `None` when the workstream vanished (killed,
    /// or its folder collapsed) — the caller then falls back to clamping.
    pub fn index_of_key(&self, key: &RowKey) -> Option<usize> {
        self.visible_rows().iter().position(|row| match (row, key) {
            (Row::Folder(i), RowKey::Folder(p)) => {
                self.folders.get(*i).is_some_and(|f| &f.prefix == p)
            }
            (Row::Session(i), RowKey::Session(m, sid)) => self
                .sessions
                .get(*i)
                .is_some_and(|s| &s.machine == m && &s.session_id == sid),
            (Row::NewSession, RowKey::NewSession) => true,
            _ => false,
        })
    }
}

#[cfg(test)]
mod folder_order_tests {
    //! Pins the recency ordering added for "float the session I just typed
    //! into to the top": folders sort by their newest child activity
    //! (`claude_last_activity`), most-recent first, with activity-less
    //! folders falling back to alphabetical order below the active ones.

    use super::*;
    use std::time::Duration;

    /// A `tmux::Session` fixture: `folder/leaf` name + optional activity
    /// timestamp expressed as seconds since the epoch.
    fn tsess(name: &str, activity_secs: Option<u64>) -> tmux::Session {
        tmux::Session {
            name: name.to_string(),
            session_id: format!("${}", name.len()),
            windows: 1,
            attached: false,
            claude: None,
            claude_demoted: false,
            claude_present: false,
            claude_context_pct: None,
            claude_usage: None,
            claude_last_activity: activity_secs
                .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s)),
            // tmux's own `#{session_activity}` (any pane I/O), which folder
            // ordering does not consult — these tests pin ordering by *Claude*
            // activity specifically.
            last_activity: None,
        }
    }

    fn prefixes(tree: &Tree) -> Vec<String> {
        tree.folders.iter().map(|f| f.prefix.clone()).collect()
    }

    fn folder_leaves(tree: &Tree, prefix: &str) -> Vec<String> {
        let f = tree
            .folders
            .iter()
            .find(|f| f.prefix == prefix)
            .expect("folder present");
        f.sessions
            .iter()
            .map(|&i| tree.sessions[i].leaf.clone())
            .collect()
    }

    #[test]
    fn most_recent_activity_folder_sorts_first() {
        // beta is newest, alpha older — beta must lead despite alpha being
        // alphabetically first.
        let per_machine = vec![(
            Machine::Local,
            vec![tsess("alpha/one", Some(100)), tsess("beta/two", Some(200))],
        )];
        let tree = Tree::build(per_machine, vec![], &HashMap::new());
        assert_eq!(prefixes(&tree), vec!["beta", "alpha"]);
    }

    #[test]
    fn activity_folders_precede_inactive_which_stay_alphabetical() {
        // Insertion order deliberately not alphabetical to prove the sort,
        // not the input order, decides placement.
        let per_machine = vec![(
            Machine::Local,
            vec![
                tsess("gamma/x", None),
                tsess("alpha/one", Some(100)),
                tsess("delta/y", None),
                tsess("beta/two", Some(200)),
            ],
        )];
        let tree = Tree::build(per_machine, vec![], &HashMap::new());
        // Active folders first by recency (beta > alpha), then the
        // activity-less folders alphabetically (delta, gamma).
        assert_eq!(prefixes(&tree), vec!["beta", "alpha", "delta", "gamma"]);
    }

    #[test]
    fn folder_activity_is_max_across_its_sessions() {
        // A folder with an old and a fresh session takes the fresh one's
        // timestamp, so it outranks a folder whose single session is in
        // between.
        let per_machine = vec![(
            Machine::Local,
            vec![
                tsess("work/old", Some(10)),
                tsess("work/fresh", Some(300)),
                tsess("solo/one", Some(200)),
            ],
        )];
        let tree = Tree::build(per_machine, vec![], &HashMap::new());
        assert_eq!(prefixes(&tree), vec!["work", "solo"]);
    }

    #[test]
    fn remote_sourced_activity_floats_across_devices() {
        // Distinct folder prefixes per machine (a shared prefix would only
        // test max-within-folder). A remote folder with the freshest activity
        // must outrank a local one, and an activity-less remote folder falls
        // to the alphabetical tail — proving the sort is machine-agnostic and
        // consumes the remote (`seq`-derived) timestamps identically.
        let per_machine = vec![
            (Machine::Local, vec![tsess("localwork/a", Some(100))]),
            (
                Machine::Remote("HumeMosh".into()),
                vec![
                    tsess("remotefresh/x", Some(300)),
                    tsess("remoteidle/y", None),
                ],
            ),
        ];
        let tree = Tree::build(per_machine, vec![], &HashMap::new());
        assert_eq!(
            prefixes(&tree),
            vec!["remotefresh", "localwork", "remoteidle"]
        );
    }

    #[test]
    fn sessions_within_folder_sort_by_recency() {
        // Within one folder: freshest session first, then older, then the
        // no-activity one (kept in backend order at the tail).
        let per_machine = vec![(
            Machine::Local,
            vec![
                tsess("work/old", Some(100)),
                tsess("work/quiet", None),
                tsess("work/fresh", Some(300)),
            ],
        )];
        let tree = Tree::build(per_machine, vec![], &HashMap::new());
        assert_eq!(folder_leaves(&tree, "work"), vec!["fresh", "old", "quiet"]);
    }

    #[test]
    fn cursor_key_follows_session_across_reorder() {
        // Helper: alpha & beta folders, each one session, parameterised times.
        let mk = |alpha: u64, beta: u64| {
            vec![
                tsess("alpha/one", Some(alpha)),
                tsess("beta/two", Some(beta)),
            ]
        };
        // tree1: alpha fresher → alpha folder on top; cursor on beta's session.
        let tree1 = Tree::build(vec![(Machine::Local, mk(300, 100))], vec![], &HashMap::new());
        let beta_row = tree1
            .visible_rows()
            .iter()
            .position(|r| matches!(r, Row::Session(i) if tree1.sessions[*i].leaf == "two"))
            .unwrap();
        let key = tree1.key_at(beta_row).unwrap();
        assert!(matches!(key, RowKey::Session(_, _)));

        // tree2: beta now fresher → beta floats up. The SAME key must resolve
        // to beta/two's new row, not whatever slid into the old position.
        let tree2 = Tree::build(vec![(Machine::Local, mk(100, 300))], vec![], &HashMap::new());
        let new_idx = tree2.index_of_key(&key).expect("cursor key still present");
        match tree2.visible_rows()[new_idx] {
            Row::Session(i) => assert_eq!(tree2.sessions[i].leaf, "two"),
            other => panic!("expected beta/two session row, got {other:?}"),
        }
        assert_ne!(new_idx, beta_row, "beta floated up, so its row index changed");
    }

    #[test]
    fn cursor_key_new_session_row_is_stable() {
        let tree = Tree::build(
            vec![(Machine::Local, vec![tsess("a/one", Some(1))])],
            vec![],
            &HashMap::new(),
        );
        let last = tree.visible_rows().len() - 1;
        assert_eq!(tree.key_at(last), Some(RowKey::NewSession));
        assert_eq!(tree.index_of_key(&RowKey::NewSession), Some(last));
    }

    #[test]
    fn cursor_key_of_vanished_session_is_none() {
        let tree = Tree::build(
            vec![(Machine::Local, vec![tsess("a/one", Some(1))])],
            vec![],
            &HashMap::new(),
        );
        // A session key that isn't in this tree → None (caller clamps).
        let gone = RowKey::Session(Machine::Local, "$999".to_string());
        assert_eq!(tree.index_of_key(&gone), None);
    }
}
