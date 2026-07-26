//! Non-interactive JSON output: `ade sessions --json` and `ade status --json`.
//!
//! These reuse the exact cross-host collection core the TUI uses
//! (`refresh::refresh_all`, which fans out `tmux::local()` + one thread per
//! configured host) — no tmux/SSH polling is reimplemented here. The only
//! job of this module is to project the collected `tmux::Session` list into
//! stable, documented DTOs and emit them as JSON on **stdout** (all logs and
//! warnings go to **stderr** so the JSON is never polluted).
//!
//! # Stability contract
//!
//! The DTO structs below — *not* the internal `tmux::Session` — are the
//! schema external consumers (e.g. the glasses app) depend on. Add fields
//! additively; do not rename or repurpose existing ones. `serde_json`
//! preserves declaration order, so the field order here is the wire order.
//!
//! ## `ade sessions --json`
//!
//! ```json
//! {
//!   "sessions": [
//!     {
//!       "machine": "local",          // "local" or a configured host name
//!       "name": "archie/api",        // raw tmux session name
//!       "prefix": "archie",          // folder prefix, or null if none
//!       "leaf": "api",               // name after the prefix (whole name if no prefix)
//!       "session_id": "$3",          // tmux #{session_id} (stable across rename)
//!       "windows": 2,
//!       "attached": false,
//!       "claude": {                  // null when no Claude pane in the session
//!         "state": "working",        // "working" | "idle" | "awaiting_approval"
//!         "context_pct": 15,         // 0..=100, or null if no usage seen yet
//!         "model": "claude-opus-4-8",// or null
//!         "ctx_tokens": 150000,      // latest-turn input+cache tokens, or null
//!         "session_id": "c7d9-..."   // Claude session UUID, or null
//!       }
//!     }
//!   ],
//!   "errors": [                      // per-host collection failures (never dropped)
//!     { "machine": "web", "error": "web unreachable" }
//!   ]
//! }
//! ```
//!
//! `claude` is present whenever the session has a Claude pane (working, idle,
//! or awaiting approval). `state` is `"idle"` for a Claude sitting at the
//! prompt. `context_pct`/`model`/`ctx_tokens`/`session_id` are `null` when no
//! status file with usage data has been written yet (legacy hook, or no
//! assistant turn has happened).
//!
//! ## `ade status --json`
//!
//! ```json
//! {
//!   "totals": {
//!     "sessions": 12,          // all tmux sessions across every machine
//!     "working": 2,            // Claude sessions actively working
//!     "idle": 5,               // Claude sessions present but idle
//!     "awaiting_approval": 1   // Claude sessions waiting on a permission prompt
//!   },
//!   "worst_context_pct": 95,   // highest context_pct across Claude sessions, or null
//!   "needs_you": [             // one row per awaiting_approval session
//!     { "machine": "local", "name": "archie/api", "session_id": "$3" }
//!   ],
//!   "sessions": [              // compact row per Claude-present session
//!     { "machine": "local", "name": "archie/api", "session_id": "$3",
//!       "state": "working", "context_pct": 15 }
//!   ],
//!   "errors": [ { "machine": "web", "error": "web unreachable" } ]
//! }
//! ```
//!
//! `working + idle + awaiting_approval` partition the Claude-present sessions;
//! non-Claude tmux sessions count only toward `totals.sessions`.
//!
//! # Exit codes
//!
//! `0` whenever JSON was emitted (including when `errors[]` is non-empty — a
//! host being unreachable is data, not a command failure). Nonzero only on
//! bad arguments or a serialization failure.

use serde::Serialize;

use crate::claude_status::ClaudeState;
use crate::hosts::Config;
use crate::model::{split_prefix_leaf, Machine};
use crate::refresh::{refresh_all, RefreshResult};
use crate::tmux::Session as TmuxSession;

fn state_str(s: ClaudeState) -> &'static str {
    match s {
        ClaudeState::Working => "working",
        ClaudeState::Idle => "idle",
        ClaudeState::AwaitingApproval => "awaiting_approval",
    }
}

/// Claude sub-object of a session row. See module docs for field meaning.
#[derive(Debug, Serialize, PartialEq)]
pub struct ClaudeDto {
    pub state: &'static str,
    pub context_pct: Option<u8>,
    pub model: Option<String>,
    pub ctx_tokens: Option<u64>,
    pub session_id: Option<String>,
}

