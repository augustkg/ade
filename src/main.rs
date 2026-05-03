#[cfg(test)]
mod acceptance;
mod app;
mod claude_status;
mod cwd;
mod debug;
mod embedded_term;
mod hosts;
mod install_hooks;
mod install_tmux;
mod model;
mod notifications;
mod preview_pane;
mod refresh;
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
         \x20\x20ade install-tmux-config [--host H]     Install tmux clipboard config (local or remote)\n\
         \x20\x20ade install-tmux-config --uninstall    Remove the tmux clipboard config\n\
         \x20\x20ade debug claude [--host H]            Diagnose why ADE does/doesn't see Claude per pane\n\
         \x20\x20ade help                               Show this message"
    );
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
            other => {
                eprintln!("Error: unknown argument '{}'", other);
                std::process::exit(2);
            }
        }
        i += 1;
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

fn run_install_tmux(args: &[String]) -> Result<()> {
    let mut host: Option<String> = None;
    let mut uninstall = false;
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
            other => {
                eprintln!("Error: unknown argument '{}'", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if uninstall {
        let result = match host {
            None => install_tmux::uninstall_local()
                .map(|r| ("local".to_string(), r.summary())),
            Some(name) => {
                let (config, _warning) = Config::load();
                install_tmux::uninstall_remote(&config, &name)
                    .map(|r| (name.clone(), r.summary()))
            }
        };
        return match result {
            Ok((target, msg)) => {
                println!("{}: {}", target, msg);
                Ok(())
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
    }

    let result = match host {
        None => install_tmux::install_local().map(|r| {
            let mut msg = format!("local: {}", r.summary());
            if r.mouse_off {
                msg.push_str(
                    "\nWarning: detected `mouse off` in your tmux config. \
                     ADE's clipboard config requires `mouse on` for drag-select-to-copy. \
                     Remove or update that line, then reload tmux.",
                );
            }
            if !r.is_noop() {
                msg.push_str(
                    "\nNext: run `tmux source-file ~/.tmux.conf` (or restart tmux) to apply.",
                );
            }
            msg
        }),
        Some(name) => {
            let (config, _warning) = Config::load();
            install_tmux::install_remote(&config, &name).map(|r| {
                let mut msg = format!("{}: {}", name, r.summary());
                if !r.is_noop() {
                    msg.push_str(
                        "\nNext: on that host, run `tmux source-file ~/.tmux.conf` \
                         (or restart tmux) to apply.",
                    );
                }
                msg
            })
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
