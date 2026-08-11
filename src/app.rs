use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::claude_status::ClaudeState;
use crate::cwd;
use crate::embedded_term::{chord_step, translate_mouse, ChordOutcome, ChordState, EmbeddedTerm};
use crate::notifications;
use crate::hosts::{Config, Host, HostKind};
use crate::install_hooks;
use crate::install_tmux::InstallStatus;
use crate::kanban::{self, KanbanConfig};
use crate::mail;
use crate::mail_delivery::{self, ComposerState, DeliveryEnv, DeliveryRequest, Gate, Outcome};
use crate::model::{Machine, Row, RowKey, Tree};
use crate::preview_pane::{PreviewKey, PreviewPane};
use crate::refresh::{refresh_all, RefreshResult};
use crate::state::State;
use crate::text_field::TextField;
use crate::tmux::{self, TmuxBackend};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How often the TUI fires a background refresh while idle. Local backend is
/// cheap; remote backends are spawned in parallel threads and bounded by the
/// per-host SSH ConnectTimeout. Tuning point if it ever feels janky.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Per-session notification debounce — suppresses repeats within this
/// window. Defends against status-file flapping (e.g. `Stop` immediately
/// followed by another `UserPromptSubmit`, or `permission_prompt` firing
/// multiple times during a single approval flow). Conservative default;
/// raise if real users report banner spam.
const NOTIFICATION_DEBOUNCE: Duration = Duration::from_secs(5);

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

    /// Build the form with an explicit prefix (skip the cwd guess). Used when
    /// `n` or the folder action chip is invoked from a row that already
    /// carries folder context, so the user only has to type the leaf.
    pub fn with_prefix(prefix: &str) -> Self {
        let focus = if prefix.is_empty() {
            CreateField::Prefix
        } else {
            CreateField::Name
        };
        Self {
            machine: Machine::Local,
            prefix: TextField::from_str(prefix),
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

/// Cursor state for the kanban board view. Focus is *identity-first*:
/// `focused_key` names the session under the cursor, and every board
/// rebuild (2s refresh or keypress) re-resolves it to `(col, card)`
/// indices — so a card that auto-moves columns keeps the cursor, and
/// `H`/`L` never act on whatever slid into the old position. Only when
/// the keyed session is gone do the positional indices act as the
/// clamped fallback (same semantics as the tree's `selected_index`).
#[derive(Debug, Clone, PartialEq)]
pub struct KanbanView {
    pub focused_col: usize,
    pub focused_card: usize,
    pub focused_key: Option<(Machine, String)>,
    /// `Some` while the folder-filter picker overlay is open (`f`).
    /// Kept inside the view (not a separate `AppState` variant) so the
    /// board renders underneath and its focus survives untouched.
    pub picker: Option<FilterPicker>,
}

/// Cursor state of the folder-filter picker. Rows are derived fresh each
/// frame/keypress by `App::kanban_filter_rows` — row 0 is "(no folder)",
/// the rest are folder prefixes. The cursor is stored by *identity*
/// (`None` = the "(no folder)" row, `Some(prefix)` = that folder row),
/// not by index — a background refresh can add/remove folder rows, and a
/// positional cursor would let Space toggle a different bucket than the
/// one highlighted. The index is derived per frame/keypress; a vanished
/// prefix falls back to row 0.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterPicker {
    pub selected: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Tree,
    Kanban(KanbanView),
    CreatingSession(CreateForm),
    RenamingSession {
        original_name: String,
        machine: Machine,
    },
    DuplicatingSession {
        source_name: String,
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
    /// Optional alternate action keyed off a single char. Lets one confirm
    /// modal offer two destructive choices (e.g. folder delete vs dissolve)
    /// without spawning a second keybinding. `Esc`/`n` always cancel.
    pub alternate: Option<ConfirmAlternate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmAlternate {
    pub key: char,
    pub label: String,
    pub action: PendingAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingAction {
    KillSession {
        machine: Machine,
        name: String,
    },
    /// Kill every session inside a folder. The folder disappears once tmux
    /// no longer reports any session sharing the prefix.
    DeleteFolder {
        prefix: String,
        targets: Vec<(Machine, String)>,
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
    NewSession,
    Rename,
    Duplicate,
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
    /// Whether the right-side ambient preview pane is shown. Toggled with
    /// `p` in tree state; persisted to `~/.config/ade/state.toml`.
    pub preview_pane_enabled: bool,
    /// Cache + worker pool for the preview pane. Populated only when
    /// `preview_pane_enabled` is true.
    pub preview_pane: PreviewPane,
    /// `Some` while the user has Tab'd into a session. The PTY is
    /// alive and the right panel renders its vt100 grid instead of the
    /// ambient snapshot. Mutated only on Tab (enter), the exit chord
    /// (`Ctrl+\` then `q`), or detection that the child has died.
    pub embedded_term: Option<EmbeddedTerm>,
    /// Exit-chord state machine. Lives on App so it persists across
    /// keystrokes inside one embedded session and resets on exit.
    pub embedded_chord: ChordState,
    /// The right-pane rect (x, y, w, h) the renderer drew the embedded
    /// terminal into on the most recent frame. `Cell` interior
    /// mutability is used because `render` takes `&App`. Mouse events
    /// hit the App with frame-local coords; we use this rect to decide
    /// whether a mouse event lands inside the embedded pane and to
    /// translate frame coords to pane-local coords before forwarding.
    pub embedded_panel_rect: Cell<Option<(u16, u16, u16, u16)>>,
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

    // ---- Notification dispatch state (`src/notifications.rs`) ----
    /// Per-session Claude state from the most recent refresh, keyed by
    /// `(Machine, session_id)`. Compared against the current refresh in
    /// `apply_refresh_result` to detect transitions (see the table in
    /// `lets-add-a-new-toasty-narwhal.md`). `session_id` is tmux's stable
    /// `$N` — survives `rename-session` so the diff doesn't miss
    /// transitions across mid-turn renames.
    pub prior_claude_states: HashMap<(Machine, String), ClaudeState>,
    /// Per-session "last time we fired a notification" timestamp, used by
    /// suppression rule 7 (debounce: skip repeats within 5s).
    pub last_notified_at: HashMap<(Machine, String), Instant>,
    /// True after the first successful `apply_refresh_result`. Until then,
    /// `prior_claude_states` is empty and we'd notify for every initial
    /// state — suppression rule 2 skips that pass entirely.
    pub notifications_initialised: bool,
    /// `Some((machine, session_id))` while the user has Tab'd into a
    /// session. Mirror of `embedded_term`'s target — kept separate
    /// because `embedded_term` doesn't carry tmux identity. Used by
    /// suppression rule 4: don't fire for the session the user is
    /// actively viewing in the embedded panel. Cleared everywhere
    /// `exit_embedded` is invoked (which itself runs on the chord exit,
    /// the write-failure path, and `tick`'s dead-child detection).
    pub embedded_target: Option<(Machine, String)>,
    /// Loaded from `state.notifications.first_seen`. When false AND
    /// `state.notifications.enabled` is false, the first-run footer
    /// nudge ("Desktop notifications available — press N…") is shown.
    /// Flipped true when the user presses `N` (enable) or `x` (dismiss).
    pub notifications_first_seen: bool,
    /// Sessions whose *most recent* refresh saw a `Provenance::Demoted`
    /// reading (TTL synthesised an Idle from a stale active file).
    /// Suppression rule 5 reads this on the *next* tick to skip the
    /// false-positive `Some(Working) → None` banner that the demote
    /// would otherwise produce.
    pub prior_demoted: HashSet<(Machine, String)>,

    // ---- Kanban board (`src/kanban.rs`) ----
    /// Column layout from `~/.config/ade/kanban.toml` (defaults when the
    /// file is missing or invalid). Read-only for the app's lifetime.
    pub kanban_config: KanbanConfig,
    /// Authoritative in-memory manual placements, `(machine, raw_name) →
    /// column id`. Loaded once at startup, mutated by card moves /
    /// reconciliation / rename migration, and mirrored to `state.toml`
    /// via `persist_kanban_placements`. Never rebuilt from disk mid-run —
    /// render and key handling must not do file I/O.
    pub kanban_placements: HashMap<(Machine, String), String>,
    /// Folder-level board filter (see `state::KanbanFilterState`).
    /// Authoritative in-memory copy, same lifecycle as the placements.
    pub kanban_filter: crate::state::KanbanFilterState,

    // ---- Inter-session mail (`src/mail.rs`) ----
    /// Pending messages awaiting delivery, keyed by recipient
    /// `(machine, session_name)`. Rebuilt every refresh by `process_mail`
    /// from the on-disk inbox; render reads it for the `✉ N` chip, and
    /// `deliver_selected_mail` consumes it on the user's key. P1 recipients
    /// are always local.
    pub mail_pending: HashMap<(Machine, String), Vec<mail::Message>>,
    /// When the oldest queued message for each recipient was first seen (the
    /// file's mtime, not the sender's clock). Drives the "held for 12m" signal
    /// so a stalled workstream is visible on the board instead of only in a
    /// router log. Free to compute — `process_mail` already stats each file.
    pub mail_oldest: HashMap<(Machine, String), std::time::SystemTime>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let (config, parse_warning) = Config::load();
        let (kanban_config, kanban_warning) = KanbanConfig::load();
        // Both config files can be broken at once; join the warnings so
        // neither is silently swallowed by the single error banner.
        let startup_warning = match (parse_warning, kanban_warning) {
            (Some(a), Some(b)) => Some(format!("{}; {}", a, b)),
            (a, b) => a.or(b),
        };
        let persisted = State::load();
        let kanban_placements = kanban::entries_to_map(&persisted.kanban.placements);
        let mut kanban_filter = persisted.kanban.filter.clone();
        kanban_filter.canonicalize();
        let mut expanded_memory: HashMap<String, bool> = HashMap::new();
        for prefix in &persisted.folders.closed {
            expanded_memory.insert(prefix.clone(), false);
        }
        let mut app = Self {
            state: AppState::Tree,
            focus_area: FocusArea::SessionList,
            tree: Tree::default(),
            selected_index: 0,
            selected_action: SessionAction::default(),
            input_buffer: TextField::new(),
            should_quit: false,
            action: AppAction::None,
            error_message: startup_warning,
            expanded_memory,
            config,
            host_hooks: HashMap::new(),
            local_hooks_installed: false,
            local_tmux_config_status: InstallStatus::Missing,
            tmux_nudge_dismissed: persisted.tmux_install_nudge.dismissed,
            preview_pane_enabled: persisted.preview_pane.enabled,
            preview_pane: PreviewPane::new(),
            embedded_term: None,
            embedded_chord: ChordState::Idle,
            embedded_panel_rect: Cell::new(None),
            hosts_notice: None,
            pending_refresh: None,
            last_refresh_started: Instant::now(),
            prior_claude_states: HashMap::new(),
            last_notified_at: HashMap::new(),
            notifications_initialised: false,
            embedded_target: None,
            notifications_first_seen: persisted.notifications.first_seen,
            prior_demoted: HashSet::new(),
            kanban_config,
            kanban_placements,
            kanban_filter,
            mail_pending: HashMap::new(),
            mail_oldest: HashMap::new(),
        };
        app.refresh();
        app
    }

    /// True when the opt-in nudge for desktop notifications should appear
    /// in the main-tree footer. Mirrors `should_show_tmux_nudge`'s gating
    /// shape: only in tree state, only when notifications are off, only
    /// when the user hasn't already been shown this nudge or the hint
    /// to re-run install-hooks. Press `N` to enable (which also sets
    /// `first_seen`), `x` to dismiss.
    pub fn should_show_notifications_nudge(&self) -> bool {
        matches!(self.state, AppState::Tree)
            && !self.notifications_first_seen
            && !State::load().notifications.enabled
    }

    /// True when notifications are enabled but ADE detects the Claude
    /// hooks aren't installed (or are stale v1 / missing the new
    /// `permission_prompt` matcher) on local OR any configured remote.
    /// Tells the user to run `ade install-hooks` so they actually get
    /// banners — without this nudge they'd silently miss them after
    /// updating ADE without re-installing hooks.
    pub fn should_show_hooks_stale_nudge(&self) -> bool {
        if !matches!(self.state, AppState::Tree) {
            return false;
        }
        if !State::load().notifications.enabled {
            return false;
        }
        if !self.local_hooks_installed {
            return true;
        }
        // Any remote host whose marker check returned false (not unknown).
        self.host_hooks
            .values()
            .any(|status| matches!(status, Some(false)))
    }

    /// Save the set of collapsed folder prefixes to
    /// `~/.config/ade/state.toml`. Best-effort — a transient I/O error must
    /// not block the UI.
    ///
    /// Uses **merge** semantics rather than overwrite-from-snapshot: start
    /// from the on-disk `closed` set, then apply only what we can directly
    /// observe in the current tree. Folders we can't see (e.g. a remote
    /// host is unreachable, so `refresh::refresh_all` omitted its sessions
    /// from `per_machine` — see `src/refresh.rs:50-66`) keep their
    /// previously-recorded preference. Dead keys accumulate slowly on
    /// permanent dissolve, but that's a few bytes and self-corrects the
    /// next time the prefix is used.
    ///
    /// Cross-process limitation: two concurrent ADE instances can race on
    /// the read-modify-write here (and on the fixed `state.toml.tmp`
    /// path). For small UI preferences with single-instance-typical usage,
    /// this is acceptable; not worth the complexity of file locking.
    fn persist_folder_expansion(&self) {
        let mut state = State::load();
        state.folders.closed = compute_closed_list(
            &state.folders.closed,
            self.tree.expanded_snapshot(),
        );
        let _ = state.save();
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
    /// After the tree is rebuilt, runs the per-session transition diff that
    /// dispatches macOS desktop notifications (see `dispatch_notifications`).
    fn apply_refresh_result(&mut self, result: RefreshResult) {
        for (k, v) in self.tree.expanded_snapshot() {
            self.expanded_memory.insert(k, v);
        }
        self.host_hooks = result.remote_hooks;
        self.local_hooks_installed = result.local_hooks_installed;
        self.local_tmux_config_status = result.local_tmux_config_status;
        let current_session = result.current_session;
        let observed = result.observed;
        // Capture the cursor's workstream identity BEFORE rebuilding, so the
        // recency reorder doesn't leave the highlight on whatever row slid
        // into the old position (see `RowKey`). Only real workstreams
        // (folders/sessions) are tracked by identity; the trailing
        // `NewSession` row is positional — tracking it would glue the cursor
        // to "New session" on the first populate (the empty tree's only row
        // is `NewSession`), so it keeps the clamp behavior instead.
        let prev_key = match self.tree.key_at(self.selected_index) {
            Some(k @ (RowKey::Folder(_) | RowKey::Session(..))) => Some(k),
            _ => None,
        };
        self.tree = Tree::build(result.per_machine, result.errors, &self.expanded_memory);
        self.tree.current_session = current_session;

        // Re-resolve the cursor to the same workstream. If it vanished
        // (killed, or its folder collapsed) — or was the positional
        // `NewSession` row — clamp the old index into range.
        let n = self.tree.visible_rows().len().max(1);
        self.selected_index = prev_key
            .and_then(|k| self.tree.index_of_key(&k))
            .unwrap_or_else(|| self.selected_index.min(n - 1));

        // Kanban: apply external intents (the `ade kanban` CLI's inbox),
        // then reconcile; persist at most once, and acknowledge the
        // intent files only after that save succeeded — a failed save
        // leaves them in the inbox for retry next refresh. Any pending
        // intent FORCES a save attempt, even when it's a memory no-op:
        // after a failed save the retained intent is already reflected in
        // memory, and a changed-only gate would ack it without the disk
        // ever catching up (Codex implementation review).
        let intent_paths = self.apply_kanban_intents();
        let reconcile_changed = self.reconcile_kanban(&observed);
        let save_ok = if !intent_paths.is_empty() || reconcile_changed {
            self.persist_kanban_placements().is_ok()
        } else {
            true
        };
        if save_ok {
            kanban::remove_intents(&intent_paths);
        }

        // Mail: rebuild the pending-message view from the inbox (and expire
        // any long-unroutable messages). Delivery itself is *not* here — it's
        // an explicit user action (`deliver_selected_mail`), so nothing is
        // injected into a pane automatically (Track A).
        self.process_mail();

        self.dispatch_notifications();
    }

    /// Time an undelivered message may sit in the inbox before it's moved to
    /// dead-letter. Generous on purpose: Track A delivery is human-triggered,
    /// so a message legitimately waits until someone acts. This is only a
    /// safety valve against a typo'd / never-appearing recipient wedging the
    /// inbox — not a delivery deadline.
    const MAIL_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Rebuild `mail_pending` from the on-disk inbox, keyed by recipient
    /// `(machine, session_name)`. Also dead-letters any message whose file
    /// has sat past `MAIL_TTL` (measured by the file's own mtime, never the
    /// sender-controlled `created_millis`). Never injects — that's the user's
    /// explicit action.
    /// A claim should complete within one keypress (synchronous send-keys).
    /// Anything sitting in `claimed/` past this is from a crashed router; it
    /// is moved to dead-letter, never silently re-injected.
    const MAIL_STALE_CLAIM: Duration = Duration::from_secs(120);

    fn process_mail(&mut self) {
        let Some(dir) = mail::mail_dir() else {
            self.mail_pending.clear();
            return;
        };
        // Recover claims stranded by a crashed router (to dead-letter, not
        // back to the inbox — a stranded claim's send is ambiguous). Surface
        // the anomaly so a lost/uncertain delivery isn't silent.
        let recovered = mail::recover_stale_claims(&dir, Self::MAIL_STALE_CLAIM);
        if recovered > 0 {
            self.error_message = Some(format!(
                "mail: recovered {} stranded message(s) to the dead-letter dir \
                 (a router likely crashed mid-delivery)",
                recovered
            ));
        }

        let mut pending: HashMap<(Machine, String), Vec<mail::Message>> = HashMap::new();
        let mut oldest: HashMap<(Machine, String), std::time::SystemTime> = HashMap::new();
        for (path, msg) in mail::read_inbox() {
            // Safety valve: expire messages that have waited too long. Uses
            // the file's mtime (first-seen), not the sender's timestamp. The
            // id passed comes from the trusted filename stem (read_inbox has
            // already verified it matches the payload and is path-safe).
            let expired = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > Self::MAIL_TTL)
                .unwrap_or(false);
            if expired {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&msg.id);
                // Only hide it from the pending view once it's actually been
                // moved out of the inbox; if the move fails, keep showing it
                // (better visible-and-stale than silently swallowed).
                if mail::dead_letter(&dir, &path, stem).is_ok() {
                    continue;
                }
            }
            // P1: all recipients are local.
            let key = (Machine::Local, msg.to_session.clone());
            // First-seen wins: the inbox is in publish order, so the first
            // message for a recipient is the one that has waited longest.
            if let Ok(seen) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                oldest.entry(key.clone()).or_insert(seen);
            }
            pending.entry(key).or_default().push(msg);
        }
        self.mail_pending = pending;
        self.mail_oldest = oldest;
    }

    /// How long the oldest queued message for a session has been waiting.
    pub fn pending_mail_wait(
        &self,
        machine: &Machine,
        session_name: &str,
    ) -> Option<std::time::Duration> {
        self.mail_oldest
            .get(&(machine.clone(), session_name.to_string()))
            .and_then(|t| t.elapsed().ok())
    }

    /// The held signal for a session row: `Some((wait, reason))` once mail has
    /// been queued longer than `HELD_AFTER`. `None` while it is merely pending,
    /// so the ordinary case stays quiet.
    pub fn pending_mail_held(
        &self,
        idx: usize,
    ) -> Option<(std::time::Duration, mail_delivery::HeldReason)> {
        let s = self.tree.session(idx)?;
        let wait = self.pending_mail_wait(&s.machine, &s.raw_name)?;
        if wait < mail_delivery::HELD_AFTER {
            return None;
        }
        Some((
            wait,
            mail_delivery::HeldReason::from_session_state(s.claude_present, s.claude),
        ))
    }

    /// Number of pending messages queued for a session — drives the `✉ N`
    /// chip. Cheap in-memory lookup; render must never touch disk.
    pub fn pending_mail_count(&self, machine: &Machine, session_name: &str) -> usize {
        self.mail_pending
            .get(&(machine.clone(), session_name.to_string()))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Drain the kanban intent inbox into the in-memory placements.
    /// Intents arrive sorted by publish order, so a later intent for the
    /// same session wins. The caller persists and acknowledges the
    /// returned (= applied) paths.
    ///
    /// Intents for a session whose Claude is Working right now are
    /// **deferred**, not applied: left in the inbox, unacknowledged,
    /// until the work stops. Applying immediately would be pointless —
    /// reconcile's Working-pin clears the placement in the same pass —
    /// and the headline flow is exactly "Claude marks its own session
    /// done at the end of a turn", which reaches this code while the
    /// turn's `working` status is still on disk. Deferring turns that
    /// into "lands in Done the moment the work stops".
    fn apply_kanban_intents(&mut self) -> Vec<std::path::PathBuf> {
        let intents = kanban::read_intents();
        let mut paths = Vec::with_capacity(intents.len());
        for (path, intent) in intents {
            let machine = if intent.host == "local" {
                Machine::Local
            } else {
                Machine::Remote(intent.host.clone())
            };
            let working_now = self.tree.sessions.iter().any(|s| {
                s.machine == machine
                    && s.raw_name == intent.session
                    && s.claude == Some(ClaudeState::Working)
            });
            if working_now {
                continue; // defer: file stays in the inbox for a later tick
            }
            let key = (machine, intent.session.clone());
            match intent.column {
                Some(column) => {
                    self.kanban_placements.insert(key, column);
                }
                None => {
                    self.kanban_placements.remove(&key);
                }
            }
            paths.push(path);
        }
        paths
    }

    /// Kanban bookkeeping after every refresh (sync or background):
    ///
    /// (a) **Working precedence** — a session whose Claude is Working now
    ///     belongs to the auto-active column no matter where the user put
    ///     it; drop its manual placement so that when Claude stops it
    ///     falls to auto-awaiting rather than snapping back to Done.
    /// (b) **Prune dead sessions** — but only on machines whose session
    ///     list was *successfully observed* this refresh
    ///     (`RefreshResult::observed`). A transient tmux/SSH failure looks
    ///     like an empty list and must not wipe placements. Placements
    ///     are never pruned for referencing an unknown column id — a
    ///     temporarily broken kanban.toml must not destroy them.
    /// (c) Re-resolve the board cursor by session identity.
    ///
    /// Returns whether the placements changed; the caller
    /// (`apply_refresh_result`) folds this into a single save per refresh
    /// — the 2s steady state writes nothing.
    fn reconcile_kanban(&mut self, observed: &[Machine]) -> bool {
        let mut dirty = false;

        // (a) Working precedence.
        for s in &self.tree.sessions {
            if s.claude == Some(ClaudeState::Working) {
                dirty |= self
                    .kanban_placements
                    .remove(&(s.machine.clone(), s.raw_name.clone()))
                    .is_some();
            }
        }

        // (b) Prune placements for vanished sessions on observed machines.
        let observed_set: HashSet<&Machine> = observed.iter().collect();
        let live: HashSet<(&Machine, &str)> = self
            .tree
            .sessions
            .iter()
            .map(|s| (&s.machine, s.raw_name.as_str()))
            .collect();
        let before = self.kanban_placements.len();
        self.kanban_placements.retain(|(machine, name), _| {
            !observed_set.contains(machine) || live.contains(&(machine, name.as_str()))
        });
        dirty |= self.kanban_placements.len() != before;

        // (c) Follow the focused card by identity.
        if matches!(self.state, AppState::Kanban(_)) {
            let board = kanban::build_board(
                &self.kanban_config,
                &self.tree.sessions,
                &self.kanban_placements,
                &self.kanban_filter,
            );
            let (col, card) = self.resolve_kanban_focus(&board);
            if let AppState::Kanban(ref mut view) = self.state {
                view.focused_col = col;
                view.focused_card = card;
            }
        }

        dirty
    }

    /// Resolve the kanban cursor to concrete `(col, card)` indices for the
    /// current board: by `focused_key` identity when that session still
    /// exists, positionally clamped otherwise. Returns `(0, 0)` when not
    /// in the kanban state.
    pub fn resolve_kanban_focus(&self, board: &[Vec<usize>]) -> (usize, usize) {
        let AppState::Kanban(ref view) = self.state else {
            return (0, 0);
        };
        if let Some((machine, name)) = &view.focused_key {
            for (ci, col) in board.iter().enumerate() {
                let pos = col.iter().position(|&si| {
                    self.tree
                        .session(si)
                        .map(|s| &s.machine == machine && &s.raw_name == name)
                        .unwrap_or(false)
                });
                if let Some(pos) = pos {
                    return (ci, pos);
                }
            }
        }
        let ncols = board.len().max(1);
        let col = view.focused_col.min(ncols - 1);
        let len = board.get(col).map(|c| c.len()).unwrap_or(0);
        let card = if len == 0 {
            0
        } else {
            view.focused_card.min(len - 1)
        };
        (col, card)
    }

    /// Mirror the in-memory placements map to `state.toml`. Merge
    /// semantics like `persist_folder_expansion`: reload, replace only the
    /// kanban section, save. Same known cross-process race / fixed `.tmp`
    /// path as the rest of state persistence — acceptable single-instance
    /// best-effort. Caller decides whether a failure is worth a banner
    /// (user-initiated moves: yes; background reconcile: no).
    fn persist_kanban_placements(&self) -> Result<(), String> {
        let mut state = State::load();
        state.kanban.placements = kanban::map_to_entries(&self.kanban_placements);
        state.save()
    }

    /// Build a per-session map of `(Machine, session_id) → ClaudeState` from
    /// the freshly-rebuilt tree, diff it against `self.prior_claude_states`,
    /// and fire `notifications::fire` for each transition that survives the
    /// suppression rules. Updates `prior_claude_states`,
    /// `last_notified_at`, and `notifications_initialised` at the end.
    ///
    /// **Suppression rules** (in evaluation order — match the plan doc):
    /// 1. Global `state.notifications.enabled` flag (cheapest gate).
    /// 2. First refresh after launch: `notifications_initialised` is false.
    /// 3. Currently-attached local session (`tree.current_session`).
    /// 4. Embedded-Tab target (`embedded_target`).
    /// 5. TTL/orphan-driven demotion (the *prior* tick's session was
    ///    `claude_demoted = true`, meaning the active state we observed
    ///    last tick came from a TTL synthesis — suppressing this tick's
    ///    `→ None` transition that's just the demote materialising).
    /// 6. SessionStart restart false-positive: accepted in v1, no
    ///    additional suppression here.
    /// 7. Per-session debounce within `NOTIFICATION_DEBOUNCE`.
    fn dispatch_notifications(&mut self) {
        // Defense in depth against the embedded dead-child race: even
        // though `tick()` runs `is_alive()` before us, the child can die
        // between that check and now. Re-check here so the embedded_target
        // suppression in rule 4 doesn't read a stale value.
        if let Some(et) = self.embedded_term.as_mut() {
            if !et.is_alive() {
                self.exit_embedded();
            }
        }

        let enabled = State::load().notifications.enabled;

        // Build the new state map from the rebuilt tree. Carries
        // `claude_demoted` alongside the state so we can stash it as part
        // of the prior-state record for next-tick rule 5 evaluation.
        let mut new_states: HashMap<(Machine, String), (Option<ClaudeState>, bool)> =
            HashMap::new();
        for s in &self.tree.sessions {
            new_states.insert(
                (s.machine.clone(), s.session_id.clone()),
                (s.claude, s.claude_demoted),
            );
        }

        // Machines that errored this refresh — used by the
        // vanished-session second pass to avoid firing "Claude finished"
        // for every active session on a host that just went unreachable
        // (the sessions are still alive over there; we just couldn't
        // reach them this tick).
        let errored_machines: HashSet<Machine> = self
            .tree
            .errors
            .iter()
            .map(|(m, _)| m.clone())
            .collect();

        if enabled && self.notifications_initialised {
            for (key, (new_state, demoted)) in &new_states {
                let prior = self.prior_claude_states.get(key).copied();
                if !is_fire_transition(prior, *new_state) {
                    continue;
                }
                // Rule 5 (current-tick demote): if THIS tick saw a
                // Provenance::Demoted reading for the session, the
                // active → None transition is just the TTL/orphan
                // synthesis materialising — not a real "Claude finished".
                // Must check current-tick `demoted`, not prior_demoted —
                // the demote happens this tick, and using prior would
                // miss the immediate suppression and only catch the
                // *next* tick's `None → None` non-transition (which never
                // fires anyway).
                if *demoted && new_state.is_none() {
                    continue;
                }
                if self.suppress_transition(key) {
                    continue;
                }
                let (machine, _sid) = key;
                let session = self
                    .tree
                    .sessions
                    .iter()
                    .find(|s| &s.machine == machine && &s.session_id == &key.1);
                let Some(s) = session else { continue };
                notifications::fire(
                    machine.title_label(),
                    s.prefix.as_deref(),
                    &s.leaf,
                );
                self.last_notified_at.insert(key.clone(), Instant::now());
            }

            // Sessions that disappeared entirely between ticks: same
            // logic — if the prior state was active, treat as `→ None`.
            // The `for new_states` loop above only iterates sessions
            // that exist *now*, so killed sessions wouldn't be caught
            // without this second pass.
            let prior_keys: Vec<(Machine, String)> =
                self.prior_claude_states.keys().cloned().collect();
            for key in prior_keys {
                if new_states.contains_key(&key) {
                    continue;
                }
                // Skip "vanished" entries when their host errored this
                // refresh — the sessions probably still exist; we just
                // couldn't see them. Without this guard, a brief mosh
                // network blip would fire `host/$id` notifications for
                // every active session on that host.
                if errored_machines.contains(&key.0) {
                    continue;
                }
                let prior = self.prior_claude_states.get(&key).copied();
                if !is_fire_transition(prior, None) {
                    continue;
                }
                if self.suppress_transition(&key) {
                    continue;
                }
                // Without a current Session row we have no folder/leaf
                // for the body — fall back to the raw session_id. This
                // is the rare case where a session vanished between
                // ticks; copy reads "local/$3" but the user still gets
                // the signal that something they were watching ended.
                let (machine, sid) = &key;
                notifications::fire(machine.title_label(), None, sid);
                self.last_notified_at.insert(key, Instant::now());
            }
        }

        // Rebuild prior_claude_states only with sessions we actually saw.
        // For errored machines, KEEP the previous prior entries — they
        // didn't actually disappear; we just couldn't observe them. This
        // way, when the host comes back, the comparison is against the
        // last-known state, not against an empty baseline.
        let mut next_prior: HashMap<(Machine, String), ClaudeState> =
            new_states
                .iter()
                .filter_map(|(k, (state, _))| state.map(|s| (k.clone(), s)))
                .collect();
        for (key, state) in &self.prior_claude_states {
            if errored_machines.contains(&key.0) && !next_prior.contains_key(key) {
                next_prior.insert(key.clone(), *state);
            }
        }
        self.prior_claude_states = next_prior;

        // `prior_demoted` is no longer load-bearing for suppression
        // (current-tick demoted flag in `new_states[key].1` covers it),
        // but we keep it cached so future tick logic — e.g. tracking
        // sessions that recovered from demotion — has the data on hand.
        self.prior_demoted = new_states
            .iter()
            .filter_map(|(k, (_, dem))| if *dem { Some(k.clone()) } else { None })
            .collect();
        self.notifications_initialised = true;
    }

    /// Per-key suppression evaluator — applies rules 3 (current session),
    /// 4 (embedded target), 5 (TTL demote), 7 (debounce). Rules 1
    /// (enabled) and 2 (initialised) are checked at the call site
    /// because they short-circuit the whole loop, not per-key.
    fn suppress_transition(&self, key: &(Machine, String)) -> bool {
        // Rule 3: currently-attached local session.
        if matches!(key.0, Machine::Local) {
            if let Some(current_name) = &self.tree.current_session {
                // tmux::current_session returns by name; resolve to a
                // session_id via the tree.
                let cur_id = self
                    .tree
                    .sessions
                    .iter()
                    .find(|s| &s.raw_name == current_name && matches!(s.machine, Machine::Local))
                    .map(|s| s.session_id.clone());
                if cur_id.as_ref() == Some(&key.1) {
                    return true;
                }
            }
        }
        // Rule 4: embedded-Tab target.
        if self.embedded_target.as_ref() == Some(key) {
            return true;
        }
        // Rule 5: prior tick's reading was demoted (TTL/orphan).
        if self.prior_demoted.contains(key) {
            return true;
        }
        // Rule 7: per-session debounce.
        if let Some(last) = self.last_notified_at.get(key) {
            if last.elapsed() < NOTIFICATION_DEBOUNCE {
                return true;
            }
        }
        false
    }

    /// Called once per event-loop iteration. Applies a finished background
    /// refresh and schedules a new one when the interval has elapsed.
    /// Non-blocking: if the worker is still running, just leaves it alone.
    ///
    /// Dead-child detection runs **before** `apply_refresh_result` so the
    /// notification dispatch (rule 4: suppress for the embedded target)
    /// reads a fresh `embedded_target = None` on the same tick the child
    /// died, rather than seeing a stale embedded_target and incorrectly
    /// suppressing a real "Claude finished" banner for the just-detached
    /// session.
    pub fn tick(&mut self) {
        // Detect a dead embedded child (target session killed externally,
        // mosh/ssh dropped, etc.) and exit cleanly back to the current
        // view (tree, or the kanban board when embedded from a card) —
        // BEFORE we apply the refresh, so the diff in
        // `apply_refresh_result` sees the cleared `embedded_target`.
        let embedded_dead = self
            .embedded_term
            .as_mut()
            .map(|et| !et.is_alive())
            .unwrap_or(false);
        if embedded_dead {
            self.exit_embedded();
        }

        if let Some(handle) = self.pending_refresh.take() {
            if handle.is_finished() {
                if let Ok(result) = handle.join() {
                    self.apply_refresh_result(result);
                }
            } else {
                self.pending_refresh = Some(handle);
            }
        }

        // Preview pane lives on its own short cadence (~500ms) keyed by
        // the highlighted session, separate from the 2s session-list
        // refresh. Skip work entirely when the pane is off — and also
        // when we're embedded, since the right panel renders the live
        // PTY grid in that case (no need to keep snapshotting).
        // Ambient snapshots feed only the tree's side pane. The kanban
        // board has no read-only preview (its modal IS the embedded
        // terminal, which renders the live PTY grid) — skip the capture
        // work there.
        if self.preview_pane_enabled
            && self.embedded_term.is_none()
            && !matches!(self.state, AppState::Kanban(_))
        {
            let target = self.preview_target();
            self.preview_pane.tick(target.as_ref(), &self.config.hosts);
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

    /// Title to write to the outer terminal tab. Session rows produce
    /// `folder/session | host` (or `session | host` for loose sessions);
    /// every other state collapses to a static `"ade"`.
    pub fn tab_title(&self) -> String {
        if self.focus_area == FocusArea::TitleBar
            || !matches!(self.state, AppState::Tree)
        {
            return "ade".to_string();
        }
        match self.current_row() {
            Some(Row::Session(idx)) => match self.tree.session(idx) {
                Some(s) => crate::term_title::format_session(
                    s.prefix.as_deref(),
                    &s.leaf,
                    s.machine.title_label(),
                ),
                None => "ade".to_string(),
            },
            _ => "ade".to_string(),
        }
    }

    /// True when the in-TUI "tmux config not installed / out of date" nudge
    /// should be rendered. Only shown when ADE is running inside tmux
    /// (otherwise the fix isn't relevant), the marker is missing or stale
    /// locally, the user hasn't already dismissed it, and we're in the main
    /// tree state — so the `x` dismissal binding is always reachable when
    /// the nudge is visible rather than being shadowed by a modal's own
    /// keymap. Stale matches when the managed body version bumped (e.g.
    /// v3 → v4) and the user hasn't re-run `ade install-tmux-config` yet.
    pub fn should_show_tmux_nudge(&self) -> bool {
        !self.tmux_nudge_dismissed
            && matches!(self.state, AppState::Tree)
            && tmux::is_inside_tmux()
            && matches!(
                self.local_tmux_config_status,
                InstallStatus::Missing | InstallStatus::Stale
            )
    }

    /// Persist the dismissal so we don't re-pester after restart. State
    /// persistence is best-effort — failing to save doesn't block the UI.
    fn dismiss_tmux_nudge(&mut self) {
        self.tmux_nudge_dismissed = true;
        let mut state = State::load();
        state.tmux_install_nudge.dismissed = true;
        let _ = state.save();
    }

    /// Toggle the right-side ambient preview pane and persist the new
    /// value. Default is off; on each toggle we save immediately so the
    /// preference survives quitting.
    fn toggle_preview_pane(&mut self) {
        self.preview_pane_enabled = !self.preview_pane_enabled;
        let mut state = State::load();
        state.preview_pane.enabled = self.preview_pane_enabled;
        let _ = state.save();
    }

    /// Toggle desktop notifications and persist. First toggle also flips
    /// `first_seen` so the opt-in nudge stops appearing. The new value is
    /// announced via a transient `error_message` (existing pattern in
    /// other toggles) so the user gets immediate visual confirmation.
    fn toggle_notifications(&mut self) {
        let mut state = State::load();
        state.notifications.enabled = !state.notifications.enabled;
        state.notifications.first_seen = true;
        let new_state = state.notifications.enabled;
        let _ = state.save();
        self.notifications_first_seen = true;
        self.error_message = Some(if new_state {
            "Notifications enabled".to_string()
        } else {
            "Notifications disabled".to_string()
        });
    }

    /// Persist the user's dismissal of the desktop-notifications nudge
    /// without enabling notifications. Same pattern as
    /// `dismiss_tmux_nudge`.
    fn dismiss_notifications_nudge(&mut self) {
        self.notifications_first_seen = true;
        let mut state = State::load();
        state.notifications.first_seen = true;
        let _ = state.save();
    }

    /// The session currently under the cursor, expressed as a `PreviewKey`,
    /// or `None` for non-Session rows / no selection. Used by the preview
    /// pane scheduler in `tick` (tree only) and, on the kanban board, by
    /// the embed-modal title — there the "cursor" is the focused card.
    pub fn preview_target(&self) -> Option<PreviewKey> {
        if matches!(self.state, AppState::Kanban(_)) {
            let board = kanban::build_board(
                &self.kanban_config,
                &self.tree.sessions,
                &self.kanban_placements,
                &self.kanban_filter,
            );
            let (col, card) = self.resolve_kanban_focus(&board);
            let &si = board.get(col)?.get(card)?;
            let session = self.tree.session(si)?;
            return Some(PreviewKey {
                machine: session.machine.clone(),
                name: session.raw_name.clone(),
            });
        }
        let Row::Session(idx) = self.current_row()? else {
            return None;
        };
        let session = self.tree.session(idx)?;
        Some(PreviewKey {
            machine: session.machine.clone(),
            name: session.raw_name.clone(),
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.error_message = None;
        // While in embedded mode, the entire keymap is shadowed: keys
        // either flow to the embedded child via the chord state machine
        // or trigger the exit chord. Tree/host/modal keymaps don't run.
        if self.embedded_term.is_some() {
            return self.handle_embedded_key(key);
        }
        match &self.state {
            AppState::Tree => self.handle_tree_key(key),
            AppState::Kanban(_) => self.handle_kanban_key(key),
            AppState::CreatingSession(_) => self.handle_creating_session_key(key),
            AppState::RenamingSession { .. } => self.handle_renaming_session_key(key),
            AppState::DuplicatingSession { .. } => self.handle_duplicating_session_key(key),
            AppState::RenamingFolder { .. } => self.handle_renaming_folder_key(key),
            AppState::Confirming(_) => self.handle_confirming_key(key),
            AppState::HostsList { .. } => self.handle_hosts_list_key(key),
            AppState::HostForm(_) => self.handle_host_form_key(key),
        }
    }

    /// Forward a mouse event to the embedded PTY *only* when the click
    /// landed inside the embedded panel rect that the renderer last
    /// drew. Outside the panel (i.e. on the tree side) the event is
    /// dropped — we don't currently handle mouse on the tree, and we
    /// definitely don't want stray clicks to reach the embedded
    /// session.
    pub fn handle_mouse(&mut self, event: crossterm::event::MouseEvent) {
        let Some(rect) = self.embedded_panel_rect.get() else {
            return;
        };
        let bytes = translate_mouse(event, rect);
        if bytes.is_empty() {
            return;
        }
        if let Some(et) = self.embedded_term.as_mut() {
            if let Err(e) = et.write(&bytes) {
                self.error_message = Some(format!("embedded mouse write: {}", e));
                self.exit_embedded();
            }
        }
    }

    /// Drive the chord state machine and forward keystrokes to the
    /// embedded PTY. Called only while `embedded_term` is `Some`.
    fn handle_embedded_key(&mut self, key: KeyEvent) {
        let outcome = chord_step(&mut self.embedded_chord, key);
        match outcome {
            ChordOutcome::Forward(bytes) => {
                if let Some(et) = self.embedded_term.as_mut() {
                    if let Err(e) = et.write(&bytes) {
                        // Write failure usually means the PTY closed
                        // underneath us. Bail out cleanly so the user
                        // gets back to the tree.
                        self.error_message = Some(format!("embedded write: {}", e));
                        self.exit_embedded();
                    }
                }
            }
            ChordOutcome::Exit => self.exit_embedded(),
        }
    }

    /// Try to enter embedded mode against the currently-highlighted
    /// session. No-op in any case where the focus or row isn't right
    /// (folder rows, NewSession placeholder, etc.). Errors during PTY/
    /// child spawn surface via `error_message`. If the preview pane
    /// isn't open yet, Tab opens it first — equivalent to `p` + `Tab`
    /// in one keystroke.
    fn try_enter_embedded(&mut self) {
        if self.focus_area != FocusArea::SessionList {
            return;
        }
        let Some(Row::Session(idx)) = self.current_row() else {
            return;
        };
        if !self.preview_pane_enabled {
            self.toggle_preview_pane();
        }
        self.enter_embedded_session(idx);
    }

    /// Same as `try_enter_embedded`, but against the kanban board's
    /// focused card. Doesn't touch the persisted preview preference —
    /// the board renders the embedded grid in its modal regardless.
    fn try_enter_embedded_kanban(&mut self) {
        let board = kanban::build_board(
            &self.kanban_config,
            &self.tree.sessions,
            &self.kanban_placements,
            &self.kanban_filter,
        );
        let (col, card) = self.resolve_kanban_focus(&board);
        let Some(&idx) = board.get(col).and_then(|c| c.get(card)) else {
            return; // empty column
        };
        self.enter_embedded_session(idx);
    }

    /// Spawn the embedded PTY for the session at `idx` in the tree.
    /// Shared by the tree's Tab (right-pane embed) and the board's Tab
    /// (modal embed) — the exit chord tears it down identically in both.
    fn enter_embedded_session(&mut self, idx: usize) {
        let Some(session) = self.tree.session(idx) else {
            return;
        };
        let name = session.raw_name.clone();
        let machine = session.machine.clone();
        let session_id = session.session_id.clone();

        // Placeholder size — UI's first frame after entering embedded
        // immediately calls `resize()` with the actual rect dimensions.
        let result = match machine.clone() {
            Machine::Local => EmbeddedTerm::spawn_local(&name, 24, 80),
            Machine::Remote(host_name) => match self
                .config
                .host_by_name(&host_name)
                .cloned()
            {
                Some(host) => EmbeddedTerm::spawn_remote(&host, &name, 24, 80),
                None => Err(format!("host '{}' not found in config", host_name)),
            },
        };

        match result {
            Ok(et) => {
                self.embedded_term = Some(et);
                self.embedded_chord = ChordState::Idle;
                // Notification suppression rule 4: while the user is
                // viewing this session in the embedded panel, don't fire
                // banners for it. Cleared in `exit_embedded`.
                self.embedded_target = Some((machine, session_id));
            }
            Err(e) => {
                self.error_message = Some(format!("embedded: {}", e));
            }
        }
    }

    /// Tear down the embedded session and return focus to the tree.
    /// Idempotent: calling when not embedded is a no-op. Called by:
    /// the exit chord, write-failure detection in `handle_embedded_key`,
    /// and child-death detection in `tick`. Clears `embedded_target` in
    /// every path so notification rule 4 doesn't permanently suppress
    /// the previously-attached session.
    fn exit_embedded(&mut self) {
        if let Some(mut et) = self.embedded_term.take() {
            et.kill();
            // EmbeddedTerm::Drop handles the wait + reader detach.
            drop(et);
        }
        self.embedded_chord = ChordState::Idle;
        self.embedded_target = None;
    }

    /// Returns the embedded session's PTY size if active, useful for
    /// the renderer to decide whether to call `resize()` on layout
    /// changes.
    pub fn embedded_active(&self) -> bool {
        self.embedded_term.is_some()
    }

    /// `true` while the exit-chord prefix has been pressed but the
    /// next key hasn't arrived. The renderer uses this to switch the
    /// embedded-pane border to mauve and the help-bar copy to a
    /// "chord armed" variant — Codex Phase-6 review pointed out that
    /// without a UI cue, the modal-ish state would feel surprising.
    pub fn embedded_chord_pending(&self) -> bool {
        matches!(self.embedded_chord, ChordState::Pending)
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
            KeyCode::Right | KeyCode::Char('l') => match self.current_row() {
                Some(Row::Session(_)) => {
                    // Sessions skip the NewSession slot — that chip is folder-only.
                    // NewSession arm snaps back into the session cycle in case state
                    // drifted in from a folder row across a refresh.
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::Rename,
                        SessionAction::Rename => SessionAction::Duplicate,
                        SessionAction::Duplicate => SessionAction::Delete,
                        SessionAction::Delete => SessionAction::Delete,
                        SessionAction::NewSession => SessionAction::Rename,
                    };
                }
                Some(Row::Folder(_)) => {
                    // Folder rows have no Duplicate slot — folders are a
                    // pure UI grouping, not a tmux primitive.
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::NewSession,
                        SessionAction::NewSession => SessionAction::Rename,
                        SessionAction::Rename => SessionAction::Delete,
                        SessionAction::Duplicate | SessionAction::Delete => SessionAction::Delete,
                    };
                }
                _ => {}
            },
            KeyCode::Left | KeyCode::Char('h') => match self.current_row() {
                Some(Row::Session(_)) => {
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::Enter,
                        SessionAction::Rename => SessionAction::Enter,
                        SessionAction::Duplicate => SessionAction::Rename,
                        SessionAction::Delete => SessionAction::Duplicate,
                        // Stale NewSession from a prior folder row: `h` is
                        // "move left", so snap to Enter rather than Rename to
                        // keep the directional intent of the keypress.
                        SessionAction::NewSession => SessionAction::Enter,
                    };
                }
                Some(Row::Folder(_)) => {
                    self.selected_action = match self.selected_action {
                        SessionAction::Enter => SessionAction::Enter,
                        SessionAction::NewSession => SessionAction::Enter,
                        SessionAction::Rename => SessionAction::NewSession,
                        SessionAction::Duplicate | SessionAction::Delete => SessionAction::Rename,
                    };
                }
                _ => {}
            },
            KeyCode::Char('o') | KeyCode::Char(' ') => {
                if let Some(Row::Folder(idx)) = self.current_row() {
                    self.tree.toggle_folder(idx);
                    self.persist_folder_expansion();
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
                // If we're on a folder row (or a session inside one), pre-fill
                // the modal's prefix from that context so the user only has
                // to type the leaf. Row::NewSession + None fall through to
                // the cwd-guessed default — no stale folder prefix leaks
                // into the placeholder path.
                let form = match self.current_row() {
                    Some(Row::Folder(idx)) => self
                        .tree
                        .folders
                        .get(idx)
                        .map(|f| CreateForm::with_prefix(&f.prefix))
                        .unwrap_or_else(CreateForm::new),
                    Some(Row::Session(idx)) => self
                        .tree
                        .session(idx)
                        .and_then(|s| s.prefix.clone())
                        .map(|p| CreateForm::with_prefix(&p))
                        .unwrap_or_else(CreateForm::new),
                    _ => CreateForm::new(),
                };
                self.state = AppState::CreatingSession(form);
                self.input_buffer.clear();
            }
            KeyCode::Char('R') => self.start_rename_selected(),
            KeyCode::Char('y') => self.start_duplicate_selected(),
            KeyCode::Char('d') => self.start_delete_selected(),
            KeyCode::Char('m') => self.deliver_selected_mail(),
            KeyCode::Char('H') => {
                self.state = AppState::HostsList { selected: 0 };
            }
            KeyCode::Char('K') => {
                self.state = AppState::Kanban(KanbanView {
                    focused_col: 0,
                    focused_card: 0,
                    focused_key: None,
                    picker: None,
                });
            }
            KeyCode::Char('x') => {
                // Both opt-in nudges share the `x` key. When both are
                // visible at once, dismiss the tmux one first (it's
                // rendered on top); the next `x` clears the notifications
                // nudge. Visible-but-already-dismissed cases short-circuit
                // via the `should_show_*` checks.
                if self.should_show_tmux_nudge() {
                    self.dismiss_tmux_nudge();
                } else if self.should_show_notifications_nudge() {
                    self.dismiss_notifications_nudge();
                }
            }
            KeyCode::Char('p') => {
                self.toggle_preview_pane();
            }
            KeyCode::Char('N') => {
                self.toggle_notifications();
            }
            KeyCode::Tab => {
                self.try_enter_embedded();
            }
            _ => {}
        }
    }

    /// Keymap for the kanban board. Every branch works on a
    /// freshly-built board with the cursor re-resolved by identity, so a
    /// background refresh between keypresses can't make a key act on the
    /// wrong card.
    fn handle_kanban_key(&mut self, key: KeyEvent) {
        // Filter-picker overlay shadows the board keymap while open.
        if matches!(&self.state, AppState::Kanban(v) if v.picker.is_some()) {
            return self.handle_kanban_filter_key(key);
        }
        let board = kanban::build_board(
            &self.kanban_config,
            &self.tree.sessions,
            &self.kanban_placements,
            &self.kanban_filter,
        );
        let (col, card) = self.resolve_kanban_focus(&board);
        let ncols = board.len().max(1);

        // Re-focus a session identity after the cursor moved to
        // (new_col, new_card); empty column → keep position, clear key.
        let focus_to = |view: &mut KanbanView,
                        tree: &Tree,
                        new_col: usize,
                        new_card: usize| {
            view.focused_col = new_col;
            view.focused_card = new_card;
            view.focused_key = board
                .get(new_col)
                .and_then(|c| c.get(new_card))
                .and_then(|&si| tree.session(si))
                .map(|s| (s.machine.clone(), s.raw_name.clone()));
        };

        match key.code {
            // `q` means "back to tree" here (vs quit in tree) — the help
            // bar calls this out.
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('K') => {
                self.state = AppState::Tree;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            // Card moves before column nav: Shift+arrows share the
            // Left/Right key codes.
            KeyCode::Char('H') => self.move_focused_card(-1),
            KeyCode::Char('L') => self.move_focused_card(1),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_focused_card(-1)
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_focused_card(1)
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let new_col = col.saturating_sub(1);
                let len = board.get(new_col).map(|c| c.len()).unwrap_or(0);
                let new_card = if len == 0 { 0 } else { card.min(len - 1) };
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, new_col, new_card);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let new_col = (col + 1).min(ncols - 1);
                let len = board.get(new_col).map(|c| c.len()).unwrap_or(0);
                let new_card = if len == 0 { 0 } else { card.min(len - 1) };
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, new_col, new_card);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let new_card = card.saturating_sub(1);
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, col, new_card);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = board.get(col).map(|c| c.len()).unwrap_or(0);
                let new_card = if len == 0 { 0 } else { (card + 1).min(len - 1) };
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, col, new_card);
                }
            }
            KeyCode::Char('g') => {
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, col, 0);
                }
            }
            KeyCode::Char('G') => {
                let len = board.get(col).map(|c| c.len()).unwrap_or(0);
                let tree = &self.tree;
                if let AppState::Kanban(ref mut view) = self.state {
                    focus_to(view, tree, col, len.saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                // Attach. After detach the loop resumes with `state`
                // still Kanban, so the user lands back on the board.
                let session = board
                    .get(col)
                    .and_then(|c| c.get(card))
                    .and_then(|&si| self.tree.session(si))
                    .cloned();
                if let Some(s) = session {
                    self.action = AppAction::AttachSession {
                        name: s.raw_name,
                        machine: s.machine,
                    };
                }
            }
            KeyCode::Char('r') => self.refresh(),
            // Both open the focused card's session *interactively* right
            // away (a modal hosting the live embedded PTY — type into it
            // immediately; the exit chord returns to the board). There is
            // deliberately no read-only preview stop-over on the board,
            // and `p` does NOT touch the tree's persisted preview-pane
            // preference.
            KeyCode::Char('p') | KeyCode::Tab => self.try_enter_embedded_kanban(),
            KeyCode::Char('f') => {
                if let AppState::Kanban(ref mut view) = self.state {
                    view.picker = Some(FilterPicker { selected: None });
                }
            }
            // Clear the folder filter without opening the picker (the
            // summary bar advertises this). No-op when inactive.
            KeyCode::Char('c') => {
                if self.kanban_filter.is_active() {
                    self.kanban_filter = crate::state::KanbanFilterState::default();
                    if let Err(e) = self.persist_kanban_filter() {
                        self.error_message = Some(format!("failed to save filter: {}", e));
                    }
                    self.reanchor_kanban_focus();
                }
            }
            _ => {}
        }
    }

    /// Rows of the folder-filter picker: index 0 is the "(no folder)"
    /// bucket (`None`), the rest are folder prefixes — the union of what
    /// the tree currently shows and what the saved filter references, so
    /// a selected-but-currently-absent folder stays visible and can be
    /// unticked. Sorted, stable across refreshes.
    pub fn kanban_filter_rows(&self) -> Vec<Option<String>> {
        let mut prefixes: std::collections::BTreeSet<String> = self
            .tree
            .sessions
            .iter()
            .filter_map(|s| s.prefix.clone())
            .collect();
        prefixes.extend(self.kanban_filter.folders.iter().cloned());
        let mut rows = vec![None];
        rows.extend(prefixes.into_iter().map(Some));
        rows
    }

    /// Keymap of the filter-picker overlay. Toggles apply and persist
    /// immediately (same pattern as the other prefs); `c` clears the
    /// whole filter back to show-all; Esc/`f`/`q` close the picker.
    /// Resolve the picker's identity cursor to a row index for the
    /// current row set — falling back to row 0 ("(no folder)", which
    /// always exists) when the remembered prefix vanished.
    pub fn kanban_picker_index(&self, rows: &[Option<String>]) -> usize {
        let selected = match &self.state {
            AppState::Kanban(v) => v.picker.as_ref().map(|p| p.selected.clone()),
            _ => None,
        };
        selected
            .and_then(|sel| rows.iter().position(|r| *r == sel))
            .unwrap_or(0)
    }

    fn handle_kanban_filter_key(&mut self, key: KeyEvent) {
        let rows = self.kanban_filter_rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('f') | KeyCode::Char('q') => {
                if let AppState::Kanban(ref mut view) = self.state {
                    view.picker = None;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let idx = self.kanban_picker_index(&rows).saturating_sub(1);
                if let AppState::Kanban(ref mut view) = self.state {
                    if let Some(p) = view.picker.as_mut() {
                        p.selected = rows[idx].clone();
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let idx = (self.kanban_picker_index(&rows) + 1).min(rows.len() - 1);
                if let AppState::Kanban(ref mut view) = self.state {
                    if let Some(p) = view.picker.as_mut() {
                        p.selected = rows[idx].clone();
                    }
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                // Toggle by IDENTITY — never by row index, which a
                // background refresh can shift under the cursor.
                let selected = match &self.state {
                    AppState::Kanban(v) => v.picker.as_ref().map(|p| p.selected.clone()),
                    _ => None,
                };
                let Some(selected) = selected else { return };
                match &selected {
                    None => self.kanban_filter.loose = !self.kanban_filter.loose,
                    Some(prefix) => {
                        if let Some(pos) =
                            self.kanban_filter.folders.iter().position(|f| f == prefix)
                        {
                            self.kanban_filter.folders.remove(pos);
                        } else {
                            self.kanban_filter.folders.push(prefix.clone());
                        }
                    }
                }
                self.kanban_filter.canonicalize();
                if let Err(e) = self.persist_kanban_filter() {
                    self.error_message = Some(format!("failed to save filter: {}", e));
                }
                self.reanchor_kanban_focus();
            }
            KeyCode::Char('c') => {
                self.kanban_filter = crate::state::KanbanFilterState::default();
                if let Err(e) = self.persist_kanban_filter() {
                    self.error_message = Some(format!("failed to save filter: {}", e));
                }
                self.reanchor_kanban_focus();
            }
            _ => {}
        }
    }

    /// Mirror the in-memory filter to `state.toml` (merge semantics, like
    /// the placements).
    fn persist_kanban_filter(&self) -> Result<(), String> {
        let mut state = State::load();
        state.kanban.filter = self.kanban_filter.clone();
        state.save()
    }

    /// After a filter change, snap the board cursor onto what is actually
    /// visible. `resolve_kanban_focus` already falls back positionally
    /// when `focused_key` is filtered out — but the stale hidden key must
    /// then be REPLACED with the visible fallback card's identity (or
    /// cleared), otherwise re-enabling that folder later would teleport
    /// focus back to the long-hidden card. (Codex design review.)
    fn reanchor_kanban_focus(&mut self) {
        let board = kanban::build_board(
            &self.kanban_config,
            &self.tree.sessions,
            &self.kanban_placements,
            &self.kanban_filter,
        );
        let (col, card) = self.resolve_kanban_focus(&board);
        let key = board
            .get(col)
            .and_then(|c| c.get(card))
            .and_then(|&si| self.tree.session(si))
            .map(|s| (s.machine.clone(), s.raw_name.clone()));
        if let AppState::Kanban(ref mut view) = self.state {
            view.focused_col = col;
            view.focused_card = card;
            view.focused_key = key;
        }
    }

    /// Move the focused card one manual column left or right.
    ///
    /// The auto-active column is fenced at the operation boundary in both
    /// directions (safe under custom column orders, not just directional
    /// skipping): a Working card is pinned — a user move would be
    /// silently reverted by the next reconcile tick, so it's refused
    /// loudly instead — and a card crossing over auto-active skips it as
    /// a target. Moving *into* the auto-awaiting column removes the
    /// placement (auto-awaiting IS the "no manual placement" state, which
    /// makes "back to default" expressible with the same keys).
    fn move_focused_card(&mut self, dir: isize) {
        let board = kanban::build_board(
            &self.kanban_config,
            &self.tree.sessions,
            &self.kanban_placements,
            &self.kanban_filter,
        );
        let (col, card) = self.resolve_kanban_focus(&board);
        let Some(&session_idx) = board.get(col).and_then(|c| c.get(card)) else {
            return; // empty column
        };
        let Some(session) = self.tree.session(session_idx) else {
            return;
        };
        let key = (session.machine.clone(), session.raw_name.clone());

        let active_idx = self.kanban_config.auto_active_idx();
        let awaiting_idx = self.kanban_config.auto_awaiting_idx();
        if col == active_idx {
            self.error_message =
                Some("Claude is working — session stays in Active until it stops".to_string());
            return;
        }

        let ncols = self.kanban_config.columns.len() as isize;
        let mut target = col as isize + dir;
        while target >= 0 && target < ncols && target == active_idx as isize {
            target += dir;
        }
        if target < 0 || target >= ncols {
            return;
        }
        let target = target as usize;

        if target == awaiting_idx {
            self.kanban_placements.remove(&key);
        } else {
            let column_id = self.kanban_config.columns[target].id.clone();
            self.kanban_placements.insert(key.clone(), column_id);
        }

        // User-initiated change: a failed save deserves a banner.
        if let Err(e) = self.persist_kanban_placements() {
            self.error_message = Some(format!("failed to save kanban state: {}", e));
        }

        // Follow the card to its new column.
        if let AppState::Kanban(ref mut view) = self.state {
            view.focused_col = target;
            view.focused_key = Some(key);
            // focused_card re-resolves from focused_key on the next
            // build; keep a sane positional fallback meanwhile.
            view.focused_card = 0;
        }
        let board = kanban::build_board(
            &self.kanban_config,
            &self.tree.sessions,
            &self.kanban_placements,
            &self.kanban_filter,
        );
        let (new_col, new_card) = self.resolve_kanban_focus(&board);
        if let AppState::Kanban(ref mut view) = self.state {
            view.focused_col = new_col;
            view.focused_card = new_card;
        }
    }

    fn start_new_session_in_folder(&mut self, folder_idx: usize) {
        let Some(folder) = self.tree.folders.get(folder_idx) else {
            return;
        };
        self.state = AppState::CreatingSession(CreateForm::with_prefix(&folder.prefix));
        self.input_buffer.clear();
    }

    /// Deliver the oldest pending message to the currently-selected session
    /// (the `m` key). This is the human-triggered final hop of Track A: the
    /// user is present and chose the moment, which sidesteps the "is it
    /// really idle / is a human mid-draft" hazards of automatic injection.
    ///
    /// Still fail-closed on obviously-unsafe targets: refuse to inject unless
    /// the recipient has a live Claude pane that is idle at its prompt (not
    /// Working, not at a permission prompt). One message per keypress.
    fn deliver_selected_mail(&mut self) {
        let Some(Row::Session(idx)) = self.current_row() else {
            return;
        };
        // Only the session list acts on a session (mirrors the other
        // session-action keys); ignore in the title bar.
        if self.focus_area != FocusArea::SessionList {
            return;
        }
        let Some(session) = self.tree.session(idx) else {
            return;
        };
        let name = session.raw_name.clone();
        let machine = session.machine.clone();
        let req = DeliveryRequest {
            session_name: &name,
            is_local: matches!(machine, Machine::Local),
            has_pending_mail: self
                .mail_pending
                .get(&(machine.clone(), name.clone()))
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            claude_present: session.claude_present,
            claude: session.claude,
        };

        let Some(dir) = mail::mail_dir() else {
            self.error_message =
                Some("no $HOME / $XDG_CACHE_HOME — cannot locate the mailbox".to_string());
            return;
        };

        // Oldest pending message for this recipient; the in-memory view holds
        // payloads, not on-disk paths.
        let Some((path, msg)) = mail::read_inbox()
            .into_iter()
            .find(|(_, m)| m.to_session == name)
        else {
            self.error_message = Some(format!("no pending mail for '{}'", name));
            return;
        };

        let env = TmuxDelivery;
        let pane_id = match mail_delivery::gate(&env, &req, &msg.to_session_addr) {
            Gate::Deliver { pane_id } => pane_id,
            Gate::Refuse(reason) => {
                self.error_message = Some(reason);
                return;
            }
        };

        // Atomically lease it so a second ADE instance can't also deliver it.
        let claimed = match mail::claim(&dir, &path) {
            Ok(c) => c,
            Err(e) => {
                self.error_message = Some(format!("mail: {}", e));
                self.process_mail();
                return;
            }
        };

        self.error_message = Some(match mail_delivery::execute(
            &env, &dir, &claimed, &msg, &pane_id,
        ) {
            Outcome::Delivered => format!(
                "delivered mail from '{}' to '{}'",
                msg.from_session, name
            ),
            Outcome::DeliveredButNotArchived(e) => format!(
                "delivered to '{}', but couldn't archive the record: {}",
                name, e
            ),
            Outcome::Ambiguous(e) => format!(
                "mail delivery to '{}' is uncertain ({}). Not retrying — check \
                 that session's prompt; the record is held for recovery.",
                name, e
            ),
            Outcome::Requeued(e) => format!("mail delivery failed: {}", e),
            Outcome::RequeueFailed { send, requeue } => format!(
                "mail delivery failed ({}) and requeue failed ({}) — message is \
                 in the claimed/ dir",
                send, requeue
            ),
        });
        self.process_mail();
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

    fn start_duplicate_selected(&mut self) {
        let Some(Row::Session(idx)) = self.current_row() else {
            return;
        };
        let Some(session) = self.tree.session(idx) else {
            return;
        };
        let source = session.raw_name.clone();
        let machine = session.machine.clone();
        // Pre-fill with `<source>-copy`. Users can edit before committing.
        let suggested = format!("{}-copy", source);
        self.state = AppState::DuplicatingSession {
            source_name: source,
            machine,
        };
        self.input_buffer = TextField::from_str(&suggested);
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
                    alternate: None,
                });
            }
            Some(Row::Folder(idx)) => {
                let Some(folder) = self.tree.folders.get(idx) else {
                    return;
                };
                let prefix = folder.prefix.clone();
                // Build both action shapes from the same folder so the confirm
                // modal can offer "delete + kill" (primary) and "dissolve"
                // (alternate) without re-walking the tree.
                let rename_targets: Vec<(Machine, String, String)> = folder
                    .sessions
                    .iter()
                    .filter_map(|&i| self.tree.sessions.get(i))
                    .map(|s| (s.machine.clone(), s.raw_name.clone(), s.leaf.clone()))
                    .collect();
                let kill_targets: Vec<(Machine, String)> = rename_targets
                    .iter()
                    .map(|(m, raw, _)| (m.clone(), raw.clone()))
                    .collect();
                let n = kill_targets.len();
                let mut body = vec![format!(
                    "Delete folder \"{}\" — kill {} session{}:",
                    prefix,
                    n,
                    if n == 1 { "" } else { "s" }
                )];
                for (m, raw) in &kill_targets {
                    body.push(format!("  • {} ({})", raw, m.label()));
                }
                body.push(String::new());
                body.push(format!(
                    "All windows and unsaved work in those {} session{} will be lost.",
                    n,
                    if n == 1 { "" } else { "s" }
                ));
                self.state = AppState::Confirming(PendingConfirm {
                    title: "Delete folder".to_string(),
                    body,
                    action: PendingAction::DeleteFolder {
                        prefix: prefix.clone(),
                        targets: kill_targets,
                    },
                    alternate: Some(ConfirmAlternate {
                        key: 's',
                        label: "dissolve (keep sessions)".to_string(),
                        action: PendingAction::DissolveFolder {
                            prefix,
                            targets: rename_targets,
                        },
                    }),
                });
            }
            _ => {}
        }
    }

    fn activate_current(&mut self) {
        match self.current_row() {
            Some(Row::Folder(idx)) => match self.selected_action {
                SessionAction::Enter => {
                    self.tree.toggle_folder(idx);
                    self.persist_folder_expansion();
                }
                SessionAction::NewSession => self.start_new_session_in_folder(idx),
                SessionAction::Rename => self.start_rename_selected(),
                SessionAction::Delete => self.start_delete_selected(),
                // Folder rows can't reach Duplicate via cycling; the arm
                // exists only for type completeness.
                SessionAction::Duplicate => {}
            },
            Some(Row::Session(idx)) => {
                let session = match self.tree.session(idx) {
                    Some(s) => s.clone(),
                    None => return,
                };
                match self.selected_action {
                    // The same-session-no-op case is decided in
                    // main.rs::attach_outcome: when the user picks the
                    // session they're already in (only reachable from
                    // inside-tmux), the loop treats it as a no-op and
                    // keeps the picker on screen — switch-client to
                    // self would be silent anyway.
                    //
                    // NewSession is reachable here only if a refresh
                    // demoted a folder while its NewSession chip was
                    // active; the session row doesn't render that chip,
                    // so falling through to attach (rather than dead-Enter)
                    // gives the keypress a sensible default.
                    SessionAction::Enter | SessionAction::NewSession => {
                        self.action = AppAction::AttachSession {
                            name: session.raw_name,
                            machine: session.machine,
                        };
                        self.selected_action = SessionAction::Enter;
                    }
                    SessionAction::Rename => {
                        self.state = AppState::RenamingSession {
                            original_name: session.raw_name.clone(),
                            machine: session.machine,
                        };
                        self.input_buffer = TextField::from_str(&session.raw_name);
                    }
                    SessionAction::Duplicate => self.start_duplicate_selected(),
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
                // Drop the kanban placement eagerly (the observed-machine
                // prune would catch it next refresh; this keeps state.toml
                // tidy immediately). Only on success — a failed kill means
                // the session still exists.
                if self
                    .kanban_placements
                    .remove(&(machine.clone(), name.to_string()))
                    .is_some()
                {
                    let _ = self.persist_kanban_placements();
                }
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
        let mut placements_dirty = false;
        for (machine, from, to) in &targets {
            let result = match self.backend(machine) {
                Some(b) => b.rename_session(from, to),
                None => Err(format!("unknown host: {}", machine.label())),
            };
            match result {
                // Migrate the kanban placement per-target, only after that
                // target's tmux call succeeded — a failed rename means the
                // old name still exists, and migrating it anyway would
                // orphan the entry for the observed-machine prune to eat.
                Ok(()) => {
                    if let Some(col) = self
                        .kanban_placements
                        .remove(&(machine.clone(), from.clone()))
                    {
                        self.kanban_placements
                            .insert((machine.clone(), to.clone()), col);
                        placements_dirty = true;
                    }
                }
                Err(e) => errors.push(format!("{}: {}", from, e)),
            }
        }
        if placements_dirty {
            let _ = self.persist_kanban_placements();
        }
        if !errors.is_empty() {
            self.error_message = Some(errors.join("; "));
        }
        self.refresh();
        self.selected_action = SessionAction::Enter;
    }

    /// Kill a batch of sessions across machines. Errors are collected so one
    /// missing session (e.g. killed externally between modal-open and confirm)
    /// doesn't abort the rest of the batch.
    fn execute_kill_sessions(&mut self, targets: Vec<(Machine, String)>) {
        let mut errors: Vec<String> = Vec::new();
        let mut placements_dirty = false;
        for (machine, name) in &targets {
            let result = match self.backend(machine) {
                Some(b) => b.kill_session(name),
                None => Err(format!("unknown host: {}", machine.label())),
            };
            match result {
                Ok(()) => {
                    placements_dirty |= self
                        .kanban_placements
                        .remove(&(machine.clone(), name.clone()))
                        .is_some();
                }
                Err(e) => errors.push(format!("{}: {}", name, e)),
            }
        }
        if placements_dirty {
            let _ = self.persist_kanban_placements();
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
            // Tab steps through every option in order: each available machine,
            // then Prefix, then Name. Past the last field it wraps back to the
            // first machine. This lets users navigate the whole modal without
            // touching arrows. ↑↓ keeps the field-only jump (Machine → Prefix
            // → Name) for users who want to skip machine cycling.
            //
            // If `form.machine` is stale (e.g. user deleted a host while the
            // modal was open), normalize back to Local before stepping so the
            // Tab cycle is consistent regardless of how we got here.
            KeyCode::Tab => {
                if form.focus == CreateField::Machine
                    && !machines.iter().any(|m| m == &form.machine)
                {
                    form.machine = machines.first().cloned().unwrap_or(Machine::Local);
                }
                match form.focus {
                    CreateField::Machine => {
                        let last_idx = machines.len().saturating_sub(1);
                        let cur_idx = machines
                            .iter()
                            .position(|m| m == &form.machine)
                            .unwrap_or(0);
                        if machines.is_empty() || cur_idx >= last_idx {
                            form.focus = CreateField::Prefix;
                        } else {
                            form.machine = machines[cur_idx + 1].clone();
                        }
                    }
                    CreateField::Prefix => form.focus = CreateField::Name,
                    CreateField::Name => {
                        form.focus = CreateField::Machine;
                        if let Some(first) = machines.first() {
                            form.machine = first.clone();
                        }
                    }
                }
            }
            KeyCode::BackTab => {
                if form.focus == CreateField::Machine
                    && !machines.iter().any(|m| m == &form.machine)
                {
                    form.machine = machines.first().cloned().unwrap_or(Machine::Local);
                }
                match form.focus {
                    CreateField::Machine => {
                        let cur_idx = machines
                            .iter()
                            .position(|m| m == &form.machine)
                            .unwrap_or(0);
                        if cur_idx == 0 {
                            form.focus = CreateField::Name;
                        } else {
                            form.machine = machines[cur_idx - 1].clone();
                        }
                    }
                    CreateField::Prefix => {
                        form.focus = CreateField::Machine;
                        if let Some(last) = machines.last() {
                            form.machine = last.clone();
                        }
                    }
                    CreateField::Name => form.focus = CreateField::Prefix,
                }
            }
            KeyCode::Down => form.focus = form.focus.next(),
            KeyCode::Up => form.focus = form.focus.prev(),
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

    fn handle_duplicating_session_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
            }
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() {
                    self.execute_duplicate_session();
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

    fn execute_duplicate_session(&mut self) {
        let new_name = self.input_buffer.trim().to_string();
        if new_name.is_empty() {
            crate::duplicate_log::log("execute: aborted (empty new_name)");
            return;
        }
        let (source, machine) = if let AppState::DuplicatingSession {
            source_name,
            machine,
        } = &self.state
        {
            (source_name.clone(), machine.clone())
        } else {
            crate::duplicate_log::log("execute: aborted (state was not DuplicatingSession)");
            return;
        };
        crate::duplicate_log::log(&format!(
            "execute: source={:?} new_name={:?} machine={:?}",
            source,
            new_name,
            machine.label()
        ));

        // Pre-validate name collision on the same host so the banner reads
        // cleanly instead of surfacing raw tmux stderr.
        let collision = self.tree.sessions.iter().any(|s| {
            s.machine == machine && s.raw_name == new_name
        });
        if collision {
            crate::duplicate_log::log("execute: collision precheck rejected");
            self.error_message = Some(format!(
                "session '{}' already exists on {}",
                new_name,
                machine.label()
            ));
            self.state = AppState::Tree;
            self.input_buffer.clear();
            self.selected_action = SessionAction::Enter;
            return;
        }

        // Pull `claude_running` from the already-refreshed session field —
        // computed identically for local and remote by the refresh loop's
        // descendant-PID walk, so local and remote duplicate share detection.
        // We key off `claude_present` (any Claude pane) rather than
        // `claude.is_some()` (active state only): an idle Claude sitting
        // at its prompt is exactly the case we want to fork, but it has
        // no `state` and would be missed by the active-state check.
        let claude_running = self
            .tree
            .sessions
            .iter()
            .find(|s| s.machine == machine && s.raw_name == source)
            .map(|s| s.claude_present)
            .unwrap_or(false);
        crate::duplicate_log::log(&format!(
            "execute: claude_running={} (resolved from tree.sessions)",
            claude_running
        ));

        let result = match self.backend(&machine) {
            Some(b) => {
                crate::duplicate_log::log("execute: calling backend.duplicate_session");
                b.duplicate_session(&source, &new_name, claude_running)
            }
            None => {
                crate::duplicate_log::log(&format!(
                    "execute: backend lookup returned None for machine={:?}",
                    machine.label()
                ));
                Err(format!("unknown host: {}", machine.label()))
            }
        };
        match result {
            Ok(()) => {
                crate::duplicate_log::log("execute: backend returned Ok — calling refresh()");
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
                self.refresh();
            }
            Err(e) => {
                crate::duplicate_log::log(&format!("execute: backend returned Err: {:?}", e));
                self.error_message = Some(e);
                self.state = AppState::Tree;
                self.input_buffer.clear();
                self.selected_action = SessionAction::Enter;
            }
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
            alternate: None,
        });
        self.input_buffer.clear();
    }

    fn handle_confirming_key(&mut self, key: KeyEvent) {
        // Pull both the primary action and the optional alternate up front.
        // The alternate key is matched after the cancel/confirm keys so it
        // can't accidentally hijack Esc/n/y/Enter.
        let (primary, alternate) = match self.state {
            AppState::Confirming(ref c) => (Some(c.action.clone()), c.alternate.clone()),
            _ => (None, None),
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.state = AppState::Tree;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let Some(action) = primary else { return };
                self.state = AppState::Tree;
                self.dispatch_confirm_action(action);
            }
            KeyCode::Char(ch) => {
                if let Some(alt) = alternate {
                    if ch == alt.key {
                        self.state = AppState::Tree;
                        self.dispatch_confirm_action(alt.action);
                    }
                }
            }
            _ => {}
        }
    }

    fn dispatch_confirm_action(&mut self, action: PendingAction) {
        match action {
            PendingAction::KillSession { machine, name } => {
                self.execute_kill_session(machine, &name);
            }
            PendingAction::DeleteFolder { targets, .. } => {
                self.execute_kill_sessions(targets);
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
                // Carry the kanban placement across the rename so a Done
                // session doesn't reset to auto-awaiting.
                if let Some(col) = self
                    .kanban_placements
                    .remove(&(machine.clone(), original.clone()))
                {
                    self.kanban_placements
                        .insert((machine.clone(), new_name.clone()), col);
                    let _ = self.persist_kanban_placements();
                }
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
                        alternate: None,
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

    /// A host was renamed in the host form: re-key every kanban placement
    /// from the old host name to the new one. (Host *deletion* deliberately
    /// preserves placements — the machine is never observed afterward, so
    /// the entries are inert and survive a re-add of the same host.)
    fn migrate_host_placements(&mut self, old: &str, new: &str) {
        let keys: Vec<(Machine, String)> = self
            .kanban_placements
            .keys()
            .filter(|(m, _)| matches!(m, Machine::Remote(n) if n == old))
            .cloned()
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            if let Some(col) = self.kanban_placements.remove(&key) {
                self.kanban_placements
                    .insert((Machine::Remote(new.to_string()), key.1), col);
            }
        }
        let _ = self.persist_kanban_placements();
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
                    // Captured before the upsert so a host *rename* can
                    // migrate kanban placements keyed by the old name.
                    let old_host_name = editing_idx
                        .and_then(|i| self.config.hosts.get(i))
                        .map(|h| h.name.clone());
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
                            if let Some(old) = old_host_name {
                                if old != host_name {
                                    self.migrate_host_placements(&old, &host_name);
                                }
                            }
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

/// Merge an on-disk `closed` list with a freshly-observed expansion
/// snapshot to produce the next persisted closed list. Pure function so
/// it's covered by unit tests without needing an `App`.
///
/// Semantic: prefixes the snapshot doesn't mention are *preserved* from
/// the previous list. For prefixes the snapshot does mention, the
/// snapshot is authoritative — collapsed → present, expanded → absent.
/// Result is sorted for stable diffs.
fn compute_closed_list(prev_closed: &[String], snapshot: HashMap<String, bool>) -> Vec<String> {
    let mut closed: std::collections::HashSet<String> = prev_closed.iter().cloned().collect();
    for (prefix, expanded) in snapshot {
        if expanded {
            closed.remove(&prefix);
        } else {
            closed.insert(prefix);
        }
    }
    let mut out: Vec<String> = closed.into_iter().collect();
    out.sort();
    out
}

/// Production `DeliveryEnv`: every method is a live tmux query. Kept as a
/// zero-sized adapter so the policy in `mail_delivery` stays testable with a
/// fake while production keeps talking to the real tmux server.
pub struct TmuxDelivery;

impl DeliveryEnv for TmuxDelivery {
    /// ONE query for both the session address and every pane across ALL its
    /// windows. Session-scope (`-s`) is essential: without it `list-panes`
    /// reports only the active window, so a second window could hide a shell we
    /// would then type into. Reading the address here (rather than trusting the
    /// last refresh) is what catches a session killed and recreated between
    /// refreshes.
    fn live_session(&self, name: &str) -> Option<(String, Vec<String>)> {
        let target = format!("={}", name);
        let out = std::process::Command::new("tmux")
            .args([
                "list-panes",
                "-s",
                "-t",
                &target,
                "-F",
                "#{pid}:#{session_id}\t#{pane_id}",
            ])
            .output()
            .ok()?;
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
            if p.is_empty() {
                continue;
            }
            // `-s` scopes to one session, so every row carries the same
            // address; record it once.
            if addr.is_empty() {
                addr = a.to_string();
            }
            panes.push(p.to_string());
        }
        if addr.is_empty() {
            return None;
        }
        Some((addr, panes))
    }

    fn composer(&self, pane_id: &str) -> ComposerState {
        match std::process::Command::new("tmux")
            .args(["capture-pane", "-e", "-p", "-t", pane_id])
            .output()
        {
            Ok(o) if o.status.success() => {
                mail_delivery::composer_state(&String::from_utf8_lossy(&o.stdout))
            }
            // Unreadable pane must not be mistaken for an empty composer.
            _ => ComposerState::Unknown,
        }
    }

    fn send_text(&self, pane_id: &str, text: &str) -> Result<(), crate::tmux::SendTextError> {
        tmux::local().send_text(pane_id, text)
    }
}

/// Decide whether a `(prior, new)` per-session state pair should fire a
/// "Claude is waiting for you" banner. Mirrors the transition table in
/// `lets-add-a-new-toasty-narwhal.md`. Per-key suppression rules
/// (current session, embedded target, demote, debounce) are handled
/// separately by `App::suppress_transition`.
fn is_fire_transition(prior: Option<ClaudeState>, new: Option<ClaudeState>) -> bool {
    use ClaudeState::*;
    match (prior, new) {
        // Active → finished: the headline notification.
        (Some(Working), None) | (Some(AwaitingApproval), None) => true,
        // Working → AwaitingApproval: switched to needing approval.
        (Some(Working), Some(AwaitingApproval)) => true,
        // None → AwaitingApproval: needs approval from a fresh state.
        (None, Some(AwaitingApproval)) => true,
        // Everything else (Idle → Working = user's own action,
        // Working → Working = still going, AwaitingApproval → Working
        // = approval granted + work resumed, identity transitions) is
        // not user-facing.
        _ => false,
    }
}

#[cfg(test)]
mod is_fire_transition_tests {
    use super::*;
    use ClaudeState::*;

    #[test]
    fn working_to_none_fires() {
        assert!(is_fire_transition(Some(Working), None));
    }

    #[test]
    fn awaiting_to_none_fires() {
        assert!(is_fire_transition(Some(AwaitingApproval), None));
    }

    #[test]
    fn none_to_working_does_not_fire() {
        // User's own action — they just submitted a prompt.
        assert!(!is_fire_transition(None, Some(Working)));
    }

    #[test]
    fn working_to_working_does_not_fire() {
        assert!(!is_fire_transition(Some(Working), Some(Working)));
    }

    #[test]
    fn working_to_awaiting_fires() {
        assert!(is_fire_transition(Some(Working), Some(AwaitingApproval)));
    }

    #[test]
    fn awaiting_to_working_does_not_fire() {
        // Approval granted; work resumed.
        assert!(!is_fire_transition(Some(AwaitingApproval), Some(Working)));
    }

    #[test]
    fn none_to_awaiting_fires() {
        assert!(is_fire_transition(None, Some(AwaitingApproval)));
    }

    #[test]
    fn none_to_none_does_not_fire() {
        assert!(!is_fire_transition(None, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, bool)]) -> HashMap<String, bool> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    #[test]
    fn compute_closed_list_inserts_collapsed_visible_folders() {
        let out = compute_closed_list(&[], snap(&[("work", false), ("infra", true)]));
        assert_eq!(out, vec!["work".to_string()]);
    }

    #[test]
    fn compute_closed_list_removes_expanded_visible_folders() {
        let prev = vec!["work".to_string(), "infra".to_string()];
        let out = compute_closed_list(&prev, snap(&[("work", true)]));
        // `infra` not in snapshot → preserved. `work` expanded → removed.
        assert_eq!(out, vec!["infra".to_string()]);
    }

    #[test]
    fn compute_closed_list_preserves_unobserved_prefixes() {
        // The bug Codex flagged: a remote host is unreachable, its folder
        // is collapsed in the user's preference, snapshot doesn't include
        // it. We must NOT drop it just because the user toggled some
        // other folder.
        let prev = vec!["infra".to_string()];
        let out = compute_closed_list(&prev, snap(&[("work", false)]));
        let mut expected = vec!["infra".to_string(), "work".to_string()];
        expected.sort();
        assert_eq!(out, expected);
    }

    #[test]
    fn compute_closed_list_output_is_sorted() {
        let prev = vec!["zeta".to_string(), "alpha".to_string()];
        let out = compute_closed_list(&prev, snap(&[("mu", false)]));
        assert_eq!(
            out,
            vec!["alpha".to_string(), "mu".to_string(), "zeta".to_string()]
        );
    }
}

