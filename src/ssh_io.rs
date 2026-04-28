//! Small SSH command helpers shared by `install_hooks` and `install_tmux`.
//! Each function is a thin wrapper around `ssh <host> <remote_cmd>` with the
//! ADE standard option set (`BatchMode`, `ConnectTimeout`, `accept-new`).

use std::io::Write;
use std::process::{Command, Stdio};

use crate::hosts::Host;

pub const SSH_OPTS: &[&str] = &[
    "-o",
    "ConnectTimeout=5",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// Run `remote_cmd` on `host` over SSH and return stdout. Errors only on
/// spawn failure or non-zero exit status. Callers that need lenient reads
/// (e.g. "file may not exist") should append `2>/dev/null || true` to the
/// command themselves.
pub fn run(host: &Host, remote_cmd: &str) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS);
    for a in &host.ssh_args {
        cmd.arg(a);
    }
    cmd.arg(&host.target);
    cmd.arg(remote_cmd);
    let out = cmd.output().map_err(|e| format!("ssh failed: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ssh exit {}: {}", out.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run `remote_cmd` on `host` over SSH and pipe `stdin_body` to it. Returns
/// stdout on success; errors on spawn failure or non-zero exit.
pub fn run_with_stdin(
    host: &Host,
    remote_cmd: &str,
    stdin_body: &[u8],
) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS);
    for a in &host.ssh_args {
        cmd.arg(a);
    }
    cmd.arg(&host.target);
    cmd.arg(remote_cmd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("ssh spawn: {}", e))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "no ssh stdin".to_string())?;
        stdin
            .write_all(stdin_body)
            .map_err(|e| format!("ssh write: {}", e))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("ssh wait: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ssh exit {}: {}", out.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
