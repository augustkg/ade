#[cfg(test)]
mod acceptance;
mod app;
mod claude_status;
mod cwd;
mod debug;
mod duplicate_log;
mod embedded_term;
mod hosts;
mod install_hooks;
mod install_tmux;
mod json_out;
mod kanban;
mod mail;
mod mail_delivery;
mod model;
mod notifications;
mod peek;
mod preview_pane;
mod prompt_snapshot;
mod refresh;
mod spawn;
mod ssh_io;
mod state;
mod term_title;
#[cfg(test)]
mod test_harness;
mod text_field;
mod theme;
mod tmux;
mod tui_lifecycle;
mod ui;

use std::process::Command;
use std::time::Duration;

use app::{App, AppAction};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyEventKind};
use hosts::{Config, Host, HostKind};
use model::Machine;
use ratatui::DefaultTerminal;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();

    // CLI subcommands take precedence over the TUI.
    if argv.len() >= 2 {
        match argv[1].as_str() {
            "install-hooks" => return run_install_hooks(&argv[2..]),
            "install-tmux-config" => return run_install_tmux(&argv[2..]),
            "debug" => return run_debug(&argv[2..]),
            "kanban" => return run_kanban(&argv[2..]),
            "mail" => return run_mail(&argv[2..]),
            "session" => return run_session(&argv[2..]),
            // Non-interactive JSON inventory / monitor summary. Both diverge
            // (print to stdout and `std::process::exit`) — they never start
            // the TUI and never return.
            "sessions" => json_out::run_sessions(&argv[2..]),
            "status" => json_out::run_status(&argv[2..]),
            // Local-only: what is a session asking + a transcript recap, for
            // the HUD. Diverges (prints + exits) like sessions/status.
            "peek" => peek::run_peek(&argv[2..]),
            "--help" | "-h" | "help" => {
                print_usage();
                return Ok(());
            }
            _ => {}
        }
    }

    color_eyre::install()?;

    let mut terminal = ratatui::init();
    // Mouse capture is *not* enabled here. It's scoped to the
    // duration of an embedded session via `EmbeddedTerm`'s
    // `MouseCaptureGuard` — enabling it globally would swallow
    // the user's normal terminal scroll / Cmd+drag selection
    // while they're just browsing the tree.
    //
    // No `Config::load()` here — `App::new()` loads it (and the Hosts
    // screen mutates `app.config` in place + persists). Loading a
    // separate snapshot here would go stale the moment the user adds
    // or edits a host mid-session, and the next attach to that host
    // would resolve against the old config.
    let result = run_loop(&mut terminal);
    ratatui::restore();
    term_title::clear();

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
    Ok(())
}

fn print_usage() {
    println!(
        "ade — Agentic Development Environment\n\
         \n\
         Usage:\n\
         \x20\x20ade                                    Launch the TUI\n\
         \x20\x20ade install-hooks [--host H]           Install Claude Code status hooks (local or remote)\n\
         \x20\x20ade install-hooks --all                Install hooks on local + every host in hosts.toml\n\
         \x20\x20ade install-tmux-config [--host H]     Install tmux clipboard config (local or one remote)\n\
         \x20\x20ade install-tmux-config --all          Install on local + every host in hosts.toml\n\
         \x20\x20ade install-tmux-config --uninstall    Remove the tmux clipboard config (use --all for everywhere)\n\
         \x20\x20ade debug claude [--host H]            Diagnose why ADE does/doesn't see Claude per pane\n\
         \x20\x20ade kanban move <column> [--session S] Move a session's kanban card to a manual column\n\
         \x20\x20ade kanban clear [--session S]         Return a session's card to the automatic columns\n\
         \x20\x20ade mail send --session S --body TEXT   Leave a message for another local session (routed by ADE)\n\
         \x20\x20ade mail whoami                         Print this tmux session's name (its mail address)\n\
         \x20\x20ade mail list                           Show this session's pending messages\n\
         \x20\x20ade mail deliver [--session S] [--dry-run]  Deliver queued mail without the TUI (headless router)\n\
         \x20\x20ade session new --name N [--cwd D] [--prompt TEXT]  Spin up a new workstream session\n\
         \x20\x20ade sessions --json [--local]          Print the full cross-host session inventory as JSON\n\
         \x20\x20ade status --json [--local]            Print a Claude monitor summary as JSON\n\
         \x20\x20ade peek <session> --json              Print what a local session is asking + a transcript recap\n\
         \x20\x20ade help                               Show this message\n\
         \n\
         `ade kanban` defaults to the tmux session it runs inside — so a Claude Code\n\
         session can mark itself done. A running ADE picks the change up within ~2s;\n\
         otherwise it applies at the next launch. Local sessions only."
    );
}

