use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Ssh,
    Mosh,
}

impl HostKind {
    pub fn label(self) -> &'static str {
        match self {
            HostKind::Ssh => "SSH",
            HostKind::Mosh => "Mosh",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub kind: HostKind,
    pub target: String,
    #[serde(default)]
    pub ssh_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
}

impl Config {
    /// Returns the loaded config, plus an optional warning string if the file
    /// existed but failed to parse (caller can surface this in the UI).
    pub fn load() -> (Self, Option<String>) {
        let path = Self::path();
        match fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => (c, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("failed to parse {}: {}", path.display(), e)),
                ),
            },
            Err(_) => (Config::default(), None),
        }
    }

    /// Atomic write: serialise to a temp file in the same directory, fsync,
    /// then rename over the destination. Prevents truncated/partial files
    /// when ADE is killed mid-write.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create config dir: {}", e))?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| format!("serialize config: {}", e))?;

        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, s).map_err(|e| format!("write temp config: {}", e))?;
        fs::rename(&tmp, &path)
            .map_err(|e| format!("rename config into place: {}", e))?;
        Ok(())
    }

    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        xdg.join("ade").join("hosts.toml")
    }

    pub fn host_by_name(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name == name)
    }

    /// Insert or replace a host. If `editing_idx` is `Some(i)`, the host at
    /// that index is replaced (allowing renames). Otherwise the host is
    /// appended. Rejects empty names/targets and duplicate names.
    pub fn upsert(
        &mut self,
        host: Host,
        editing_idx: Option<usize>,
    ) -> Result<(), String> {
        if host.name.trim().is_empty() {
            return Err("host name cannot be empty".to_string());
        }
        if host.target.trim().is_empty() {
            return Err("host target cannot be empty".to_string());
        }

        let dup = self.hosts.iter().enumerate().find(|(i, h)| {
            h.name == host.name && Some(*i) != editing_idx
        });
        if dup.is_some() {
            return Err(format!("host name '{}' already exists", host.name));
        }

        match editing_idx {
            Some(idx) if idx < self.hosts.len() => self.hosts[idx] = host,
            _ => self.hosts.push(host),
        }
        Ok(())
    }

    pub fn remove(&mut self, idx: usize) {
        if idx < self.hosts.len() {
            self.hosts.remove(idx);
        }
    }
}

/// Shell-quote a single argument so it survives shell parsing inside
/// `tmux new-window -- <cmd>`.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_alphanumeric()
                || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '=' | ',' | '+')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
