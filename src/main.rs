mod app;
mod claude_status;
mod cwd;
mod hosts;
mod install_hooks;
mod model;
mod refresh;
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
         \x20\x20ade                          Launch the TUI\n\
         \x20\x20ade install-hooks            Install Claude Code status hooks locally\n\
         \x20\x20ade install-hooks --host H   Install hooks on a configured remote host\n\
         \x20\x20ade help                     Show this message"
    );
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

fn attach(name: &str, machine: &Machine, config: &Config) {
    let target = format!("={}", name);
    let inside = tmux::is_inside_tmux();

    match machine {
        Machine::Local => {
            if inside {
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
            if inside {
                let inner = build_attach_shell_cmd(host, &target);
                let window_name = format!("{}@{}", name, host.name);
                run_status("tmux", &["new-window", "-n", &window_name, &inner]);
            } else {
                let (program, args) = build_attach_command(host, &target);
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                exec_replace(&program, &arg_refs);
            }
        }
    }
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

/// Build a single shell command-line suitable for `tmux new-window -- <cmd>`.
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
    match host.kind {
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
    }
}

fn run_status(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("Error: {} exited with {}", program, status);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: failed to run {}: {}", program, e);
            std::process::exit(1);
        }
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
