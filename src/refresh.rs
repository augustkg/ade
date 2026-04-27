use std::collections::HashMap;
use std::thread::{self, JoinHandle};

use crate::hosts::Config;
use crate::install_hooks;
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
}

type RemoteHandle = (String, JoinHandle<Result<RemoteRefresh, String>>);

pub fn refresh_all(config: &Config) -> RefreshResult {
    // Local always present; treat any local error as empty so the UI never disappears.
    let local_handle =
        thread::spawn(|| tmux::local().list_sessions().unwrap_or_default());

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

    let local = local_handle.join().unwrap_or_default();
    let mut per_machine: Vec<(Machine, Vec<Session>)> = vec![(Machine::Local, local)];
    let mut errors: Vec<(Machine, String)> = Vec::new();
    let mut remote_hooks: HashMap<String, Option<bool>> = HashMap::new();

    for (name, handle) in remote_handles {
        let machine = Machine::Remote(name.clone());
        match handle.join() {
            Ok(Ok(refresh)) => {
                remote_hooks.insert(name, refresh.hooks_installed);
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
    }
}