/// One full session row for `ade sessions --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct SessionDto {
    pub machine: String,
    pub name: String,
    pub prefix: Option<String>,
    pub leaf: String,
    pub session_id: String,
    pub windows: u32,
    pub attached: bool,
    pub claude: Option<ClaudeDto>,
}

/// A per-host collection failure, surfaced instead of being dropped.
#[derive(Debug, Serialize, PartialEq)]
pub struct HostErrorDto {
    pub machine: String,
    pub error: String,
}

/// Top-level payload for `ade sessions --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct SessionsOutput {
    pub sessions: Vec<SessionDto>,
    pub errors: Vec<HostErrorDto>,
}

/// Counts block for `ade status --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct Totals {
    pub sessions: usize,
    pub working: usize,
    pub idle: usize,
    pub awaiting_approval: usize,
}

/// One `needs_you` entry (a session awaiting a permission approval).
#[derive(Debug, Serialize, PartialEq)]
pub struct NeedsYouDto {
    pub machine: String,
    pub name: String,
    pub session_id: String,
}

/// Compact per-session Claude row inside `ade status --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct StatusRowDto {
    pub machine: String,
    pub name: String,
    pub session_id: String,
    pub state: &'static str,
    pub context_pct: Option<u8>,
}

/// Top-level payload for `ade status --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct StatusOutput {
    pub totals: Totals,
    pub worst_context_pct: Option<u8>,
    pub needs_you: Vec<NeedsYouDto>,
    pub sessions: Vec<StatusRowDto>,
    pub errors: Vec<HostErrorDto>,
}

/// Build the `claude` sub-object for a session, or `None` when the session
/// has no Claude pane. An idle Claude (no active state, but `claude_present`)
/// renders `state: "idle"`.
fn claude_dto(s: &TmuxSession) -> Option<ClaudeDto> {
    if !s.claude_present {
        return None;
    }
    let state = s.claude.map(state_str).unwrap_or("idle");
    let (model, ctx_tokens, session_id) = match &s.claude_usage {
        Some(u) => (
            Some(u.model.clone()),
            Some(u.tokens),
            Some(u.session_id.clone()),
        ),
        None => (None, None, None),
    };
    Some(ClaudeDto {
        state,
        context_pct: s.claude_context_pct,
        model,
        ctx_tokens,
        session_id,
    })
}

fn errors_dto(errors: &[(Machine, String)]) -> Vec<HostErrorDto> {
    errors
        .iter()
        .map(|(machine, error)| HostErrorDto {
            machine: machine.title_label().to_string(),
            error: error.clone(),
        })
        .collect()
}

/// Project the collected per-machine session lists into the full
/// `sessions --json` payload. Pure — takes exactly what it needs so tests
/// can drive it from fixtures without a live `RefreshResult`.
pub fn build_sessions_output(
    per_machine: &[(Machine, Vec<TmuxSession>)],
    errors: &[(Machine, String)],
) -> SessionsOutput {
    let mut sessions = Vec::new();
    for (machine, list) in per_machine {
        let m = machine.title_label();
        for s in list {
            let (prefix, leaf) = split_prefix_leaf(&s.name);
            sessions.push(SessionDto {
                machine: m.to_string(),
                name: s.name.clone(),
                prefix,
                leaf,
                session_id: s.session_id.clone(),
                windows: s.windows,
                attached: s.attached,
                claude: claude_dto(s),
            });
        }
    }
    SessionsOutput {
        sessions,
        errors: errors_dto(errors),
    }
}

