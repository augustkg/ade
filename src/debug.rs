//! `ade debug claude` — diagnostic table explaining, for each tmux pane on
//! each configured machine, why ADE does or does not see Claude running
//! there. Prints to stdout and exits; never starts the TUI.
//!
//! This is the single artifact the user can reach for when the ` claude `
//! chip doesn't appear as expected — it shows the raw inputs (pane command,
//! pane pid, descendant `claude` processes, status file presence, hooks
//! marker) plus the decision ADE would make from them.

use std::collections::HashMap;
use std::process::Command;

use crate::claude_status::{self, ClaudeState};
use crate::hosts::{Config, Host, HostKind};
use crate::install_hooks;
use crate::tmux::parse_pane_line;

const PANE_FORMAT: &str =
    "#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}";

const REMOTE_DEBUG_CMD: &str = concat!(
    "tmux list-panes -a -F '#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}' 2>/dev/null; ",
    "echo '---ADE-PS---'; ",
    "ps -A -o pid,ppid,comm 2>/dev/null; ",
    "echo '---ADE-STATUS---'; ",
    "for f in \"$HOME\"/.cache/ade/claude-status/*.json; do ",
    "  [ -f \"$f\" ] || continue; ",
    "  printf '%s\\n' \"$(basename \"$f\" .json)\"; ",
    "  cat \"$f\"; ",
    "  printf '\\n---ADE-STATUS-END---\\n'; ",
    "done; ",
    "echo '---ADE-HOOKS---'; ",
    "if grep -q ade-status-marker \"$HOME\"/.claude/settings.json 2>/dev/null; then echo OK; else echo MISSING; fi"
);

const SSH_OPTS: &[&str] = &[
    "-o",
    "ConnectTimeout=5",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

pub fn run(host_filter: Option<&str>) -> Result<(), String> {
    let (config, warning) = Config::load();
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }

    match host_filter {
        None => {
            print_local();
            for host in &config.hosts {
                println!();
                print_remote(host);
            }
        }
        Some("local") => print_local(),
        Some(name) => {
            let host = config
                .host_by_name(name)
                .ok_or_else(|| format!("host '{}' not found in config", name))?;
            print_remote(host);
        }
    }
    Ok(())
}

fn print_local() {
    println!("=== local ===");
    let panes_text = capture("tmux", &["list-panes", "-a", "-F", PANE_FORMAT]);
    let ps_text = capture("ps", &["-A", "-o", "pid,ppid,comm"]);
    let statuses = claude_status::read_local_statuses();
    let hooks_installed = install_hooks::is_installed_local();
    print_section(&panes_text, &ps_text, &statuses, Some(hooks_installed));
}

fn print_remote(host: &Host) {
    let kind = match host.kind {
        HostKind::Ssh => "ssh",
        HostKind::Mosh => "mosh",
    };
    println!("=== {} ({} via {}) ===", host.name, host.target, kind);

    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS);
    for a in &host.ssh_args {
        cmd.arg(a);
    }
    cmd.arg(&host.target);
    cmd.arg(REMOTE_DEBUG_CMD);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            println!("  ssh failed to launch: {}", e);
            return;
        }
    };

    if !output.status.success() && output.status.code() == Some(255) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr.lines().next().unwrap_or("unreachable");
        println!("  unreachable: {}", line);
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (panes_part, rest) = stdout.split_once("---ADE-PS---").unwrap_or((stdout.as_ref(), ""));
    let (ps_part, rest) = rest.split_once("---ADE-STATUS---").unwrap_or((rest, ""));
    let (status_part, hooks_part) = rest.split_once("---ADE-HOOKS---").unwrap_or((rest, ""));

    let statuses = claude_status::parse_remote_statuses(status_part);
    let hooks_installed = match hooks_part.trim() {
        "OK" => Some(true),
        "MISSING" => Some(false),
        _ => None,
    };
    print_section(panes_part, ps_part, &statuses, hooks_installed);
}

fn print_section(
    panes_text: &str,
    ps_text: &str,
    statuses: &HashMap<String, ClaudeState>,
    hooks_installed: Option<bool>,
) {
    let panes: Vec<(String, String, String, u32)> = panes_text
        .lines()
        .filter_map(parse_pane_line)
        .collect();

    if panes.is_empty() {
        println!("  (no tmux panes)");
        print_hooks_footer(hooks_installed);
        return;
    }

    let pane_pids: Vec<u32> = panes.iter().map(|p| p.3).collect();
    let claude_pane_pids = claude_status::find_claude_pane_pids(&pane_pids, ps_text);
    let descendants_by_root = build_descendants(&pane_pids, ps_text);

    println!(
        "  {:<22} {:>6} {:>7} {:<28} {:<22} {}",
        "session", "pane", "pid", "claude descendants", "status file", "decision"
    );
    for (session, cmd, pane_id, pane_pid) in &panes {
        let claude_descs: Vec<String> = descendants_by_root
            .get(pane_pid)
            .map(|kids| {
                kids.iter()
                    .filter(|(_, comm)| comm == "claude")
                    .map(|(pid, _)| format!("{}", pid))
                    .collect()
            })
            .unwrap_or_default();
        let claude_str = if claude_descs.is_empty() {
            "-".to_string()
        } else {
            claude_descs.join(",")
        };

        let status_str = match statuses.get(pane_id) {
            Some(ClaudeState::Working) => format!("{}.json (working)", pane_id),
            Some(ClaudeState::Idle) => format!("{}.json (idle)", pane_id),
            None => "-".to_string(),
        };

        let is_claude = cmd == "claude" || claude_pane_pids.contains(pane_pid);
        let decision = if is_claude {
            let state = statuses.get(pane_id).copied().unwrap_or(ClaudeState::Idle);
            match state {
                ClaudeState::Working => "claude=working",
                ClaudeState::Idle => "claude=idle",
            }
        } else {
            "no claude"
        };

        println!(
            "  {:<22} {:>6} {:>7} {:<28} {:<22} {}",
            truncate(&format!("{} [{}]", session, cmd), 22),
            pane_id,
            pane_pid,
            truncate(&claude_str, 28),
            truncate(&status_str, 22),
            decision,
        );
    }

    print_hooks_footer(hooks_installed);
}

fn print_hooks_footer(hooks_installed: Option<bool>) {
    match hooks_installed {
        Some(true) => println!("  hooks: installed"),
        Some(false) => println!("  hooks: MISSING (run `ade install-hooks` here)"),
        None => println!("  hooks: unknown"),
    }
}

/// For each pane root pid, collect every descendant as `(pid, comm)`. Used
/// for the "claude descendants" column in the diagnostic table.
fn build_descendants(roots: &[u32], ps_text: &str) -> HashMap<u32, Vec<(u32, String)>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut comm_by_pid: HashMap<u32, String> = HashMap::new();

    for line in ps_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let comm = parts.next().unwrap_or("").to_string();
        comm_by_pid.insert(pid, comm);
        children.entry(ppid).or_default().push(pid);
    }

    let mut out: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for &root in roots {
        let mut collected: Vec<(u32, String)> = Vec::new();
        let mut stack = vec![root];
        let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur) {
                continue;
            }
            if let Some(comm) = comm_by_pid.get(&cur) {
                collected.push((cur, comm.clone()));
            }
            if let Some(kids) = children.get(&cur) {
                stack.extend_from_slice(kids);
            }
        }
        out.insert(root, collected);
    }
    out
}

fn capture(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
