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
    pub preview_pane: PreviewPaneState,
    #[serde(default)]
    pub notifications: NotificationsState,
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
pub struct PreviewPaneState {
    /// User's saved preference for the right-side ambient preview panel.
    /// Default `false` — the panel is opt-in. Toggled via `p` in the tree
    /// view; the new value is persisted immediately.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationsState {
    /// User's saved preference for macOS desktop notifications when
    /// Claude finishes a turn or pops a permission prompt. Default
    /// `false` — opt-in. Toggled via `N` in the tree view (persisted
    /// immediately). When false, every call to `notifications::fire`
    /// from `App::apply_refresh_result` is short-circuited at the top
    /// of the suppression chain.
    #[serde(default)]
    pub enabled: bool,
    /// Set true on the first time the user either presses `N` to enable
    /// or `x` to dismiss the first-run footer nudge. Drives whether the
    /// peach "Desktop notifications available — press N…" line is
    /// rendered at the bottom of the main tree.
    #[serde(default)]
    pub first_seen: bool,
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
        original.preview_pane.enabled = true;

        let serialized = toml::to_string_pretty(&original).unwrap();
        let restored: State = toml::from_str(&serialized).unwrap();

        assert_eq!(restored.tmux_install_nudge.dismissed, true);
        assert_eq!(restored.folders.closed, vec!["infra".to_string(), "work".to_string()]);
        assert!(restored.preview_pane.enabled);
    }

    #[test]
    fn missing_preview_pane_section_defaults() {
        let body = "[tmux_install_nudge]\ndismissed = true\n[folders]\nclosed = [\"work\"]\n";
        let state: State = toml::from_str(body).unwrap();
        assert!(!state.preview_pane.enabled);
    }

    #[test]
    fn missing_notifications_section_defaults() {
        // Existing state.toml files written before notifications shipped
        // must load cleanly with notifications off and first_seen false
        // (so the first-run nudge fires for them).
        let body = "[tmux_install_nudge]\ndismissed = true\n";
        let state: State = toml::from_str(body).unwrap();
        assert!(!state.notifications.enabled);
        assert!(!state.notifications.first_seen);
    }

    #[test]
    fn round_trip_preserves_notifications_state() {
        let mut original = State::default();
        original.notifications.enabled = true;
        original.notifications.first_seen = true;
        let serialized = toml::to_string_pretty(&original).unwrap();
        let restored: State = toml::from_str(&serialized).unwrap();
        assert!(restored.notifications.enabled);
        assert!(restored.notifications.first_seen);
    }

    #[test]
    fn empty_input_yields_default() {
        let state: State = toml::from_str("").unwrap();
        assert!(!state.tmux_install_nudge.dismissed);
        assert!(state.folders.closed.is_empty());
        assert!(!state.notifications.enabled);
    }
}
