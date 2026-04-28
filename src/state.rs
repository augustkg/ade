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
    #[serde(default)]
    pub folders: FoldersState,
    #[serde(default)]
    pub preview_hint: PreviewHintState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NudgeState {
    #[serde(default)]
    pub dismissed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoldersState {
    /// Folder prefixes the user has explicitly collapsed. Folders not
    /// listed here default to expanded — matching the in-tree default at
    /// `src/model.rs::Tree::group`.
    #[serde(default)]
    pub closed: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreviewHintState {
    /// Set to `true` after ADE has emitted the one-time
    /// `tmux display-message "prefix+L returns to ADE"` toast on a
    /// preview-mode hop. Gated so we don't repeat the hint on every Tab.
    #[serde(default)]
    pub shown: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_folders_section_defaults_to_empty() {
        // Existing state.toml files (written before the folders feature
        // landed) only have [tmux_install_nudge]. Loading must succeed and
        // produce an empty FoldersState.
        let body = "[tmux_install_nudge]\ndismissed = true\n";
        let state: State = toml::from_str(body).unwrap();
        assert!(state.tmux_install_nudge.dismissed);
        assert!(state.folders.closed.is_empty());
    }

    #[test]
    fn round_trip_preserves_closed_folders() {
        let mut original = State::default();
        original.tmux_install_nudge.dismissed = true;
        original.folders.closed = vec!["infra".to_string(), "work".to_string()];
        original.preview_hint.shown = true;

        let serialized = toml::to_string_pretty(&original).unwrap();
        let restored: State = toml::from_str(&serialized).unwrap();

        assert_eq!(restored.tmux_install_nudge.dismissed, true);
        assert_eq!(restored.folders.closed, vec!["infra".to_string(), "work".to_string()]);
        assert_eq!(restored.preview_hint.shown, true);
    }

    #[test]
    fn missing_preview_hint_section_defaults() {
        let body = "[tmux_install_nudge]\ndismissed = true\n[folders]\nclosed = [\"work\"]\n";
        let state: State = toml::from_str(body).unwrap();
        assert!(!state.preview_hint.shown);
    }

    #[test]
    fn empty_input_yields_default() {
        let state: State = toml::from_str("").unwrap();
        assert!(!state.tmux_install_nudge.dismissed);
        assert!(state.folders.closed.is_empty());
    }
}
