use std::collections::HashMap;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cwd;
use crate::hosts::{Config, Host, HostKind};
use crate::install_hooks;
use crate::install_tmux::InstallStatus;
use crate::model::{Machine, Row, Tree};
use crate::refresh::{refresh_all, RefreshResult};
use crate::state::State;
use crate::text_field::TextField;
use crate::tmux::{self, TmuxBackend};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How often the TUI fires a background refresh while idle. Local backend is
/// cheap; remote backends are spawned in parallel threads and bounded by the
/// per-host SSH ConnectTimeout. Tuning point if it ever feels janky.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
}

#[allow(dead_code)]
impl Notice {
    pub fn success(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Success,
            text: text.into(),
        }
    }
    pub fn warning(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Warning,
            text: text.into(),
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Error,
            text: text.into(),
        }
    }
    pub fn info(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Info,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreateField {
    Machine,
    Prefix,
    Name,
}

impl CreateField {
    pub fn next(self) -> Self {
        match self {
            CreateField::Machine => CreateField::Prefix,
            CreateField::Prefix => CreateField::Name,
            CreateField::Name => CreateField::Machine,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            CreateField::Machine => CreateField::Name,
            CreateField::Prefix => CreateField::Machine,
            CreateField::Name => CreateField::Prefix,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateForm {
    pub machine: Machine,
    pub prefix: TextField,
    pub name: TextField,
    pub focus: CreateField,
}

impl CreateForm {
    pub fn new() -> Self {
        let prefix_str = cwd::guess_prefix().unwrap_or_default();
        let focus = if prefix_str.is_empty() {
            CreateField::Prefix
        } else {
            CreateField::Name
        };
        Self {
            machine: Machine::Local,
            prefix: TextField::from_str(&prefix_str),
            name: TextField::new(),
            focus,
        }
    }

    pub fn final_name(&self) -> String {
        let p = self.prefix.trim();
        let n = self.name.trim();
        if p.is_empty() {
            n.to_string()
        } else if n.is_empty() {
            p.to_string()
        } else {
            format!("{}/{}", p, n)
        }
    }

    pub fn is_valid(&self) -> bool {
        let final_name = self.final_name();
        !final_name.is_empty() && !final_name.starts_with('/') && !final_name.ends_with('/')
    }

    /// Returns a mutable reference to the TextField currently focused, if the
    /// focus is on a text field (not the Machine chip).
    pub fn focused_field_mut(&mut self) -> Option<&mut TextField> {
        match self.focus {
            CreateField::Machine => None,
            CreateField::Prefix => Some(&mut self.prefix),
            CreateField::Name => Some(&mut self.name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostField {
    Name,
    Kind,
    Target,
    SshArgs,
}

impl HostField {
    pub fn next(self) -> Self {
        match self {
            HostField::Name => HostField::Kind,
            HostField::Kind => HostField::Target,
            HostField::Target => HostField::SshArgs,
            HostField::SshArgs => HostField::Name,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            HostField::Name => HostField::SshArgs,
            HostField::Kind => HostField::Name,
            HostField::Target => HostField::Kind,
            HostField::SshArgs => HostField::Target,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostForm {
    pub name: TextField,
    pub kind: HostKind,
    pub target: TextField,
    pub ssh_args: TextField,
    pub focus: HostField,
    pub editing_idx: Option<usize>,
}

impl HostForm {
    pub fn new() -> Self {
        Self {
            name: TextField::new(),
            kind: HostKind::Ssh,
            target: TextField::new(),
            ssh_args: TextField::new(),
            focus: HostField::Name,
            editing_idx: None,
        }
    }

    pub fn from_host(idx: usize, h: &Host) -> Self {
        Self {
            name: TextField::from_str(&h.name),
            kind: h.kind,
            target: TextField::from_str(&h.target),
            ssh_args: TextField::from_str(&h.ssh_args.join(" ")),
            focus: HostField::Name,
            editing_idx: Some(idx),
        }
    }

    pub fn to_host(&self) -> Host {
        Host {
            name: self.name.trim().to_string(),
            kind: self.kind,
            target: self.target.trim().to_string(),
            ssh_args: self
                .ssh_args
                .as_str()
                .split_whitespace()
                .map(String::from)
                .collect(),
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.name.trim().is_empty() && !self.target.trim().is_empty()
    }

    pub fn focused_field_mut(&mut self) -> Option<&mut TextField> {
        match self.focus {
            HostField::Kind => None,
            HostField::Name => Some(&mut self.name),
            HostField::Target => Some(&mut self.target),
            HostField::SshArgs => Some(&mut self.ssh_args),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Tree,
    CreatingSession(CreateForm),
    RenamingSession {
        original_name: String,
        machine: Machine,
    },
    RenamingFolder {
        original_prefix: String,
    },
    Confirming(PendingConfirm),
    HostsList {
        selected: usize,
    },
    HostForm(HostForm),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirm {
    pub title: String,
    pub body: Vec<String>,
    pub action: PendingAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingAction {
    KillSession {
        machine: Machine,
        name: String,
    },
    /// Strip the `{prefix}/` part from each child session, demoting them to
    /// loose sessions. The folder dissolves automatically once no session
    /// shares the prefix anymore. Sessions are never killed.
    DissolveFolder {
        prefix: String,
        targets: Vec<(Machine, String, String)>,
    },
    RenameFolder {
        from: String,
        to: String,
        targets: Vec<(Machine, String, String)>,
    },
    DeleteHost {
        idx: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SessionAction {
    #[default]
    Enter,
    Rename,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FocusArea {
    #[default]
    SessionList,
    TitleBar,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppAction {
    None,
    AttachSession { name: String, machine: Machine },
    Quit,
}

pub struct App {
    pub state: AppState,
    pub focus_area: FocusArea,
    pub tree: Tree,
    pub selected_index: usize,
    pub selected_action: SessionAction,
    pub input_buffer: TextField,
    pub should_quit: bool,
    pub action: AppAction,
    pub error_message: Option<String>,
    pub expanded_memory: HashMap<String, bool>,
    pub config: Config,
    /// Per-host hook install state, populated each refresh from the SSH
    /// query. `Some(true)` = installed, `Some(false)` = missing, `None` =
    /// unreachable / couldn't determine.
    pub host_hooks: HashMap<String, Option<bool>>,
    /// Same idea for the local machine — checked on every refresh by
    /// reading `~/.claude/settings.local.json`.
    pub local_hooks_installed: bool,
    /// Install state of ADE's tmux clipboard config locally. Drives the
    /// in-TUI nudge that points users at `ade install-tmux-config`.
    pub local_tmux_config_status: InstallStatus,
    /// True if the user has dismissed the "tmux clipboard not configured"
    /// nudge. Loaded from `~/.config/ade/state.toml` on launch and persisted
    /// when the user presses `x`.
    pub tmux_nudge_dismissed: bool,
    /// Transient banner shown at the top of the Hosts screen (install /
    /// retry results). Cleared on the next keypress in HostsList.
    pub hosts_notice: Option<Notice>,
    /// Background refresh worker, if one is in flight. The TUI never blocks
    /// on this — `tick()` polls `is_finished()` and applies the result when
    /// ready. Only one runs at a time; manual `r` cancels by dropping it.
    pending_refresh: Option<JoinHandle<RefreshResult>>,
    /// When the most recent refresh (sync or background) was *started*. Used
    /// by `tick()` to decide when the next background refresh is due.
    last_refresh_started: Instant,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let (config, parse_warning) = Config::load();
        let persisted = State::load();
        let mut app = Self {
            state: AppState::Tree,
            focus_area: FocusArea::SessionList,
            tree: Tree::default(),
            selected_index: 0,
            selected_action: SessionAction::default(),
            input_buffer: TextField::new(),
            should_quit: false,
            action: AppAction::None,
            error_message: parse_warning,
            expanded_memory: HashMap::new(),
            config,
            host_hooks: HashMap::new(),
            local_hooks_installed: false,
            local_tmux_config_status: InstallStatus::Missing,
            tmux_nudge_dismissed: persisted.tmux_install_nudge.dismissed,
            hosts_notice: None,
            pending_refresh: None,
            last_refresh_started: Instant::now(),
        };
        app.refresh();
        app
    }

    fn backend(&self, m: &Machine) -> Option<Box<dyn TmuxBackend>> {
        tmux::backend_for(m, &self.config.hosts)
    }

    /// All machines available for new-session creation: Local + every configured host.
    pub fn available_machines(&self) -> Vec<Machine> {
        let mut out = vec![Machine::Local];
        for h in &self.config.hosts {
            out.push(Machine::Remote(h.name.clone()));
        }
        out
    }

    pub fn refresh(&mut self) {
        // Drop any in-flight background refresh — the worker keeps running
        // but its result will be discarded. We want the user's `r` to feel
        // immediate, not "wait for the previous tick to finish".
        self.pending_refresh = None;
        let result = refresh_all(&self.config);
        self.apply_refresh_result(result);
        self.last_refresh_started = Instant::now();
        // Manual refresh resets the per-row action toggle. Background ticks
        // do not — preserving whatever action the user has cycled to.
        self.selected_action = SessionAction::Enter;
    }

    /// Apply a refresh result (from sync or background) to App state.
    /// Snapshots current expansion state first so user toggles aren't lost.
    fn apply_refresh_result(&mut self, result: RefreshResult) {
        for (k, v) in self.tree.expanded_snapshot() {
            self.expanded_memory.insert(k, v);
        }
        self.host_hooks = result.remote_hooks;
        self.local_hooks_installed = result.local_hooks_installed;
        self.local_tmux_config_status = result.local_tmux_config_status;
        let current_session = result.current_session;
        self.tree = Tree::build(result.per_machine, result.errors, &self.expanded_memory);
        self.tree.current_session = current_session;

        let n = self.tree.visible_rows().len().max(1);
        if self.selected_index >= n {
            self.selected_index = n - 1;
        }
    }

    /// Called once per event-loop iteration. Applies a finished background
    /// refresh and schedules a new one when the interval has elapsed.
    /// Non-blocking: if the worker is still running, just leaves it alone.
    pub fn tick(&mut self) {
        if let Some(handle) = self.pending_refresh.take() {
            if handle.is_finished() {
                if let Ok(result) = handle.join() {
                    self.apply_refresh_result(result);
                }
            } else {
                self.pending_refresh = Some(handle);
            }
        }

        if self.pending_refresh.is_none()
            && self.last_refresh_started.elapsed() >= AUTO_REFRESH_INTERVAL
        {
            let config = self.config.clone();
            self.pending_refresh =
                Some(std::thread::spawn(move || refresh_all(&config)));
            self.last_refresh_started = Instant::now();
        }
    }

    pub fn current_row(&self) -> Option<Row> {
        self.tree.visible_rows().get(self.selected_index).copied()
    }

    /// True when the in-TUI "tmux clipboard not configured" nudge should be
    /// rendered. Only shown when ADE is running inside tmux (otherwise the
    /// fix isn't relevant), the marker is missing locally, the user hasn't
    /// already dismissed it, and we're in the main tree state — so the `x`
    /// dismissal binding is always reachable when the nudge is visible
    /// rather than being shadowed by a modal's own keymap.
    pub fn should_show_tmux_nudge(&self) -> bool {
        !self.tmux_nudge_dismissed
            && matches!(self.state, AppState::Tree)
            && tmux::is_inside_tmux()
            && matches!(self.local_tmux_config_status, InstallStatus::Missing)
    }

    /// Persist the dismissal so we don't re-pester after restart. State
    /// persistence is best-effort — failing to save doesn't block the UI.
    fn dismiss_tmux_nudge(&mut self) {
        self.tmux_nudge_dismissed = true;
        let mut state = State::load();
        state.tmux_install_nudge.dismissed = true;
        let _ = state.save();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.error_message = None;
        match &self.state {
            AppState::Tree => self.handle_tree_key(key),
            AppState::CreatingSession(_) => self.handle_creating_session_key(key),
            AppState::RenamingSession { .. } => self.handle_renaming_session_key(key),
            AppState::RenamingFolder { .. } => self.handle_renaming_folder_key(key),
            AppState::Confirming(_) => self.handle_confirming_key(key),
            AppState::HostsList { .. } => self.handle_hosts_list_key(key),
            AppState::HostForm(_) => self.handle_host_form_key(key),
        }
    }

    fn handle_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus_area == FocusArea::TitleBar {
                    // already at top
                } else if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.selected_action = SessionAction::Enter;
                } else {
                    self.focus_area = FocusArea::TitleBar;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let n = self.tree.visible_rows().len();
                if self.focus_area == FocusArea::TitleBar {
                    self.focus_area = FocusArea::SessionList;
                } else if self.selected_index + 1 < n {
                    self.selected_index += 1;
                    self.selected_action = SessionAction::Enter;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if matches!(
                    self.current_row(),
                    Some(Row::Session(_)) | Some(Row::Folder(_))
                ) {
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::Rename,
                        SessionAction::Rename => SessionAction::Delete,
                        SessionAction::Delete => SessionAction::Delete,
                    };
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if matches!(
                    self.current_row(),
                    Some(Row::Session(_)) | Some(Row::Folder(_))
                ) {
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::Enter,
                        SessionAction::Rename => SessionAction::Enter,
                        SessionAction::Delete => SessionAction::Rename,
                    };
                }
            }
            KeyCode::Char('o') | KeyCode::Char(' ') => {
                if let Some(Row::Folder(idx)) = self.current_row() {
                    self.tree.toggle_folder(idx);
                }
            }
            KeyCode::Enter => {
                if self.focus_area == FocusArea::TitleBar {
                    self.refresh();
                    self.focus_area = FocusArea::SessionList;
                } else {
                    self.activate_current();
                }
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('g') => {
                if self.focus_area == FocusArea::SessionList {
                    self.selected_index = 0;
                    self.selected_action = SessionAction::Enter;
                }
            }
            KeyCode::Char('G') => {
                if self.focus_area == FocusArea::SessionList {
                    let n = self.tree.visible_rows().len();
                    if n > 0 {
                        self.selected_index = n - 1;
                    }
                    self.selected_action = SessionAction::Enter;
                }
            }
            KeyCode::Char('n') => {
                self.state = AppState::CreatingSession(CreateForm::new());
                self.input_buffer.clear();
            }
            KeyCode::Char('R') => self.start_rename_selected(),
            KeyCode::Char('d') => self.start_delete_selected(),
            KeyCode::Char('H') => {
                self.state = AppState::HostsList { selected: 0 };
            }
            KeyCode::Char('x') => {
                if self.should_show_tmux_nudge() {
                    self.dismiss_tmux_nudge();
                }
            }
            _ => {}
        }
    }

    fn start_rename_selected(&mut self) {
        match self.current_row() {
            Some(Row::Session(idx)) => {
                let Some(session) = self.tree.session(idx) else {
                    return;
                };
                let raw = session.raw_name.clone();
                let machine = session.machine.clone();
                self.state = AppState::RenamingSession {
                    original_name: raw.clone(),
                    machine,
                };
                self.input_buffer = TextField::from_str(&raw);
            }
            Some(Row::Folder(idx)) => {
                let Some(folder) = self.tree.folders.get(idx) else {
                    return;
                };
                let prefix = folder.prefix.clone();
                self.state = AppState::RenamingFolder {
                    original_prefix: prefix.clone(),
                };
                self.input_buffer = TextField::from_str(&prefix);
            }
            _ => {}
        }
    }

    fn start_delete_selected(&mut self) {
        match self.current_row() {
            Some(Row::Session(idx)) => {
                let Some(session) = self.tree.session(idx) else {
                    return;
                };
                let body = vec![
                    format!(
                        "Kill session {} on {}?",
                        session.raw_name,
                        session.machine.label()
                    ),
                    "All windows and unsaved work will be lost.".to_string(),
                ];
                self.state = AppState::Confirming(PendingConfirm {
                    title: "Delete session".to_string(),
                    body,
                    action: PendingAction::KillSession {
                        machine: session.machine.clone(),
                        name: session.raw_name.clone(),
                    },
                });
            }
            Some(Row::Folder(idx)) => {
                let Some(folder) = self.tree.folders.get(idx) else {
                    return;
                };
                let prefix = folder.prefix.clone();
                // Build rename targets that strip the `{prefix}/` portion of
                // each child's name (folder dissolves; sessions stay alive).
                let targets: Vec<(Machine, String, String)> = folder
                    .sessions
                    .iter()
                    .filter_map(|&i| self.tree.sessions.get(i))
                    .map(|s| (s.machine.clone(), s.raw_name.clone(), s.leaf.clone()))
                    .collect();
                let mut body = vec![format!(
                    "Dissolve folder \"{}\" — strip \"{}/\" from {} session{}:",
                    prefix,
                    prefix,
                    targets.len(),
                    if targets.len() == 1 { "" } else { "s" }
                )];
                for (m, from, to) in &targets {
                    body.push(format!("  • {} → {} ({})", from, to, m.label()));
                }
                self.state = AppState::Confirming(PendingConfirm {
                    title: "Dissolve folder".to_string(),
                    body,
                    action: PendingAction::DissolveFolder { prefix, targets },
                });
            }
            _ => {}
        }
    }

    fn activate_current(&mut self) {
        match self.current_row() {
            Some(Row::Folder(idx)) => match self.selected_action {
                SessionAction::Enter => self.tree.toggle_folder(idx),
                SessionAction::Rename => self.start_rename_selected(),
                SessionAction::Delete => self.start_delete_selected(),
            },
            Some(Row::Session(idx)) => {
                let session = match self.tree.session(idx) {
                    Some(s) => s.clone(),
                    None => return,
                };
                match self.selected_action {
                    SessionAction::Enter => {
                        // The same-session-no-op case is handled in
                        // main.rs::attach: it just returns early so ADE quits
                        // and the user stays in the session they were in,
                        // which is exactly what they wanted.
                        self.action = AppAction::AttachSession {
                            name: session.raw_name,
                            machine: session.machine,
                        };
                    }
                    SessionAction::Rename => {
                        self.state = AppState::RenamingSession {
                            original_name: session.raw_name.clone(),
                            machine: session.machine,
                        };
                        self.input_buffer = TextField::from_str(&session.raw_name);
                    }
                    SessionAction::Delete => self.start_delete_selected(),
                }
            }
            Some(Row::NewSession) => {
                self.state = AppState::CreatingSession(CreateForm::new());
                self.input_buffer.clear();
            }
            None => {}
        }
    }

    fn execute_kill_session(&mut self, machine: Machine, name: &str) {
        let result = match self.backend(&machine) {
            Some(b) => b.kill_session(name),
            None => Err(format!("unknown host: {}", machine.label())),
        };
        match result {
            Ok(()) => {
                self.refresh();
                self.selected_action = SessionAction::Enter;
            }
            Err(e) => self.error_message = Some(e),
        }
    }

    /// Run a batch of session renames across machines. Used by both the
    /// folder-rename cascade and the folder-dissolve operation — both reduce
    /// to "rename N sessions, possibly across multiple hosts".
    fn execute_renames(&mut self, targets: Vec<(Machine, String, String)>) {
        let mut errors: Vec<String> = Vec::new();
        for (machine, from, to) in &targets {
            let result = match self.backend(machine) {
                Some(b) => b.rename_session(from, to),
                None => Err(format!("unknown host: {}", machine.label())),
            };
            if let Err(e) = result {
                errors.push(format!("{}: {}", from, e));
            }
        }
        if !errors.is_empty() {
            self.error_message = Some(errors.join("; "));
        }
        self.refresh();
        self.selected_action = SessionAction::Enter;
    }

    fn handle_creating_session_key(&mut self, key: KeyEvent) {
        let machines = self.available_machines();
        let AppState::CreatingSession(ref mut form) = self.state else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                self.state = AppState::Tree;
            }
            KeyCode::Tab | KeyCode::Down => form.focus = form.focus.next(),
            KeyCode::BackTab | KeyCode::Up => form.focus = form.focus.prev(),
            KeyCode::Enter => {
                if form.is_valid() {
                    let snapshot = form.clone();
                    self.create_and_attach_session(snapshot);
                }
            }
            KeyCode::Left => {
                if form.focus == CreateField::Machine {
                    cycle_machine(&mut form.machine, &machines, false);
                } else if let Some(f) = form.focused_field_mut() {
                    f.move_left();
                }
            }
            KeyCode::Right => {
                if form.focus == CreateField::Machine {
                    cycle_machine(&mut form.machine, &machines, true);
                } else if let Some(f) = form.focused_field_mut() {
                    f.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(f) = form.focused_field_mut() {
                    f.move_home();
                }
            }
            KeyCode::End => {
                if let Some(f) = form.focused_field_mut() {
                    f.move_end();
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = form.focused_field_mut() {
                    f.delete_left();
                }
            }
            KeyCode::Delete => {
                if let Some(f) = form.focused_field_mut() {
                    f.delete_right();
                }
            }
            KeyCode::Char(c) => match form.focus {
                CreateField::Machine => {
                    if c == 'h' || c == 'H' {
                        cycle_machine(&mut form.machine, &machines, false);
                    } else if c == 'l' || c == 'L' {
                        cycle_machine(&mut form.machine, &machines, true);
                    }
                }
                CreateField::Prefix if c.is_alphanumeric() || c == '-' || c == '_' => {
                    form.prefix.insert(c);
                }
                CreateField::Name if c.is_alphanumeric() || c == '-' || c == '_' => {
                    form.name.insert(c);
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn create_and_attach_session(&mut self, form: CreateForm) {
        let final_name = form.final_name();
        let result = match self.backend(&form.machine) {
            Some(b) => b.create_session(&final_name),
            None => Err(format!("unknown host: {}", form.machine.label())),
        };
        match result {
            Ok(()) => {
                self.action = AppAction::AttachSession {
                    name: final_name,
                    machine: form.machine,
                };
            }
            Err(e) => {
                self.error_message = Some(e);
                self.state = AppState::Tree;
            }
        }
    }

    fn handle_renaming_session_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
            }
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() {
                    self.rename_current_session();
                }
            }
            KeyCode::Backspace => self.input_buffer.delete_left(),
            KeyCode::Delete => self.input_buffer.delete_right(),
            KeyCode::Left => self.input_buffer.move_left(),
            KeyCode::Right => self.input_buffer.move_right(),
            KeyCode::Home => self.input_buffer.move_home(),
            KeyCode::End => self.input_buffer.move_end(),
            KeyCode::Char(c) => {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' {
                    self.input_buffer.insert(c);
                }
            }
            _ => {}
        }
    }

    fn handle_renaming_folder_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state = AppState::Tree;
                self.input_buffer.clear();
            }
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() {
                    self.commit_folder_rename_to_confirm();
                }
            }
            KeyCode::Backspace => self.input_buffer.delete_left(),
            KeyCode::Delete => self.input_buffer.delete_right(),
            KeyCode::Left => self.input_buffer.move_left(),
            KeyCode::Right => self.input_buffer.move_right(),
            KeyCode::Home => self.input_buffer.move_home(),
            KeyCode::End => self.input_buffer.move_end(),
            KeyCode::Char(c) => {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    self.input_buffer.insert(c);
                }
            }
            _ => {}
        }
    }

    fn commit_folder_rename_to_confirm(&mut self) {
        let new_prefix = self.input_buffer.trim().to_string();
        if new_prefix.is_empty() {
            return;
        }
        let original = if let AppState::RenamingFolder { original_prefix } = &self.state {
            original_prefix.clone()
        } else {
            return;
        };
        if new_prefix == original {
            self.state = AppState::Tree;
            self.input_buffer.clear();
            return;
        }

        let folder = self
            .tree
            .folders
            .iter()
            .find(|f| f.prefix == original)
            .cloned();
        let Some(folder) = folder else {
            self.state = AppState::Tree;
            self.input_buffer.clear();
            return;
        };

        let targets: Vec<(Machine, String, String)> = folder
            .sessions
            .iter()
            .filter_map(|&i| self.tree.sessions.get(i))
            .map(|s| {
                let new_name = format!("{}/{}", new_prefix, s.leaf);
                (s.machine.clone(), s.raw_name.clone(), new_name)
            })
            .collect();

        let mut body = vec![format!(
            "Rename folder \"{}\" → \"{}\" — {} session{}:",
            original,
            new_prefix,
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        )];
        for (m, from, to) in &targets {
            body.push(format!("  • {} → {} ({})", from, to, m.label()));
        }

        self.state = AppState::Confirming(PendingConfirm {
            title: "Rename folder".to_string(),
            body,
            action: PendingAction::RenameFolder {
                from: original,
                to: new_prefix,
                targets,
            },
        });
        self.input_buffer.clear();
    }

    fn handle_confirming_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.state = AppState::Tree;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let action = if let AppState::Confirming(ref c) = self.state {
                    c.action.clone()
                } else {
                    return;
                };
                self.state = AppState::Tree;
                match action {
                    PendingAction::KillSession { machine, name } => {
                        self.execute_kill_session(machine, &name);
                    }
                    PendingAction::DissolveFolder { targets, .. } => {
                        self.execute_renames(targets);
                    }
                    PendingAction::RenameFolder { targets, .. } => {
                        self.execute_renames(targets);
                    }
                    PendingAction::DeleteHost { idx } => {
                        let mut tentative = self.config.clone();
                        tentative.remove(idx);
                        match tentative.save() {
                            Ok(()) => {
                                self.config = tentative;
                                self.refresh();
                            }
                            Err(e) => self.error_message = Some(e),
                        }
                        self.state = AppState::HostsList {
                            selected: idx.saturating_sub(1),
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn rename_current_session(&mut self) {
        let new_name = self.input_buffer.trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let (original, machine) = if let AppState::RenamingSession {
            original_name,
            machine,
        } = &self.state
        {
            (original_name.clone(), machine.clone())
        } else {
            return;
        };
        let result = match self.backend(&machine) {
            Some(b) => b.rename_session(&original, &new_name),
            None => Err(format!("unknown host: {}", machine.label())),
        };
        match result {
            Ok(()) => {
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
                self.refresh();
            }
            Err(e) => {
                self.error_message = Some(e);
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
            }
        }
    }

    // --- Hosts management ---

    fn handle_hosts_list_key(&mut self, key: KeyEvent) {
        // Any keypress inside the Hosts screen dismisses a stale install
        // notice from a previous action.
        self.hosts_notice = None;

        let n = self.config.hosts.len();
        let selected_host_name: Option<String> = if let AppState::HostsList { selected } = self.state {
            self.config.hosts.get(selected).map(|h| h.name.clone())
        } else {
            None
        };

        let AppState::HostsList { ref mut selected } = self.state else {
            return;
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('H') => {
                self.state = AppState::Tree;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if *selected > 0 {
                    *selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 && *selected + 1 < n {
                    *selected += 1;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                self.state = AppState::HostForm(HostForm::new());
            }
            KeyCode::Enter | KeyCode::Char('R') | KeyCode::Char('e') => {
                if let Some(host) = self.config.hosts.get(*selected) {
                    self.state = AppState::HostForm(HostForm::from_host(*selected, host));
                }
            }
            KeyCode::Char('d') => {
                if let Some(host) = self.config.hosts.get(*selected) {
                    let body = vec![
                        format!("Delete host \"{}\"?", host.name),
                        "Sessions on this host will disappear from the tree until re-added."
                            .to_string(),
                    ];
                    self.state = AppState::Confirming(PendingConfirm {
                        title: "Delete host".to_string(),
                        body,
                        action: PendingAction::DeleteHost { idx: *selected },
                    });
                }
            }
            KeyCode::Char('i') => {
                if let Some(name) = selected_host_name {
                    self.install_remote_hooks(&name);
                }
            }
            KeyCode::Char('L') => {
                self.install_local_hooks();
            }
            _ => {}
        }
    }

    fn install_remote_hooks(&mut self, host_name: &str) {
        match install_hooks::install_remote(&self.config, host_name) {
            Ok(msg) => {
                self.hosts_notice = Some(Notice::success(msg));
                self.refresh();
            }
            Err(e) => {
                self.hosts_notice = Some(Notice::error(format!("{}: {}", host_name, e)));
            }
        }
    }

    fn install_local_hooks(&mut self) {
        match install_hooks::install_local() {
            Ok(msg) => {
                self.hosts_notice = Some(Notice::success(msg));
                self.refresh();
            }
            Err(e) => {
                self.hosts_notice = Some(Notice::error(format!("local: {}", e)));
            }
        }
    }

    fn handle_host_form_key(&mut self, key: KeyEvent) {
        let AppState::HostForm(ref mut form) = self.state else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                self.state = AppState::HostsList {
                    selected: form.editing_idx.unwrap_or(0),
                };
            }
            KeyCode::Tab | KeyCode::Down => form.focus = form.focus.next(),
            KeyCode::BackTab | KeyCode::Up => form.focus = form.focus.prev(),
            KeyCode::Enter => {
                if form.is_valid() {
                    let host = form.to_host();
                    let host_name = host.name.clone();
                    let editing_idx = form.editing_idx;
                    // Save-then-commit: mutate a clone, write to disk first,
                    // and only swap into self.config if both succeed. Keeps
                    // disk and memory in sync even if the save fails.
                    let mut tentative = self.config.clone();
                    match tentative
                        .upsert(host, editing_idx)
                        .and_then(|()| tentative.save())
                    {
                        Ok(()) => {
                            self.config = tentative;
                            // Auto-install ADE hooks on the new/edited host
                            // so live status detection just works. Result
                            // surfaces in the hosts screen banner.
                            let install_result =
                                install_hooks::install_remote(&self.config, &host_name);
                            // Only nudge tmux-config install for new mosh
                            // hosts — that's the configuration where OSC 52
                            // gets dropped in transit. SSH passes bytes
                            // unchanged.
                            let new_mosh = editing_idx.is_none()
                                && self
                                    .config
                                    .host_by_name(&host_name)
                                    .map(|h| matches!(h.kind, HostKind::Mosh))
                                    .unwrap_or(false);
                            self.refresh();
                            self.hosts_notice = match install_result {
                                Ok(mut msg) => {
                                    if new_mosh {
                                        msg.push_str(&format!(
                                            ". Tip: `ade install-tmux-config --host {}` \
                                             to set up clipboard there.",
                                            host_name
                                        ));
                                    }
                                    Some(Notice::success(msg))
                                }
                                Err(e) => Some(Notice::warning(format!(
                                    "saved {} — hooks not installed: {}. Press i to retry.",
                                    host_name, e
                                ))),
                            };
                            let new_selected = editing_idx
                                .unwrap_or_else(|| self.config.hosts.len().saturating_sub(1));
                            self.state = AppState::HostsList {
                                selected: new_selected,
                            };
                        }
                        Err(e) => self.error_message = Some(e),
                    }
                }
            }
            KeyCode::Left => {
                if form.focus == HostField::Kind {
                    form.kind = match form.kind {
                        HostKind::Ssh => HostKind::Mosh,
                        HostKind::Mosh => HostKind::Ssh,
                    };
                } else if let Some(f) = form.focused_field_mut() {
                    f.move_left();
                }
            }
            KeyCode::Right => {
                if form.focus == HostField::Kind {
                    form.kind = match form.kind {
                        HostKind::Ssh => HostKind::Mosh,
                        HostKind::Mosh => HostKind::Ssh,
                    };
                } else if let Some(f) = form.focused_field_mut() {
                    f.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(f) = form.focused_field_mut() {
                    f.move_home();
                }
            }
            KeyCode::End => {
                if let Some(f) = form.focused_field_mut() {
                    f.move_end();
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = form.focused_field_mut() {
                    f.delete_left();
                }
            }
            KeyCode::Delete => {
                if let Some(f) = form.focused_field_mut() {
                    f.delete_right();
                }
            }
            KeyCode::Char(c) => match form.focus {
                HostField::Name => {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        form.name.insert(c);
                    }
                }
                HostField::Target => {
                    // Targets allow user@host, IPs, hostnames — broad set of chars.
                    if !c.is_control() && c != ' ' {
                        form.target.insert(c);
                    }
                }
                HostField::SshArgs => {
                    // Allow most chars including spaces for multi-arg input.
                    if !c.is_control() {
                        form.ssh_args.insert(c);
                    }
                }
                HostField::Kind => {
                    if c == 'h' || c == 'H' {
                        form.kind = HostKind::Ssh;
                    } else if c == 'l' || c == 'L' {
                        form.kind = HostKind::Mosh;
                    }
                }
            },
            _ => {}
        }
    }
}

fn cycle_machine(current: &mut Machine, machines: &[Machine], forward: bool) {
    if machines.is_empty() {
        return;
    }
    let idx = machines
        .iter()
        .position(|m| m == current)
        .unwrap_or(0);
    let new_idx = if forward {
        (idx + 1) % machines.len()
    } else {
        (idx + machines.len() - 1) % machines.len()
    };
    *current = machines[new_idx].clone();
}
