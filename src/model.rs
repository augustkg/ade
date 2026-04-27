use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    pub prefix: Option<String>,
    pub leaf: String,
    pub windows: u32,
    pub attached: bool,
    pub machine: Machine,
    pub claude: Option<ClaudeState>,
}

impl Session {
    pub fn from_tmux(s: tmux::Session, machine: Machine) -> Self {
        // Folder/leaf separator is `/`. tmux silently rewrites `:` and `.`
        // in session names, so `:` is unusable as a grouping convention; `/`
        // passes through untouched.
        let (prefix, leaf) = match s.name.split_once('/') {
            Some((p, l)) if !p.is_empty() && !l.is_empty() => {
                (Some(p.to_string()), l.to_string())
            }
            _ => (None, s.name.clone()),
        };
        Self {
            raw_name: s.name,
            prefix,
            leaf,
            windows: s.windows,
            attached: s.attached,
            machine,
            claude: s.claude,
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
        let folders: Vec<Folder> = by_prefix
            .into_iter()
            .map(|(prefix, idxs)| {
                let mut machines = BTreeSet::new();
                let mut claude: Option<ClaudeState> = None;
                for &i in &idxs {
                    machines.insert(sessions[i].machine.clone());
                    if let Some(state) = sessions[i].claude {
                        claude = Some(match claude {
                            Some(cur) => cur.max(state),
                            None => state,
                        });
                    }
                }
                let expanded = *prev_expanded.get(&prefix).unwrap_or(&true);
                Folder {
                    prefix,
                    expanded,
                    sessions: idxs,
                    machines,
                    claude,
                }
            })
            .collect();
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
}