/// `ade kanban move <column> [--session S]` / `ade kanban clear [--session S]`.
/// Publishes an intent file for the running (or next) ADE instance to
/// consume — never writes state.toml directly, the App owns that file.
fn run_kanban(args: &[String]) -> Result<()> {
    fn fail(msg: &str) -> ! {
        eprintln!("Error: {}", msg);
        std::process::exit(2);
    }
    fn parse_session(args: &[String], mut i: usize) -> Option<String> {
        let mut session = None;
        while i < args.len() {
            match args[i].as_str() {
                "--session" => {
                    if i + 1 >= args.len() {
                        fail("--session requires a value");
                    }
                    session = Some(args[i + 1].clone());
                    i += 2;
                }
                other => fail(&format!("unknown argument '{}'", other)),
            }
        }
        session
    }
    /// The tmux session this process runs inside, via `display-message`.
    fn current_session() -> Option<String> {
        std::env::var_os("TMUX")?;
        let out = std::process::Command::new("tmux")
            .args(["display-message", "-p", "#{session_name}"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
    fn session_exists(name: &str) -> bool {
        std::process::Command::new("tmux")
            .args(["has-session", "-t", &format!("={}", name)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    fn resolve_session(explicit: Option<String>) -> String {
        let session = explicit.or_else(current_session).unwrap_or_else(|| {
            fail(
                "not inside a tmux session — pass --session <name> \
                 (this command targets local tmux sessions)",
            )
        });
        if !session_exists(&session) {
            fail(&format!("no local tmux session named '{}'", session));
        }
        session
    }

    if args.is_empty() {
        fail("`ade kanban` requires a subcommand: move <column> | clear");
    }
    match args[0].as_str() {
        "move" => {
            if args.len() < 2 || args[1].starts_with("--") {
                fail("usage: ade kanban move <column> [--session S]");
            }
            // Strict config: refuse to resolve against silently-defaulted
            // columns when the user's kanban.toml is broken.
            let cfg = match kanban::KanbanConfig::load_result() {
                Ok(c) => c,
                Err(e) => fail(&e),
            };
            let column = match cfg.resolve_manual_column(&args[1]) {
                Ok(c) => c.clone(),
                Err(e) => fail(&e),
            };
            let session = resolve_session(parse_session(args, 2));
            let intent = kanban::Intent {
                host: "local".to_string(),
                session: session.clone(),
                column: Some(column.id.clone()),
            };
            if let Err(e) = kanban::write_intent(&intent) {
                fail(&e);
            }
            println!(
                "kanban: '{}' → {}. A running ADE applies this within ~2s \
                 (otherwise at next launch).",
                session, column.name
            );
            println!(
                "note: if Claude is actively working in the session, the move is \
                 held and applied the moment the work stops."
            );
        }
        "clear" => {
            let session = resolve_session(parse_session(args, 1));
            let intent = kanban::Intent {
                host: "local".to_string(),
                session: session.clone(),
                column: None,
            };
            if let Err(e) = kanban::write_intent(&intent) {
                fail(&e);
            }
            println!(
                "kanban: '{}' returned to automatic columns. A running ADE \
                 applies this within ~2s (otherwise at next launch).",
                session
            );
            println!(
                "note: if Claude is actively working in the session, the change \
                 is held and applied the moment the work stops."
            );
        }
        other => fail(&format!(
            "unknown kanban subcommand '{}': use move <column> | clear",
            other
        )),
    }
    Ok(())
}

/// `ade mail send --session <to> --body <text>` / `ade mail whoami` /
/// `ade mail list`. Publishes a message file for the running (or next) ADE
/// instance to route — never injects into another pane itself, the App owns
/// that side effect. Local sessions only (P1).
fn run_mail(args: &[String]) -> Result<()> {
    fn fail(msg: &str) -> ! {
        eprintln!("Error: {}", msg);
        std::process::exit(2);
    }
    /// The tmux session this process runs inside.
    fn current_session() -> Option<String> {
        std::env::var_os("TMUX")?;
        let out = std::process::Command::new("tmux")
            .args(["display-message", "-p", "#{session_name}"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!name.is_empty()).then_some(name)
    }
    /// The tmux `#{session_id}` (e.g. `$3`) this process runs inside.
    fn current_session_id() -> Option<String> {
        std::env::var_os("TMUX")?;
        let out = std::process::Command::new("tmux")
            .args(["display-message", "-p", "#{session_id}"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!id.is_empty()).then_some(id)
    }
    fn session_exists(name: &str) -> bool {
        std::process::Command::new("tmux")
            .args(["has-session", "-t", &format!("={}", name)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    if args.is_empty() {
        fail("`ade mail` requires a subcommand: send | whoami | list");
    }
    match args[0].as_str() {
        "send" => {
            let mut to: Option<String> = None;
            let mut body: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" | "--to" => {
                        if i + 1 >= args.len() {
                            fail(&format!("{} requires a value", args[i]));
                        }
                        to = Some(args[i + 1].clone());
                        i += 2;
                    }
                    "--body" => {
                        if i + 1 >= args.len() {
                            fail("--body requires a value");
                        }
                        body = Some(args[i + 1].clone());
                        i += 2;
                    }
                    other => fail(&format!("unknown argument '{}'", other)),
                }
            }
            let to = to.unwrap_or_else(|| {
                fail("usage: ade mail send --session <name> --body <text>")
            });
            let body = body.unwrap_or_else(|| {
                fail("usage: ade mail send --session <name> --body <text>")
            });
            if let Err(e) = mail::validate_body(&body) {
                fail(&e);
            }
            let from = current_session().unwrap_or_else(|| {
                fail(
                    "not inside a tmux session — `ade mail send` must run \
                     from the sending session (local sessions only)",
                )
            });
            let from_id = current_session_id().unwrap_or_default();
            if to == from {
                fail("refusing to send a message to yourself");
            }
            if !session_exists(&to) {
                fail(&format!("no local tmux session named '{}'", to));
            }
            // Capture the recipient's *current* address (server pid +
            // session id) so the router can refuse delivery if that session is
            // later killed and recreated under the same name — or recreated
            // after a tmux server restart, which resets session ids.
            let to_addr = match tmux::local_session_addr(&to) {
                Some(a) => a,
                None => fail(&format!(
                    "could not resolve the address of local session '{}'",
                    to
                )),
            };
            match mail::write_message(&from, &from_id, &to, &to_addr, &body) {
                Ok(id) => {
                    println!(
                        "mail: queued for '{}' (id {}). A running ADE surfaces it \
                         within ~2s; delivery into the session happens when you \
                         trigger it from ADE.",
                        to, id
                    );
                }
                Err(e) => fail(&e),
            }
        }
        "whoami" => match current_session() {
            Some(name) => println!("{}", name),
            None => fail("not inside a tmux session"),
        },
        "list" => {
            let me = current_session().unwrap_or_else(|| {
                fail("not inside a tmux session — `ade mail list` shows this session's inbox")
            });
            let inbox = mail::read_inbox();
            let mine: Vec<_> = inbox.iter().filter(|(_, m)| m.to_session == me).collect();
            if mine.is_empty() {
                println!("mail: no pending messages for '{}'.", me);
            } else {
                println!("mail: {} pending message(s) for '{}':", mine.len(), me);
                for (_, m) in mine {
                    println!("  from {}: {}", m.from_session, m.body);
                }
            }
        }
        "deliver" => {
            let mut only: Option<String> = None;
            let mut dry_run = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" | "--to" => {
                        if i + 1 >= args.len() {
                            fail(&format!("{} requires a value", args[i]));
                        }
                        only = Some(args[i + 1].clone());
                        i += 2;
                    }
                    "--dry-run" => {
                        dry_run = true;
                        i += 1;
                    }
                    other => fail(&format!("unknown argument '{}'", other)),
                }
            }
            let delivered = run_mail_deliver(only.as_deref(), dry_run);
            if delivered == 0 {
                std::process::exit(1);
            }
        }
        other => fail(&format!(
            "unknown mail subcommand '{}': use send | whoami | list | deliver",
            other
        )),
    }
    Ok(())
}

/// Headless delivery: the same gate + claim + send + archive machinery the TUI
/// runs on `m`, driven from the CLI so a machine can route its own mail without
/// anybody sitting in front of a terminal.
///
/// This is the seam a router daemon grows out of — the policy lives in
/// `mail_delivery`, so the TUI and this path cannot drift apart. Delivers **at
/// most one message per recipient** per invocation (matching the TUI), so a
/// burst never lands as a wall of text in someone's prompt.
///
/// Returns how many messages were delivered. Refusals are printed with their
/// reason and are not failures — "recipient is mid-turn" is the system working.
fn run_mail_deliver(only: Option<&str>, dry_run: bool) -> usize {
    use mail_delivery::{DeliveryRequest, Gate, Outcome};
    use tmux::TmuxBackend;

    let Some(dir) = mail::mail_dir() else {
        eprintln!("Error: no $HOME / $XDG_CACHE_HOME — cannot locate the mailbox");
        std::process::exit(1);
    };

    let pending = mail::read_inbox();
    if pending.is_empty() {
        println!("mail: nothing pending.");
        return 0;
    }

    // One local tmux snapshot for every recipient we consider.
    let sessions = match tmux::local().list_sessions() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: could not list local tmux sessions: {}", e);
            std::process::exit(1);
        }
    };

    let env = app::TmuxDelivery;
    let mut delivered = 0usize;
    let mut seen: Vec<String> = Vec::new();

    for (path, msg) in pending {
        if let Some(want) = only {
            if msg.to_session != want {
                continue;
            }
        }
        // At most one per recipient per run; the inbox is in publish order, so
        // this naturally picks the oldest.
        if seen.iter().any(|s| s == &msg.to_session) {
            continue;
        }
        seen.push(msg.to_session.clone());

        let Some(session) = sessions.iter().find(|s| s.name == msg.to_session) else {
            println!("{}: no such local session — skipped", msg.to_session);
            continue;
        };

        let req = DeliveryRequest {
            session_name: &msg.to_session,
            is_local: true,
            has_pending_mail: true,
            claude_present: session.claude_present,
            claude: session.claude,
        };

        let pane = match mail_delivery::gate(&env, &req, &msg.to_session_addr) {
            Gate::Deliver { pane_id } => pane_id,
            Gate::Refuse(reason) => {
                println!("{}: held — {}", msg.to_session, reason);
                continue;
            }
        };

        if dry_run {
            println!(
                "{}: WOULD deliver from '{}' into pane {}",
                msg.to_session, msg.from_session, pane
            );
            continue;
        }

        let claimed = match mail::claim(&dir, &path) {
            Ok(c) => c,
            Err(e) => {
                println!("{}: could not claim — {}", msg.to_session, e);
                continue;
            }
        };
        match mail_delivery::execute(&env, &dir, &claimed, &msg, &pane) {
            Outcome::Delivered => {
                println!("{}: delivered from '{}'", msg.to_session, msg.from_session);
                delivered += 1;
            }
            Outcome::DeliveredButNotArchived(e) => {
                println!(
                    "{}: delivered from '{}', but archiving failed: {}",
                    msg.to_session, msg.from_session, e
                );
                delivered += 1;
            }
            Outcome::Ambiguous(e) => {
                println!(
                    "{}: UNCERTAIN — {}. Not retried; held for recovery.",
                    msg.to_session, e
                );
            }
            Outcome::Requeued(e) => println!("{}: failed, requeued — {}", msg.to_session, e),
            Outcome::RequeueFailed { send, requeue } => println!(
                "{}: failed ({}) and requeue failed ({})",
                msg.to_session, send, requeue
            ),
        }
    }
    delivered
}

/// `ade session new --name N [--cwd D] [--prompt TEXT] [--no-claude]`.
///
/// Lets an orchestrator session create a new workstream. Two design choices
/// worth knowing:
///
///  * **The initial prompt is delivered as mail, not typed here.** A freshly
///    spawned Claude takes seconds to boot, so typing immediately would land in
///    a terminal that isn't listening. Queuing it means the existing router
///    delivers it exactly when the new session is idle with an empty composer,
///    reusing every gate rather than inventing a second, weaker path.
///  * **Lineage is stamped on the tmux session**, so depth and quota survive
///    ADE restarts and are cleaned up by `tmux kill-session`.
fn run_session(args: &[String]) -> Result<()> {
    fn fail(msg: &str) -> ! {
        eprintln!("Error: {}", msg);
        std::process::exit(2);
    }
    if args.is_empty() || args[0] != "new" {
        fail("`ade session` requires a subcommand: new");
    }

    let mut name: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut run_claude = true;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                if i + 1 >= args.len() { fail("--name requires a value"); }
                name = Some(args[i + 1].clone());
                i += 2;
            }
            "--cwd" => {
                if i + 1 >= args.len() { fail("--cwd requires a value"); }
                cwd = Some(args[i + 1].clone());
                i += 2;
            }
            "--prompt" => {
                if i + 1 >= args.len() { fail("--prompt requires a value"); }
                prompt = Some(args[i + 1].clone());
                i += 2;
            }
            "--no-claude" => { run_claude = false; i += 1; }
            other => fail(&format!("unknown argument '{}'", other)),
        }
    }

    let name = name.unwrap_or_else(|| {
        fail("usage: ade session new --name <name> [--cwd D] [--prompt TEXT]")
    });
    // Validate the name before touching the environment: a bad name is the
    // most useful error to return, and it must not be masked by "you are not
    // inside tmux".
    if let Err(e) = spawn::validate_session_name(&name) {
        fail(&e);
    }
    // A prompt must survive being typed into a prompt line, same rules as mail.
    if let Some(p) = &prompt {
        if let Err(e) = mail::validate_body(p) {
            fail(&format!("--prompt rejected: {}", e));
        }
    }

    // The caller is the parent of the chain. Outside tmux there is no parent,
    // which also means no depth accounting — refuse rather than silently
    // rooting a new chain from nowhere.
    let parent = tmux::current_session().unwrap_or_else(|| {
        fail("not inside a tmux session — `ade session new` records the calling \
              session as the new one's parent")
    });

    let inventory = match tmux::local::spawn_inventory() {
        Ok(inv) => inv,
        Err(e) => fail(&e),
    };
    let caller_depth = inventory
        .iter()
        .find(|r| r.name == parent)
        .map(|r| r.depth)
        .unwrap_or(0);
    let depth = match spawn::authorize_spawn(caller_depth, &inventory, &name) {
        Ok(d) => d,
        Err(e) => fail(&e),
    };

    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });

    if let Err(e) = tmux::local::spawn_session(&name, &cwd, run_claude, &parent, depth) {
        fail(&e);
    }
    println!("session: created '{}' in {} (depth {})", name, cwd, depth);

    let Some(prompt) = prompt else {
        return Ok(());
    };

    // Queue the opening instruction. The recipient address must be read AFTER
    // creation, and delivery waits for the new Claude to actually be idle.
    let Some(to_addr) = tmux::local_session_addr(&name) else {
        eprintln!(
            "Warning: created '{}' but could not resolve its address; \
             re-send the prompt with `ade mail send` once it is up.",
            name
        );
        return Ok(());
    };
    let from_id = tmux::local_session_addr(&parent).unwrap_or_default();
    match mail::write_message(&parent, &from_id, &name, &to_addr, &prompt) {
        Ok(id) => println!(
            "session: opening prompt queued for '{}' (id {}) — it is delivered \
             once that session is idle at its prompt.",
            name, id
        ),
        Err(e) => eprintln!("Warning: session created but prompt not queued: {}", e),
    }
    Ok(())
}