/// Project the collected per-machine session lists into the monitor-summary
/// `status --json` payload. Pure, for the same reason as above.
pub fn build_status_output(
    per_machine: &[(Machine, Vec<TmuxSession>)],
    errors: &[(Machine, String)],
) -> StatusOutput {
    let mut totals = Totals {
        sessions: 0,
        working: 0,
        idle: 0,
        awaiting_approval: 0,
    };
    let mut worst: Option<u8> = None;
    let mut needs_you = Vec::new();
    let mut rows = Vec::new();

    for (machine, list) in per_machine {
        let m = machine.title_label();
        for s in list {
            totals.sessions += 1;
            if !s.claude_present {
                continue;
            }
            let state = s.claude.map(state_str).unwrap_or("idle");
            match s.claude {
                Some(ClaudeState::Working) => totals.working += 1,
                Some(ClaudeState::AwaitingApproval) => {
                    totals.awaiting_approval += 1;
                    needs_you.push(NeedsYouDto {
                        machine: m.to_string(),
                        name: s.name.clone(),
                        session_id: s.session_id.clone(),
                    });
                }
                // Idle Claude (state None or, defensively, an explicit Idle):
                // present but not working/awaiting.
                _ => totals.idle += 1,
            }
            if let Some(pct) = s.claude_context_pct {
                worst = Some(worst.map_or(pct, |w| w.max(pct)));
            }
            rows.push(StatusRowDto {
                machine: m.to_string(),
                name: s.name.clone(),
                session_id: s.session_id.clone(),
                state,
                context_pct: s.claude_context_pct,
            });
        }
    }

    StatusOutput {
        totals,
        worst_context_pct: worst,
        needs_you,
        sessions: rows,
        errors: errors_dto(errors),
    }
}

/// Parsed flags shared by both subcommands.
struct Opts {
    local_only: bool,
}

/// Parse `--json` (required) and `--local` (optional). Any other argument is
/// an error. `--json` is mandatory so the command's contract stays explicit
/// and a future human-readable `ade sessions` can be added without surprise.
fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut json = false;
    let mut local_only = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--local" => local_only = true,
            other => return Err(format!("unknown argument '{}'", other)),
        }
    }
    if !json {
        return Err("this command requires --json".to_string());
    }
    Ok(Opts { local_only })
}

/// Collect the session tree via the shared refresh core. `local_only` swaps
/// in an empty host config so `refresh_all` polls local only (fast path).
/// A malformed `hosts.toml` warns on stderr and proceeds with no remotes.
fn collect(local_only: bool) -> RefreshResult {
    let (config, warning) = Config::load();
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    let config = if local_only {
        Config::default()
    } else {
        config
    };
    refresh_all(&config)
}

fn print_json<T: Serialize>(value: &T) -> Result<(), String> {
    let s = serde_json::to_string_pretty(value).map_err(|e| format!("serialize json: {}", e))?;
    println!("{}", s);
    Ok(())
}

/// Dispatch helper: parse, collect, emit, and translate errors into stderr +
/// a nonzero exit. Never prints anything but JSON to stdout.
fn run<T: Serialize>(args: &[String], build: impl Fn(&RefreshResult) -> T) -> ! {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
    };
    let result = collect(opts.local_only);
    let payload = build(&result);
    match print_json(&payload) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// `ade sessions --json [--local]`.
pub fn run_sessions(args: &[String]) -> ! {
    run(args, |r| build_sessions_output(&r.per_machine, &r.errors))
}

