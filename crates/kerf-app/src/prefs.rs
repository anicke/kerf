//! User preferences, stored as JSON in the platform config folder
//! (`~/.config/kerf/preferences.json` on Linux). `KERF_CONFIG_DIR` overrides it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How the timeline follows the playhead during playback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowMode {
    /// The view stays put.
    Off,
    /// Turn the page when the playhead reaches the right edge.
    #[default]
    Page,
    /// Keep the playhead in the middle and scroll the audio past it.
    Center,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Output device id (`None`: the system default).
    pub output_device: Option<String>,
    /// Device buffer size in frames (`None`: the device default).
    pub buffer_frames: Option<u32>,
    /// Space stops and returns to where playback started (otherwise it stops
    /// at the playhead). Shift+Space always does the opposite.
    pub space_returns_to_start: bool,
    /// Preview around the cursor (P): seconds before and after.
    pub preview_before: f64,
    pub preview_after: f64,
    /// Crash-recovery snapshot interval in seconds (0 turns it off).
    pub autosave_secs: u64,
    pub show_markers_panel: bool,
    pub show_history_panel: bool,
    pub show_stats_panel: bool,
    pub time_format: kerf_core::util::TimeFormat,
    /// Put selection edges and split points on zero crossings (avoids clicks).
    pub snap_zero_crossings: bool,
    /// Which FFmpeg decodes what the built-in decoder can't (HE-AAC, AC-3, WMA…).
    pub ffmpeg: kerf_core::decode::FfmpegSetting,
    /// Let AI assistants edit the open project over MCP (off by default).
    pub assistant_enabled: bool,
    /// Folders an assistant may read and write (empty: the home folder).
    pub assistant_folders: Vec<PathBuf>,
    pub follow: FollowMode,
    /// Recently opened or saved projects, most recent first.
    pub recent_projects: Vec<PathBuf>,
    /// Changed keyboard shortcuts: command id → keys (see `keymap`).
    pub shortcuts: crate::keymap::Overrides,
    /// Last export format.
    pub export: kerf_core::io::ExportSettings,
    /// Snap split/CUE track boundaries to CD sectors (no sector boundary errors).
    pub export_cd_safe: bool,
    /// File names for Split at Markers (see `kerf_core::naming`).
    pub track_names: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            output_device: None,
            buffer_frames: None,
            space_returns_to_start: true,
            preview_before: 2.0,
            preview_after: 2.0,
            autosave_secs: 60,
            show_markers_panel: false,
            show_history_panel: false,
            show_stats_panel: false,
            time_format: kerf_core::util::TimeFormat::MinSec,
            snap_zero_crossings: false,
            ffmpeg: Default::default(),
            assistant_enabled: false,
            assistant_folders: Vec::new(),
            follow: FollowMode::Page,
            recent_projects: Vec::new(),
            shortcuts: Default::default(),
            export: Default::default(),
            export_cd_safe: true,
            track_names: kerf_core::naming::DEFAULT_TRACK_NAMES.into(),
        }
    }
}

fn path() -> Option<PathBuf> {
    let dir = match std::env::var_os("KERF_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::config_dir()?.join("kerf"),
    };
    Some(dir.join("preferences.json"))
}

impl Prefs {
    /// Load saved preferences (defaults if none or unreadable).
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = path() else { return Ok(()) };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(p, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"preview_before": 1.5}"#).unwrap();
        assert_eq!(p.preview_before, 1.5);
        assert_eq!(p.autosave_secs, 60);
        assert!(p.space_returns_to_start);
        let back: Prefs = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
