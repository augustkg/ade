//! Ambient right-side preview pane: caches the latest `tmux capture-pane`
//! output for each session and renders it with ANSI styling. Refreshes
//! happen on cursor moves and on a TTL while the same session stays
//! selected.
//!
//! Both local and remote captures run on a worker thread so the TUI never
//! blocks on tmux/SSH. Only one in-flight refresh per key — additional
//! requests while a worker is still running are dropped (we'll catch up on
//! the next tick once it finishes).
//!
//! ## Worker lifecycle on quit
//!
//! `JoinHandle`s live in `pending` only. If ADE exits while a worker is
//! mid-`Command::output()`, dropping the handle detaches the Rust thread
//! and any spawned `ssh`/`tmux` child can outlive ADE briefly until it
//! completes or hits its own timeout (`ConnectTimeout=2` from
//! `src/tmux/remote.rs::SSH_OPTS` — applies to connection setup, not to
//! an established stalled command). This is the same shape as the
//! existing background session-list refresh in `src/refresh.rs`. v1
//! accepts it; a cancellable worker model is a follow-up if it bites.

use std::collections::HashMap;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::hosts::Host;
use crate::model::Machine;
use crate::tmux::{local, remote::RemoteTmux};

/// How long a cached **local** capture stays fresh. Local capture is
/// effectively free (a single tmux IPC), so we refresh aggressively for
/// a near-live feel.
const LOCAL_OK_TTL: Duration = Duration::from_millis(500);

/// How long a cached **remote** capture stays fresh. Remote capture is
/// an SSH round-trip — connection setup, auth, transport. Refreshing
/// every 500ms across multiple hosts would hammer the remote (and the
/// audit log), so we slow it down substantially. Cursor movement still
/// triggers an immediate refresh regardless of TTL.
const REMOTE_OK_TTL: Duration = Duration::from_millis(2000);

/// Cap on how stale a *failed* capture can be before we retry. Slightly
/// longer than the success TTL on each tier to avoid thrashing when a
/// session just vanished or a host is flapping.
const LOCAL_ERR_TTL: Duration = Duration::from_millis(1500);
const REMOTE_ERR_TTL: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PreviewKey {
    pub machine: Machine,
    pub name: String,
}

#[derive(Debug)]
pub struct Capture {
    pub at: Instant,
    pub body: Result<String, String>,
}

pub struct PreviewPane {
    cache: HashMap<PreviewKey, Capture>,
    pending: HashMap<PreviewKey, JoinHandle<Result<String, String>>>,
}

impl PreviewPane {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    /// Called every TUI tick. Drains any finished worker threads into the
    /// cache, then schedules a new refresh for `selected` if one isn't
    /// already in flight and the existing entry is stale (or missing).
    /// Pass `None` when the cursor isn't on a previewable row.
    pub fn tick(&mut self, selected: Option<&PreviewKey>, hosts: &[Host]) {
        self.drain_finished();
        let Some(key) = selected else {
            return;
        };
        if self.pending.contains_key(key) {
            return;
        }
        if needs_refresh(&key.machine, self.cache.get(key)) {
            self.spawn_refresh(key.clone(), hosts);
        }
    }

    /// Latest cached capture for `key`, if any. The caller decides what to
    /// render for missing entries (e.g. a "loading" placeholder until the
    /// first refresh lands).
    pub fn get(&self, key: &PreviewKey) -> Option<&Capture> {
        self.cache.get(key)
    }

    fn drain_finished(&mut self) {
        let finished: Vec<PreviewKey> = self
            .pending
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in finished {
            if let Some(handle) = self.pending.remove(&key) {
                let body = handle
                    .join()
                    .unwrap_or_else(|_| Err("worker panicked".to_string()));
                self.cache.insert(
                    key,
                    Capture {
                        at: Instant::now(),
                        body,
                    },
                );
            }
        }
    }

    fn spawn_refresh(&mut self, key: PreviewKey, hosts: &[Host]) {
        let machine = key.machine.clone();
        let name = key.name.clone();
        let host_for_remote = match &machine {
            Machine::Local => None,
            Machine::Remote(host_name) => hosts.iter().find(|h| h.name == *host_name).cloned(),
        };
        let handle = std::thread::spawn(move || match machine {
            Machine::Local => local::capture_pane(&name),
            Machine::Remote(_) => match host_for_remote {
                Some(host) => RemoteTmux { host }.capture_pane(&name),
                None => Err("host not configured".to_string()),
            },
        });
        self.pending.insert(key, handle);
    }
}

impl Default for PreviewPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure decision: should we kick off a fresh capture for the currently-
/// selected key, given its cached state and the target machine? Extracted
/// so we can cover the per-machine TTL logic with a unit test without
/// touching threads or tmux. Remote captures use a longer TTL than local
/// because each refresh is an SSH round-trip — refreshing too often
/// hammers the remote and the audit log.
fn needs_refresh(machine: &Machine, cached: Option<&Capture>) -> bool {
    match cached {
        None => true,
        Some(c) => {
            let ttl = match (machine, c.body.is_ok()) {
                (Machine::Local, true) => LOCAL_OK_TTL,
                (Machine::Local, false) => LOCAL_ERR_TTL,
                (Machine::Remote(_), true) => REMOTE_OK_TTL,
                (Machine::Remote(_), false) => REMOTE_ERR_TTL,
            };
            c.at.elapsed() >= ttl
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(body: Result<&str, &str>, age: Duration) -> Capture {
        Capture {
            at: Instant::now()
                .checked_sub(age)
                .expect("test ages must fit in Instant"),
            body: body.map(String::from).map_err(String::from),
        }
    }

    #[test]
    fn missing_entry_needs_refresh() {
        assert!(needs_refresh(&Machine::Local, None));
        assert!(needs_refresh(&Machine::Remote("h".into()), None));
    }

    #[test]
    fn fresh_local_success_does_not_refresh() {
        let c = capture(Ok("ok"), Duration::from_millis(50));
        assert!(!needs_refresh(&Machine::Local, Some(&c)));
    }

    #[test]
    fn stale_local_success_refreshes() {
        let c = capture(Ok("ok"), LOCAL_OK_TTL + Duration::from_millis(10));
        assert!(needs_refresh(&Machine::Local, Some(&c)));
    }

    #[test]
    fn fresh_local_error_does_not_refresh() {
        let c = capture(Err("boom"), LOCAL_OK_TTL + Duration::from_millis(10));
        assert!(!needs_refresh(&Machine::Local, Some(&c)));
    }

    #[test]
    fn stale_local_error_refreshes() {
        let c = capture(Err("boom"), LOCAL_ERR_TTL + Duration::from_millis(10));
        assert!(needs_refresh(&Machine::Local, Some(&c)));
    }

    #[test]
    fn remote_uses_longer_ttl_than_local() {
        // A capture stale by local-TTL+10ms should NOT trigger a remote
        // refresh — the remote tier has its own (longer) TTL and shouldn't
        // shell SSH that aggressively.
        let just_past_local = capture(Ok("ok"), LOCAL_OK_TTL + Duration::from_millis(10));
        assert!(needs_refresh(&Machine::Local, Some(&just_past_local)));
        assert!(!needs_refresh(&Machine::Remote("h".into()), Some(&just_past_local)));
    }

    #[test]
    fn stale_remote_success_refreshes() {
        let c = capture(Ok("ok"), REMOTE_OK_TTL + Duration::from_millis(10));
        assert!(needs_refresh(&Machine::Remote("h".into()), Some(&c)));
    }
}
