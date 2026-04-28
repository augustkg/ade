mod app;
mod claude_status;
mod cwd;
mod debug;
mod embedded_term;
mod hosts;
mod install_hooks;
mod install_tmux;
mod model;
mod preview_pane;
mod refresh;
mod ssh_io;
mod state;
mod text_field;
mod theme;
mod tmux;
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
    let result = run(&mut terminal);
    ratatui::restore();

    match result {
        Ok(Some(AppAction::AttachSession { name, machine })) => {
            // Re-load config from disk so any host edits made during the TUI session apply.
            let (config, _warning) = Config::load();
            attach(&name, &machine, &config);
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
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

fn attach(name: &str, machine: &Machine, config: &Config) {
    let target = format!("={}", name);
    let inside = tmux::is_inside_tmux();

    // Diagnostic log: starts a fresh /tmp/ade-attach.log on every attempt.
    // run_status appends subprocess results below, so the file ends up with
    // a complete trace of one attach attempt — including the env we used to
    // decide inside vs outside tmux.
    let env_tmux = std::env::var("TMUX").unwrap_or_default();
    let env_tmux_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let current = tmux::current_session().unwrap_or_default();
    let log = format!(
        "{}\n\
         attach: name={} machine={:?} inside_tmux={} target={}\n\
         env: TMUX={:?} TMUX_PANE={:?}\n\
         current_session: {:?}\n",
        chrono_now(),
        name,
        machine,
        inside,
        target,
        env_tmux,
        env_tmux_pane,
        current,
    );
    let _ = std::fs::write("/tmp/ade-attach.log", log);

    match machine {
        Machine::Local => {
            if inside {
                // Same-session early return: switch-client to the session
                // we're already in is a silent no-op, so just quit cleanly
                // instead. The user sees no popup; ADE exits and they're
                // back at the session they wanted (the one they were in).
                if let Some(current) = tmux::current_session() {
                    if current == name {
                        append_attach_log(
                            "skipped: already in this session (switch-client would be a no-op)\n",
                        );
                        return;
                    }
                }
                run_status("tmux", &["switch-client", "-t", &target]);
            } else {
                exec_replace("tmux", &["attach-session", "-t", &target]);
            }
        }
        Machine::Remote(host_name) => {
            let Some(host) = config.host_by_name(host_name) else {
                eprintln!("Error: host '{}' not found in config", host_name);
                std::process::exit(1);
            };
            // Always exec-replace into ssh/mosh. From outside tmux, that's
            // the obvious thing. From inside tmux, this means our current
            // pane becomes the mosh/ssh client and the user sees the remote
            // session attach right where ADE was — same UX in both contexts,
            // no "did the new window auto-select?" ambiguity. When the user
            // detaches, the pane closes (or returns to its shell), same as
            // any other terminal-replacing command.
            let (program, args) = build_attach_command(host, &target);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            exec_replace(&program, &arg_refs);
            let _ = inside; // remote attach path no longer branches on inside-tmux
        }
    }
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
fn remote_attach_cmd(target: &str) -> String {
    format!("tmux attach -t {}", hosts::shell_quote(target))
}

/// Build (program, args) for an exec-replace attach to a remote host.
/// No local shell is involved (Command::new + execvp).
///
/// SSH joins remaining args with spaces and ships them to the remote shell,
/// which re-parses the resulting command string — so we must pre-quote `target`
/// for the remote shell.
///
/// Mosh by contrast forwards the remote argv directly via execvp on the remote
/// host (no remote shell), so each arg goes through byte-for-byte: we pass them
/// separately and never quote.
fn build_attach_command(host: &Host, target: &str) -> (String, Vec<String>) {
    match host.kind {
        HostKind::Ssh => {
            let mut args: Vec<String> = host.ssh_args.clone();
            args.push("-t".to_string());
            args.push(host.target.clone());
            args.push(remote_attach_cmd(target));
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
            args.push("tmux".to_string());
            args.push("attach".to_string());
            args.push("-t".to_string());
            args.push(target.to_string());
            ("mosh".to_string(), args)
        }
    }
}

#[allow(dead_code)]
/// Build a single shell command-line suitable for `tmux new-window -- <cmd>`.
/// Currently unused — remote attaches always go through exec_replace — but
/// kept around in case we ever want to reintroduce a "new window" attach mode.
/// The string is parsed by the *local* shell into argv before reaching ssh/mosh.
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
            let remote_cmd = remote_attach_cmd(target);
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

fn run_status(program: &str, args: &[&str]) {
    match Command::new(program).args(args).output() {
        Ok(out) => {
            // Always append a record so /tmp/ade-attach.log captures the
            // exact subprocess result, including silent successes.
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

            if !out.status.success() {
                let stderr_trim = stderr.trim();
                if stderr_trim.is_empty() {
                    eprintln!("Error: {} exited with {}", program, out.status);
                } else {
                    eprintln!(
                        "Error: {} exited with {}: {}",
                        program, out.status, stderr_trim
                    );
                }
                std::process::exit(1);
            }
        }
        Err(e) => {
            append_attach_log(&format!(
                "subprocess: {} {} (spawn failed)\n  error: {}\n",
                program,
                args.join(" "),
                e
            ));
            eprintln!("Error: failed to run {}: {}", program, e);
            std::process::exit(1);
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

fn exec_replace(program: &str, args: &[&str]) {
    #[cfg(unix)]
    {
        let err = Command::new(program).args(args).exec();
        eprintln!("Error: failed to exec {}: {}", program, err);
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        run_status(program, args);
    }
}

fn run(terminal: &mut DefaultTerminal) -> Result<Option<AppAction>> {
    let mut app = App::new();

    loop {
        // Apply any finished background refresh and schedule a new one if
        // due. Non-blocking — the actual SSH/process calls happen on a
        // worker thread.
        app.tick();

        terminal.draw(|frame| ui::render(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                }
            }
        }

        if app.should_quit {
            return Ok(Some(AppAction::Quit));
        }

        if let AppAction::AttachSession { .. } = &app.action {
            return Ok(Some(app.action.clone()));
        }
    }
}
