//! `~/.config/ade/state.toml` — small persistent state for the TUI (nudge
//! dismissals, etc). Distinct from `hosts.toml` so it can be wiped without
//! losing host config.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub tmux_install_nudge: NudgeState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NudgeState {
    #[serde(default)]
    pub dismissed: bool,
}

impl State {
    /// Best-effort load: returns `Default` on any error (missing file, parse
    /// error, no $HOME). Persistent state is non-critical — the TUI must
    /// boot without it.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let body = match fs::read_to_string(&path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        toml::from_str(&body).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "no $HOME set".to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {}", parent.display(), e))?;
        }
        let s = toml::to_string_pretty(self).map_err(|e| format!("serialize state: {}", e))?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, s).map_err(|e| format!("write temp state: {}", e))?;
        fs::rename(&tmp, &path).map_err(|e| format!("rename state into place: {}", e))?;
        Ok(())
    }

    pub fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Some(xdg.join("ade").join("state.toml"))
    }
}