fn run_debug(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("Error: `ade debug` requires a subcommand. Try `ade debug claude`.");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "claude" => {
            let mut host: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--host" => {
                        i += 1;
                        if i >= args.len() {
                            eprintln!("Error: --host requires a value");
                            std::process::exit(2);
                        }
                        host = Some(args[i].clone());
                    }
                    other => {
                        eprintln!("Error: unknown argument '{}'", other);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }
            match debug::run(host.as_deref()) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("Error: unknown debug subcommand '{}'", other);
            std::process::exit(2);
        }
    }
}

fn run_install_hooks(args: &[String]) -> Result<()> {
    let mut host: Option<String> = None;
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --host requires a value");
                    std::process::exit(2);
                }
                host = Some(args[i].clone());
            }
            "--all" => all = true,
            other => {
                eprintln!("Error: unknown argument '{}'", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if all && host.is_some() {
        eprintln!("Error: --all and --host are mutually exclusive");
        std::process::exit(2);
    }

    if all {
        return run_install_hooks_all();
    }

    let result = match host {
        None => install_hooks::install_local(),
        Some(name) => {
            let (config, _warning) = Config::load();
            install_hooks::install_remote(&config, &name)
        }
    };

    match result {
        Ok(msg) => {
            println!("{}", msg);
            Ok(())
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run `install_hooks` across local + every host in `hosts.toml`,
/// continuing past per-host failures so the user sees a complete
/// summary in one shot. Exit code reflects worst-case: 0 only if every
/// step succeeded, 1 otherwise. Mirrors `run_install_tmux_all`.
///
/// If `hosts.toml` exists but doesn't parse, `Config::load()` returns
/// an empty default plus a warning — that would silently skip every
/// remote and exit 0, which is the opposite of what `--all` advertises.
/// Refuse to proceed in that case.
fn run_install_hooks_all() -> Result<()> {
    let (config, parse_warning) = Config::load();
    if let Some(w) = parse_warning {
        eprintln!("Error: cannot --all: hosts.toml failed to parse: {}", w);
        std::process::exit(1);
    }

    let mut any_failed = false;

    match install_hooks::install_local() {
        Ok(msg) => println!("{}", msg),
        Err(e) => {
            println!("local: error: {}", e);
            any_failed = true;
        }
    }

    if config.hosts.is_empty() {
        println!("(no remote hosts configured in ~/.config/ade/hosts.toml)");
    } else {
        for host in &config.hosts {
            match install_hooks::install_remote(&config, &host.name) {
                Ok(msg) => println!("{}", msg),
                Err(e) => {
                    println!("{}: error: {}", host.name, e);
                    any_failed = true;
                }
            }
        }
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

fn run_install_tmux(args: &[String]) -> Result<()> {
    let mut host: Option<String> = None;
    let mut uninstall = false;
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --host requires a value");
                    std::process::exit(2);
                }
                host = Some(args[i].clone());
            }
            "--uninstall" => uninstall = true,
            "--all" => all = true,
            other => {
                eprintln!("Error: unknown argument '{}'", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if all && host.is_some() {
        eprintln!("Error: --all and --host are mutually exclusive");
        std::process::exit(2);
    }

    if uninstall {
        return run_uninstall_tmux(host, all);
    }

    if all {
        return run_install_tmux_all();
    }

    // Single-host (or local) install. We extract `reload_failed`
    // before consuming the report into the message so the exit code
    // reflects reload failures the same way `--all` does — otherwise
    // CI / scripts would see exit 0 despite "reload failed: …" in
    // the printed output.
    let outcome = match host {
        None => install_tmux::install_local().map(|r| {
            let reload_failed = matches!(r.reload, install_tmux::ReloadStatus::Failed(_));
            (format_local_install_msg(r), reload_failed)
        }),
        Some(name) => {
            let (config, _warning) = Config::load();
            install_tmux::install_remote(&config, &name).map(|r| {
                let reload_failed = matches!(r.reload, install_tmux::ReloadStatus::Failed(_));
                (format!("{}: {}", name, r.summary()), reload_failed)
            })
        }
    };

    match outcome {
        Ok((msg, reload_failed)) => {
            println!("{}", msg);
            if reload_failed {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run `install_tmux_config` across local + every host in `hosts.toml`,
/// continuing past per-host failures so the user sees a complete
/// summary in one shot. Exit code reflects worst-case: 0 only if every
/// step succeeded, 1 otherwise.
///
/// If `hosts.toml` exists but doesn't parse, `Config::load()` returns
/// an empty default plus a warning — that would silently skip every
/// remote and exit 0, which is the opposite of what `--all` advertises.
/// Refuse to proceed in that case.
fn run_install_tmux_all() -> Result<()> {
    let (config, parse_warning) = Config::load();
    if let Some(w) = parse_warning {
        eprintln!("Error: cannot --all: hosts.toml failed to parse: {}", w);
        std::process::exit(1);
    }

    let mut any_failed = false;

    match install_tmux::install_local() {
        Ok(report) => {
            if matches!(report.reload, install_tmux::ReloadStatus::Failed(_)) {
                any_failed = true;
            }
            println!("{}", format_local_install_msg(report));
        }
        Err(e) => {
            println!("local: error: {}", e);
            any_failed = true;
        }
    }

    if config.hosts.is_empty() {
        println!("(no remote hosts configured in ~/.config/ade/hosts.toml)");
    } else {
        for host in &config.hosts {
            match install_tmux::install_remote(&config, &host.name) {
                Ok(report) => {
                    if matches!(report.reload, install_tmux::ReloadStatus::Failed(_)) {
                        any_failed = true;
                    }
                    println!("{}: {}", host.name, report.summary());
                }
                Err(e) => {
                    println!("{}: error: {}", host.name, e);
                    any_failed = true;
                }
            }
        }
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

fn run_uninstall_tmux(host: Option<String>, all: bool) -> Result<()> {
    if all {
        let (config, parse_warning) = Config::load();
        if let Some(w) = parse_warning {
            eprintln!("Error: cannot --all: hosts.toml failed to parse: {}", w);
            std::process::exit(1);
        }
        let mut any_failed = false;

        match install_tmux::uninstall_local() {
            Ok(report) => println!("local: {}", report.summary()),
            Err(e) => {
                println!("local: error: {}", e);
                any_failed = true;
            }
        }
        for h in &config.hosts {
            match install_tmux::uninstall_remote(&config, &h.name) {
                Ok(report) => println!("{}: {}", h.name, report.summary()),
                Err(e) => {
                    println!("{}: error: {}", h.name, e);
                    any_failed = true;
                }
            }
        }
        if any_failed {
            std::process::exit(1);
        }
        return Ok(());
    }

    let result = match host {
        None => install_tmux::uninstall_local()
            .map(|r| ("local".to_string(), r.summary())),
        Some(name) => {
            let (config, _warning) = Config::load();
            install_tmux::uninstall_remote(&config, &name)
                .map(|r| (name.clone(), r.summary()))
        }
    };
    match result {
        Ok((target, msg)) => {
            println!("{}: {}", target, msg);
            Ok(())
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn format_local_install_msg(r: install_tmux::InstallReport) -> String {
    let mut msg = format!("local: {}", r.summary());
    if r.mouse_off {
        msg.push_str(
            "\nWarning: detected `mouse off` in your tmux config. \
             ADE's clipboard config requires `mouse on` for drag-select-to-copy. \
             Remove or update that line, then reload tmux.",
        );
    }
    msg
}

/// What to do when the user picks a session from the TUI. Resolved per
/// attach attempt — depends on machine + whether ADE is running inside
/// tmux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachOutcome {
    /// Local + inside tmux: hand off via `tmux switch-client`. ADE keeps
    /// running in its original pane; the user returns via `prefix L`
    /// (last-session) — see the `prefix B` if-shell in `MANAGED_BODY`.
    SwitchClient,
    /// Local-outside-tmux or any remote attach: ADE spawns the attach
    /// command and waits for it to exit. The TUI suspends + resumes
    /// around the wait.
    SpawnAndWait,
    /// User picked the session they're already in (only reachable from
    /// inside-tmux). Nothing to do — keep the picker on screen.
    SameSessionNoOp,
}

fn attach_outcome(name: &str, machine: &Machine) -> AttachOutcome {
    let inside = tmux::is_inside_tmux();
    match machine {
        Machine::Local => {
            if !inside {
                return AttachOutcome::SpawnAndWait;
            }
            if matches!(tmux::current_session().as_deref(), Some(c) if c == name) {
                AttachOutcome::SameSessionNoOp
            } else {
                AttachOutcome::SwitchClient
            }
        }
        // Remote: always spawn-and-wait. From inside tmux, ADE's pane
        // hosts the ssh/mosh child for the duration; from outside, the
        // tab does. Either way `prefix B` → detach → child exits → ADE
        // resumes.
        Machine::Remote(_) => AttachOutcome::SpawnAndWait,
    }
}

fn log_attach_intent(name: &str, machine: &Machine, outcome: AttachOutcome) {
    let env_tmux = std::env::var("TMUX").unwrap_or_default();
    let env_tmux_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let current = tmux::current_session().unwrap_or_default();
    let log = format!(
        "{}\n\
         attach: name={} machine={:?} outcome={:?}\n\
         env: TMUX={:?} TMUX_PANE={:?}\n\
         current_session: {:?}\n",
        chrono_now(),
        name,
        machine,
        outcome,
        env_tmux,
        env_tmux_pane,
        current,
    );
    let _ = std::fs::write("/tmp/ade-attach.log", log);
}

/// Spawn the attach command and block until it exits. Caller is
/// responsible for suspending/resuming the TUI around this call.
fn spawn_and_wait_attach(
    name: &str,
    machine: &Machine,
    config: &Config,
) -> Result<(), String> {
    let target = format!("={}", name);
    let (program, args) = match machine {
        Machine::Local => (
            "tmux".to_string(),
            vec![
                "attach-session".to_string(),
                "-t".to_string(),
                target.clone(),
            ],
        ),
        Machine::Remote(host_name) => {
            let host = config
                .host_by_name(host_name)
                .ok_or_else(|| format!("host '{}' not found in config", host_name))?;
            build_attach_command(host, &target, true)
        }
    };

    let mut cmd = Command::new(&program);
    cmd.args(&args);

    #[cfg(unix)]
    {
        // SAFETY: pre_exec runs after fork in the child, before exec, in a
        // single-threaded address space. Only async-signal-safe operations
        // are permitted; `signal(2)` qualifies. Without this the child
        // inherits ADE's SIG_IGN dispositions (installed by
        // `tui_lifecycle::suspend`) and Ctrl+C / Ctrl+Z stop working in
        // remote shells.
        unsafe {
            cmd.pre_exec(tui_lifecycle::child_restore_default_signals);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {}", program, e))?;
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for {}: {}", program, e))?;

    append_attach_log(&format!(
        "spawn_and_wait: {} {} → status={}\n",
        program,
        args.join(" "),
        status,
    ));

    if status.success() {
        Ok(())
    } else {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        Err(format!("{} exited with status {}", program, code))
    }
}

/// Plant `@ade-title` on the target tmux session so the managed
/// `set-titles-string` resolves to ADE's `folder/session | host` format
/// after attach. Best-effort — failures are swallowed (the worst case is a
/// stale or generic terminal title; not worth aborting attach for).
///
/// `set-option -t` resolves its argument as a target-pane, which makes the
/// `=name` exact-match prefix that the rest of ADE uses error out with
/// "no such session". Bare prefix-match would also misfire when one session
/// name is a prefix of another (e.g. `work` and `work/web`). So we enumerate
/// `list-sessions` ourselves, exact-match in Rust, and pass the resolved
/// `$id` to `set-option`.
fn set_session_title_option(name: &str, machine: &Machine) {
    let Some(session_id) = lookup_session_id(name) else { return };
    let title = term_title::for_session_name(name, machine.title_label());
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", &session_id, "@ade-title", &title])
        .output();
}

/// Plant or clear `@ade-parent` on the target tmux session. The managed
/// `prefix B` keybinding (see `install_tmux::MANAGED_BODY`) routes to
/// `detach-client` when the option is truthy and `switch-client -l`
/// otherwise — so the marker must be set before each spawn-and-wait
/// attach and unset after, to avoid stale state confusing future direct
/// (non-ADE) attaches to the same session.
///
/// Kept separate from `set_session_title_option` on purpose: the title
/// helper is also called from the local `switch-client` path (where ADE
/// is *not* the parent), and bundling the two would mark switch-client
/// targets too — defeating the if-shell branch.
///
/// Best-effort. Local-only — remote planting/clearing is performed
/// inline by the remote shell wrapper in `remote_attach_cmd`. Failure
/// modes that leave the marker stale on a session: ADE itself killed
/// with SIGKILL (skips the local unset call); remote shell killed with
/// SIGKILL or a sufficiently abrupt connection loss (skips the `trap …
/// EXIT`). The recovery is `tmux set-option -t SESSION -u @ade-parent`.
///
/// There's also a known race when two ADE processes attach the same
/// session concurrently as parents: the first to detach unsets the
/// marker while the second is still attached, so the second's `prefix
/// B` falls through to `switch-client -l` instead of detaching. v1
/// accepts this — running two ADEs on the same session at once is
/// already an unusual setup.
fn set_session_parent_marker(name: &str, machine: &Machine, set: bool) {
    if !matches!(machine, Machine::Local) {
        return;
    }
    let Some(session_id) = lookup_session_id(name) else { return };
    let mut cmd = std::process::Command::new("tmux");
    if set {
        cmd.args(["set-option", "-t", &session_id, "@ade-parent", "1"]);
    } else {
        cmd.args(["set-option", "-t", &session_id, "-u", "@ade-parent"]);
    }
    let _ = cmd.output();
}

fn lookup_session_id(name: &str) -> Option<String> {
    let out = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}\t#{session_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some((n, id)) = line.split_once('\t') {
            if n == name {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("epoch={}", d.as_secs()))
        .unwrap_or_default()
}

/// The remote command to run on the destination host. Always pre-quoted for
/// the *remote* shell so any special chars in `target` are literal.
///
/// When `plant_parent` is true, the returned string is a multi-line shell
/// script that resolves the session id on the remote, plants `@ade-parent
/// 1` on it, installs a `trap … EXIT` to clear the marker on detach, and
/// then runs `tmux attach`. The trap fires after `tmux attach` returns
/// (whether the user detached cleanly or the connection dropped) so the
/// session never carries a stale marker into a future direct (non-ADE)
/// attach. We deliberately *don't* `exec tmux attach` — `exec` replaces
/// the shell and skips the trap.
///
/// We resolve the session id inline (rather than passing one from the
/// local side) because `tmux set-option -t` doesn't accept the `=name`
/// exact-match prefix that ADE uses everywhere else; same constraint
/// `set_session_title_option` documents on the local side.
pub(crate) fn remote_attach_cmd(target: &str, plant_parent: bool) -> String {
    let target_q = hosts::shell_quote(target);
    if !plant_parent {
        return format!("tmux attach -t {}", target_q);
    }
    let bare_name = target.strip_prefix('=').unwrap_or(target);
    let name_q = hosts::shell_quote(bare_name);
    format!(
        "__ade_id=$(tmux list-sessions -F '#{{session_name}}\t#{{session_id}}' 2>/dev/null \
         | awk -F'\\t' -v n={name_q} '$1==n{{print $2; exit}}')\n\
         if [ -n \"$__ade_id\" ]; then\n\
         tmux set-option -t \"$__ade_id\" @ade-parent 1 >/dev/null 2>&1\n\
         trap 'tmux set-option -t \"$__ade_id\" -u @ade-parent >/dev/null 2>&1' EXIT\n\
         fi\n\
         tmux attach -t {target_q}",
        name_q = name_q,
        target_q = target_q,
    )
}

/// Build (program, args) for a remote attach (spawn-and-wait or exec, the
/// builder doesn't care). No local shell is involved (Command::new +
/// execvp).
///
/// SSH joins remaining args with spaces and ships them to the remote shell,
/// which re-parses the resulting command string — so we must pre-quote `target`
/// for the remote shell. The `plant_parent` script returned by
/// `remote_attach_cmd` is already a complete shell script and goes through
/// SSH's remote-shell parse step.
///
/// Mosh by contrast forwards the remote argv directly via execvp on the
/// remote host (no remote shell). When `plant_parent` is false we pass argv
/// byte-for-byte (`tmux attach -t TARGET`). When true, we wrap the script in
/// `sh -c '<script>'` so the planting + trap cleanup actually run.
///
/// Reused by `embedded_term::EmbeddedTerm::spawn_remote` (which always
/// passes `plant_parent: false` — embedded preview attaches share the same
/// routing as full attach but must not pollute the session with a stale
/// `@ade-parent` marker).
pub(crate) fn build_attach_command(
    host: &Host,
    target: &str,
    plant_parent: bool,
) -> (String, Vec<String>) {
    match host.kind {
        HostKind::Ssh => {
            let mut args: Vec<String> = host.ssh_args.clone();
            args.push("-t".to_string());
            args.push(host.target.clone());
            args.push(remote_attach_cmd(target, plant_parent));
            ("ssh".to_string(), args)
        }
        HostKind::Mosh => {
            let mut args: Vec<String> = Vec::new();
            if !host.ssh_args.is_empty() {
                let inner_ssh = std::iter::once("ssh".to_string())
                    .chain(host.ssh_args.iter().cloned())
                    .map(|a| hosts::shell_quote(&a))
                    .collect::<Vec<_>>()
                    .join(" ");
                args.push(format!("--ssh={}", inner_ssh));
            }
            args.push(host.target.clone());
            args.push("--".to_string());
            if plant_parent {
                args.push("sh".to_string());
                args.push("-c".to_string());
                args.push(remote_attach_cmd(target, plant_parent));
            } else {
                args.push("tmux".to_string());
                args.push("attach".to_string());
                args.push("-t".to_string());
                args.push(target.to_string());
            }
            ("mosh".to_string(), args)
        }
    }
}

#[allow(dead_code)]
/// Build a single shell command-line suitable for `tmux new-window -- <cmd>`.
/// Currently unused — remote attaches always go through `Command::spawn` —
/// but kept around in case we ever want to reintroduce a "new window"
/// attach mode. The string is parsed by the *local* shell into argv before
/// reaching ssh/mosh.
///
/// For SSH, we additionally need remote-shell quoting on `target` because ssh
/// joins remaining args with spaces and the remote shell re-parses them — so we
/// shell-quote the entire remote command string twice (outer for local shell,
/// inner for remote shell).
///
/// For Mosh, only the local-shell layer matters — mosh forwards remote argv
/// directly to execvp on the remote, no remote shell. So we shell-quote each
/// arg only for the local layer; the unquoted form reaches tmux verbatim.
fn build_attach_shell_cmd(host: &Host, target: &str) -> String {
    let raw = match host.kind {
        HostKind::Ssh => {
            let remote_cmd = remote_attach_cmd(target, false);
            let mut s = String::from("ssh");
            for a in &host.ssh_args {
                s.push(' ');
                s.push_str(&hosts::shell_quote(a));
            }
            s.push_str(" -t ");
            s.push_str(&hosts::shell_quote(&host.target));
            s.push(' ');
            s.push_str(&hosts::shell_quote(&remote_cmd));
            s
        }
        HostKind::Mosh => {
            let mut s = String::from("mosh");
            if !host.ssh_args.is_empty() {
                let inner_ssh = std::iter::once("ssh".to_string())
                    .chain(host.ssh_args.iter().cloned())
                    .map(|a| hosts::shell_quote(&a))
                    .collect::<Vec<_>>()
                    .join(" ");
                s.push_str(&format!(" --ssh={}", hosts::shell_quote(&inner_ssh)));
            }
            s.push(' ');
            s.push_str(&hosts::shell_quote(&host.target));
            s.push_str(" -- tmux attach -t ");
            s.push_str(&hosts::shell_quote(target));
            s
        }
    };

    // tmux auto-closes a window when its command exits, which silently hides
    // failures (mosh can't connect, remote tmux session was killed, etc.).
    // Wrap the command so a non-zero exit prints the code and waits for the
    // user to press Enter — making the failure visible. Normal exits (the
    // user detached cleanly) close the window as usual.
    format!(
        "{}; __ade_ec=$?; if [ \"$__ade_ec\" -ne 0 ]; then printf '\\n[exited with status %s — press Enter to close]\\n' \"$__ade_ec\"; read -r _ </dev/tty 2>/dev/null || sleep 60; fi",
        raw
    )
}

/// Run a short-lived command and surface success / failure as a Result so
/// the caller can route it into the TUI's `error_message` instead of
/// killing the process. Used for the local `tmux switch-client` path,
/// which needs to keep ADE running across attempts.
fn run_command_capturing(program: &str, args: &[&str]) -> Result<(), String> {
    match Command::new(program).args(args).output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            append_attach_log(&format!(
                "subprocess: {} {}\n  status: {}\n  stdout: {}\n  stderr: {}\n",
                program,
                args.join(" "),
                out.status,
                stdout.trim(),
                stderr.trim(),
            ));

            if out.status.success() {
                Ok(())
            } else {
                let stderr_trim = stderr.trim();
                Err(if stderr_trim.is_empty() {
                    format!("{} exited with {}", program, out.status)
                } else {
                    format!("{} exited with {}: {}", program, out.status, stderr_trim)
                })
            }
        }
        Err(e) => {
            append_attach_log(&format!(
                "subprocess: {} {} (spawn failed)\n  error: {}\n",
                program,
                args.join(" "),
                e
            ));
            Err(format!("failed to run {}: {}", program, e))
        }
    }
}

fn append_attach_log(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open("/tmp/ade-attach.log")
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Persistent run loop. Today's attach is no longer terminal: ADE stays
/// alive across attaches, suspending its TUI for spawn-and-wait branches
/// (local-outside-tmux, remote SSH/Mosh) and resuming when the child
/// exits. Inside-tmux switch-client returns immediately and ADE keeps
/// drawing — the user navigates back via `prefix L` or the smart `prefix
/// B` keybinding installed by `ade install-tmux-config`.
fn run_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();

    loop {
        match run_until_action(terminal, &mut app)? {
            AppAction::Quit => return Ok(()),
            AppAction::AttachSession { name, machine } => {
                let outcome = attach_outcome(&name, &machine);
                log_attach_intent(&name, &machine, outcome);
                match outcome {
                    AttachOutcome::SameSessionNoOp => {
                        append_attach_log(
                            "skipped: already in this session (switch-client would be a no-op)\n",
                        );
                    }
                    AttachOutcome::SwitchClient => {
                        // ADE remains the parent process *of nothing*
                        // after switch-client — the user's tmux client
                        // moves to the target session, but ADE's pane
                        // stays alive in the previous session. Don't
                        // plant `@ade-parent`: the if-shell needs to fall
                        // through to switch-client -l for `prefix B` to
                        // bring the user back to ADE.
                        set_session_title_option(&name, &machine);
                        let target = format!("={}", name);
                        if let Err(msg) = run_command_capturing(
                            "tmux",
                            &["switch-client", "-t", &target],
                        ) {
                            app.error_message = Some(msg);
                        }
                    }
                    AttachOutcome::SpawnAndWait => {
                        set_session_title_option(&name, &machine);
                        // Plant `@ade-parent` for local; the remote shell
                        // wrapper in `remote_attach_cmd` plants + traps
                        // its own marker for SSH/Mosh.
                        set_session_parent_marker(&name, &machine, true);
                        let suspend_err = tui_lifecycle::suspend(terminal)
                            .map_err(|e| format!("suspend tui: {}", e));
                        let attach_err = match suspend_err {
                            // Use the live `app.config` so adds/edits via
                            // the Hosts screen take effect on the very
                            // next attach, no restart required.
                            Ok(()) => spawn_and_wait_attach(&name, &machine, &app.config),
                            Err(e) => Err(e),
                        };
                        // Always attempt resume — a stuck terminal is
                        // worse than a missed error.
                        let resume_err = tui_lifecycle::resume(terminal)
                            .map_err(|e| format!("resume tui: {}", e));
                        // Best-effort marker cleanup. Local: runs after
                        // child.wait() unless ADE itself is SIGKILL'd.
                        // Remote: cleanup lives in the remote shell's
                        // `trap … EXIT` (see `remote_attach_cmd`); abrupt
                        // disconnects or SIGKILL on the remote shell can
                        // leave the marker stale. Recovery is `tmux
                        // set-option -t SESSION -u @ade-parent` on the
                        // affected host.
                        set_session_parent_marker(&name, &machine, false);
                        if let Err(msg) = attach_err {
                            app.error_message = Some(msg);
                        }
                        if let Err(msg) = resume_err {
                            // Surface but don't return — the loop will
                            // try to keep running. If raw mode never came
                            // back, the next draw will likely fail and we
                            // bail then.
                            eprintln!("Warning: {}", msg);
                        }
                        app.refresh();
                    }
                }
                app.action = AppAction::None;
            }
            // run_until_action only returns Quit or AttachSession.
            AppAction::None => unreachable!("run_until_action returns terminal actions only"),
        }
    }
}

/// Drive the TUI until the user picks a session or quits. Returns the
/// action verbatim so `run_loop` can branch on it.
fn run_until_action(terminal: &mut DefaultTerminal, app: &mut App) -> Result<AppAction> {
    loop {
        // Apply any finished background refresh and schedule a new one if
        // due. Non-blocking — the actual SSH/process calls happen on a
        // worker thread.
        app.tick();

        terminal.draw(|frame| ui::render(frame, app))?;
        term_title::set(&app.tab_title());

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                }
                Event::Mouse(mouse) => {
                    // Mouse forwarding only matters in embedded mode;
                    // App::handle_mouse no-ops outside the panel.
                    app.handle_mouse(mouse);
                }
                _ => {}
            }
        }

        if app.should_quit {
            return Ok(AppAction::Quit);
        }

        if let AppAction::AttachSession { .. } = &app.action {
            return Ok(app.action.clone());
        }
    }
}

#[cfg(test)]
mod attach_cmd_tests {
    use super::*;

    fn ssh_host(name: &str, target: &str) -> Host {
        Host {
            name: name.to_string(),
            kind: HostKind::Ssh,
            target: target.to_string(),
            ssh_args: Vec::new(),
        }
    }

    fn mosh_host(name: &str, target: &str) -> Host {
        Host {
            name: name.to_string(),
            kind: HostKind::Mosh,
            target: target.to_string(),
            ssh_args: Vec::new(),
        }
    }

    #[test]
    fn remote_attach_cmd_no_plant_is_simple_attach() {
        // `=work/web` passes hosts::shell_quote untouched (all chars
        // are in the safe set), so the rendered form omits quotes.
        let cmd = remote_attach_cmd("=work/web", false);
        assert_eq!(cmd, "tmux attach -t =work/web");
    }

    #[test]
    fn remote_attach_cmd_with_plant_includes_set_option_and_trap() {
        let cmd = remote_attach_cmd("=work", true);
        assert!(cmd.contains("tmux list-sessions"));
        assert!(cmd.contains("@ade-parent 1"));
        assert!(cmd.contains("trap"));
        assert!(cmd.contains("-u @ade-parent"));
        assert!(cmd.contains("tmux attach -t =work"));
        // Must not `exec` the attach — exec would skip the trap.
        assert!(!cmd.contains("exec tmux attach"));
    }

    #[test]
    fn remote_attach_cmd_quotes_session_names_with_spaces() {
        let cmd = remote_attach_cmd("=my session", true);
        // Both the bare-name (for awk lookup) and the target (for attach)
        // must arrive shell-quoted.
        assert!(cmd.contains("n='my session'"));
        assert!(cmd.contains("tmux attach -t '=my session'"));
    }

    #[test]
    fn remote_attach_cmd_quotes_session_names_with_single_quotes() {
        let cmd = remote_attach_cmd("=it's-mine", true);
        // hosts::shell_quote produces 'it'\''s-mine' for embedded
        // single quotes; verify both occurrences come out intact.
        let want = hosts::shell_quote("it's-mine");
        let want_target = hosts::shell_quote("=it's-mine");
        assert!(cmd.contains(&format!("n={}", want)));
        assert!(cmd.contains(&format!("tmux attach -t {}", want_target)));
    }

    #[test]
    fn build_attach_command_ssh_no_plant_passes_simple_attach() {
        let host = ssh_host("h", "user@h");
        let (program, args) = build_attach_command(&host, "=foo", false);
        assert_eq!(program, "ssh");
        // The remote command is the last arg. `=foo` is in
        // hosts::shell_quote's safe set so no surrounding quotes.
        let remote = args.last().unwrap();
        assert_eq!(remote, "tmux attach -t =foo");
    }

    #[test]
    fn build_attach_command_ssh_with_plant_emits_script() {
        let host = ssh_host("h", "user@h");
        let (program, args) = build_attach_command(&host, "=foo", true);
        assert_eq!(program, "ssh");
        let remote = args.last().unwrap();
        assert!(remote.contains("@ade-parent 1"));
        assert!(remote.contains("trap"));
    }

    #[test]
    fn build_attach_command_mosh_no_plant_uses_direct_argv() {
        let host = mosh_host("h", "user@h");
        let (program, args) = build_attach_command(&host, "=foo", false);
        assert_eq!(program, "mosh");
        // No `sh -c` wrapper.
        assert!(!args.iter().any(|a| a == "sh"));
        // Direct argv after `--`: tmux attach -t =foo
        let dash_idx = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_idx + 1], "tmux");
        assert_eq!(args[dash_idx + 2], "attach");
        assert_eq!(args[dash_idx + 3], "-t");
        assert_eq!(args[dash_idx + 4], "=foo");
    }

    #[test]
    fn build_attach_command_mosh_with_plant_wraps_in_sh_c() {
        let host = mosh_host("h", "user@h");
        let (program, args) = build_attach_command(&host, "=foo", true);
        assert_eq!(program, "mosh");
        let dash_idx = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_idx + 1], "sh");
        assert_eq!(args[dash_idx + 2], "-c");
        let script = &args[dash_idx + 3];
        assert!(script.contains("@ade-parent 1"));
        assert!(script.contains("trap"));
        assert!(script.contains("tmux attach -t =foo"));
    }
}
