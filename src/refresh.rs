use std::collections::HashMap;
use std::thread::{self, JoinHandle};

use crate::hosts::Config;
use crate::install_hooks;
use crate::install_tmux::{self, InstallStatus};
use crate::model::Machine;
use crate::tmux::remote::{RemoteRefresh, RemoteTmux};
use crate::tmux::{self, Session, TmuxBackend};

pub struct RefreshResult {
    pub per_machine: Vec<(Machine, Vec<Session>)>,
    pub errors: Vec<(Machine, String)>,
    /// Per-host hook install status, keyed by host name. `Some(true)` =
    /// installed, `Some(false)` = missing, `None` = couldn't determine
    /// (host unreachable, older shell that didn't echo the marker, etc.).
    pub remote_hooks: HashMap<String, Option<bool>>,
    /// True if the local `~/.claude/settings.local.json` contains ADE's
    /// hook marker. Cheap to compute (single file read).
    pub local_hooks_installed: bool,
    /// Local install state of the ADE-managed tmux clipboard config. Drives
    /// the in-TUI nudge that points users at `ade install-tmux-config`.
    pub local_tmux_config_status: InstallStatus,
    /// Name of the local tmux session the user's current client is viewing,
    /// if ADE was launched from inside tmux. Used for the `· here` marker.
    pub current_session: Option<String>,
    /// Machines whose session list was *successfully* observed this
    /// refresh. Distinct from "not in `errors`": a local tmux failure is
    /// deliberately flattened to an empty session list with no error entry
    /// (so the UI never disappears), which would otherwise be
    /// indistinguishable from "zero local sessions". Consumers that prune
    /// per-session state (kanban placements) must only trust absences on
    /// machines listed here.
    pub observed: Vec<Machine>,
}

type RemoteHandle = (String, JoinHandle<Result<RemoteRefresh, String>>);

pub fn refresh_all(config: &Config) -> RefreshResult {
    // Local always present; a local error renders as an empty list so the
    // UI never disappears — but the Result is kept so `observed` can tell
    // "empty" apart from "failed".
    let local_handle = thread::spawn(|| tmux::local().list_sessions());

    // One thread per configured remote host. Each thread runs the combined
    // session+pane+status+hooks query and returns a RemoteRefresh.
    let remote_handles: Vec<RemoteHandle> = config
        .hosts
        .iter()
        .map(|host| {
            let host = host.clone();
            let name = host.name.clone();
            let handle = thread::spawn(move || RemoteTmux { host }.refresh());
            (name, handle)
        })
        .collect();

    let mut observed: Vec<Machine> = Vec::new();
    let local = match local_handle.join() {
        Ok(Ok(list)) => {
            observed.push(Machine::Local);
            list
        }
        // Worker panic or tmux failure: empty list, NOT observed.
        _ => Vec::new(),
    };
    let mut per_machine: Vec<(Machine, Vec<Session>)> = vec![(Machine::Local, local)];
    let mut errors: Vec<(Machine, String)> = Vec::new();
    let mut remote_hooks: HashMap<String, Option<bool>> = HashMap::new();

    for (name, handle) in remote_handles {
        let machine = Machine::Remote(name.clone());
        match handle.join() {
            Ok(Ok(refresh)) => {
                remote_hooks.insert(name, refresh.hooks_installed);
                // SSH succeeded, but only trust "these sessions are all
                // there is" when list-sessions itself positively succeeded
                // (see RemoteRefresh::sessions_observed).
                if refresh.sessions_observed {
                    observed.push(machine.clone());
                }
                per_machine.push((machine, refresh.sessions));
            }
            Ok(Err(e)) => {
                remote_hooks.insert(name, None);
                errors.push((machine, e));
            }
            Err(_) => {
                remote_hooks.insert(name, None);
                errors.push((machine, "worker panicked".to_string()));
            }
        }
    }

    RefreshResult {
        per_machine,
        errors,
        remote_hooks,
        local_hooks_installed: install_hooks::is_installed_local(),
        local_tmux_config_status: install_tmux::detect_local_status()
            .unwrap_or(InstallStatus::Missing),
        current_session: tmux::current_session(),
        observed,
    }
}