/// `ade status --json [--local]`.
pub fn run_status(args: &[String]) -> ! {
    run(args, |r| build_status_output(&r.per_machine, &r.errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_status::ContextUsage;

    /// Minimal `tmux::Session` builder for fixtures.
    fn sess(name: &str, sid: &str) -> TmuxSession {
        TmuxSession {
            name: name.to_string(),
            session_id: sid.to_string(),
            windows: 1,
            attached: false,
            claude: None,
            claude_demoted: false,
            claude_present: false,
            claude_context_pct: None,
            claude_usage: None,
        }
    }

    fn with_claude(
        mut s: TmuxSession,
        state: Option<ClaudeState>,
        pct: Option<u8>,
        usage: Option<ContextUsage>,
    ) -> TmuxSession {
        s.claude = state;
        s.claude_present = true;
        s.claude_context_pct = pct;
        s.claude_usage = usage;
        s
    }

    fn usage(model: &str, tokens: u64, sid: &str) -> ContextUsage {
        ContextUsage {
            tokens,
            model: model.to_string(),
            session_id: sid.to_string(),
        }
    }

    fn fixture() -> Vec<(Machine, Vec<TmuxSession>)> {
        // local: one working Claude (with usage), one plain shell (no claude).
        let working = with_claude(
            sess("archie/api", "$3"),
            Some(ClaudeState::Working),
            Some(15),
            Some(usage("claude-opus-4-8", 150_000, "c7d9-uuid")),
        );
        let plain = sess("scratch", "$4");
        // remote "web": one awaiting-approval Claude.
        let awaiting = with_claude(
            sess("deploy", "$1"),
            Some(ClaudeState::AwaitingApproval),
            Some(42),
            Some(usage("claude-opus-4-8", 420_000, "48af-uuid")),
        );
        vec![
            (Machine::Local, vec![working, plain]),
            (Machine::Remote("web".to_string()), vec![awaiting]),
        ]
    }

    #[test]
    fn sessions_output_shape_and_key_fields() {
        let pm = fixture();
        let out = build_sessions_output(&pm, &[]);
        let v = serde_json::to_value(&out).unwrap();

        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 3);

        // Working Claude row: machine "local", prefix/leaf split, full claude.
        let api = &sessions[0];
        assert_eq!(api["machine"], "local");
        assert_eq!(api["name"], "archie/api");
        assert_eq!(api["prefix"], "archie");
        assert_eq!(api["leaf"], "api");
        assert_eq!(api["session_id"], "$3");
        assert_eq!(api["claude"]["state"], "working");
        assert_eq!(api["claude"]["context_pct"], 15);
        assert_eq!(api["claude"]["model"], "claude-opus-4-8");
        assert_eq!(api["claude"]["ctx_tokens"], 150_000);
        assert_eq!(api["claude"]["session_id"], "c7d9-uuid");

        // Plain shell: null claude, null prefix.
        let scratch = &sessions[1];
        assert!(scratch["claude"].is_null(), "no Claude → claude:null");
        assert!(scratch["prefix"].is_null(), "no '/' → prefix:null");
        assert_eq!(scratch["leaf"], "scratch");

        // Remote machine label is the host name.
        let deploy = &sessions[2];
        assert_eq!(deploy["machine"], "web");
        assert_eq!(deploy["claude"]["state"], "awaiting_approval");
    }

    #[test]
    fn sessions_output_surfaces_errors() {
        let errs = vec![(
            Machine::Remote("web".to_string()),
            "web unreachable".to_string(),
        )];
        let out = build_sessions_output(&[], &errs);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["errors"][0]["machine"], "web");
        assert_eq!(v["errors"][0]["error"], "web unreachable");
    }

    #[test]
    fn status_output_totals_worst_and_needs_you() {
        let pm = fixture();
        let out = build_status_output(&pm, &[]);
        let v = serde_json::to_value(&out).unwrap();

        // 3 sessions total; 1 working, 0 idle, 1 awaiting (plain shell is not
        // Claude so it only counts toward totals.sessions).
        assert_eq!(v["totals"]["sessions"], 3);
        assert_eq!(v["totals"]["working"], 1);
        assert_eq!(v["totals"]["idle"], 0);
        assert_eq!(v["totals"]["awaiting_approval"], 1);

        // worst_context_pct is the max across Claude sessions (42 > 15).
        assert_eq!(v["worst_context_pct"], 42);

        // The awaiting_approval session surfaces in needs_you.
        let needs = v["needs_you"].as_array().unwrap();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0]["machine"], "web");
        assert_eq!(needs[0]["name"], "deploy");
        assert_eq!(needs[0]["session_id"], "$1");

        // Compact rows: one per Claude-present session (2 of the 3).
        let rows = v["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn status_idle_claude_counts_as_idle() {
        // Idle Claude: present, no active state, no usage yet → state "idle",
        // counts toward totals.idle, null context_pct.
        let idle = with_claude(sess("wip", "$9"), None, None, None);
        let pm = vec![(Machine::Local, vec![idle])];
        let out = build_status_output(&pm, &[]);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["totals"]["idle"], 1);
        assert_eq!(v["totals"]["working"], 0);
        assert_eq!(v["sessions"][0]["state"], "idle");
        assert!(v["sessions"][0]["context_pct"].is_null());
        assert!(v["worst_context_pct"].is_null());
    }

    #[test]
    fn idle_claude_sessions_dto_has_null_usage_fields() {
        let idle = with_claude(sess("wip", "$9"), None, None, None);
        let pm = vec![(Machine::Local, vec![idle])];
        let out = build_sessions_output(&pm, &[]);
        let v = serde_json::to_value(&out).unwrap();
        let claude = &v["sessions"][0]["claude"];
        assert_eq!(claude["state"], "idle");
        assert!(claude["model"].is_null());
        assert!(claude["ctx_tokens"].is_null());
        assert!(claude["session_id"].is_null());
        assert!(claude["context_pct"].is_null());
    }
}
