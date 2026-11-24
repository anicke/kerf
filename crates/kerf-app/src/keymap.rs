//! Keyboard shortcuts: every bindable command with its default keys, the
//! user's changes (stored in preferences as `command id → keys`), and the
//! resolved map the app dispatches from and labels menus with.

use std::collections::BTreeMap;

use eframe::egui::{Key, KeyboardShortcut, Modifiers};

use crate::app::{Action, ExportKind};

/// A key with modifiers. `command` is Ctrl, or ⌘ on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub command: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: Key,
}

impl Binding {
    pub fn new(modifiers: Modifiers, key: Key) -> Self {
        Self {
            command: modifiers.command || modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            key,
        }
    }

    fn modifiers(self) -> Modifiers {
        let mut m = Modifiers::NONE;
        if self.command {
            m |= Modifiers::COMMAND;
        }
        if self.shift {
            m |= Modifiers::SHIFT;
        }
        if self.alt {
            m |= Modifiers::ALT;
        }
        m
    }

    pub fn shortcut(self) -> KeyboardShortcut {
        KeyboardShortcut::new(self.modifiers(), self.key)
    }

    fn modifier_count(self) -> usize {
        self.command as usize + self.shift as usize + self.alt as usize
    }

    /// Parse the stored form, e.g. `Ctrl+Shift+ArrowLeft` or `Comma`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts: Vec<&str> = s.split('+').collect();
        let key = Key::from_name(parts.pop()?)?;
        let mut b = Self::new(Modifiers::NONE, key);
        for m in parts {
            match m {
                "Ctrl" | "Cmd" => b.command = true,
                "Shift" => b.shift = true,
                "Alt" => b.alt = true,
                _ => return None,
            }
        }
        Some(b)
    }

    /// The stored form (stable key names, no symbols).
    pub fn to_stored(self) -> String {
        let mut s = String::new();
        for (on, name) in [
            (self.command, "Ctrl+"),
            (self.shift, "Shift+"),
            (self.alt, "Alt+"),
        ] {
            if on {
                s.push_str(name);
            }
        }
        s.push_str(self.key.name());
        s
    }

    /// For people: `Ctrl+Shift+←`, `Del`, `,`.
    pub fn label(self) -> String {
        let mut s = String::new();
        let cmd = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let alt = if cfg!(target_os = "macos") {
            "⌥"
        } else {
            "Alt+"
        };
        for (on, name) in [(self.command, cmd), (self.shift, "Shift+"), (self.alt, alt)] {
            if on {
                s.push_str(name);
            }
        }
        s.push_str(match self.key {
            Key::ArrowLeft => "←",
            Key::ArrowRight => "→",
            Key::ArrowUp => "↑",
            Key::ArrowDown => "↓",
            Key::Delete => "Del",
            Key::Escape => "Esc",
            Key::PageUp => "PgUp",
            Key::PageDown => "PgDn",
            Key::Backspace => "⌫",
            Key::Minus => "-",
            k => k.symbol_or_name(),
        });
        s
    }
}

/// A command that can have shortcuts.
pub struct Command {
    /// Stable name used in preferences.
    pub id: &'static str,
    pub name: &'static str,
    pub group: &'static str,
    pub action: Action,
    pub defaults: &'static [&'static str],
}

macro_rules! commands {
    ($( $group:literal { $( $id:literal, $name:literal, $action:expr, [$($key:literal),*]; )* } )*) => {
        &[ $( $( Command { id: $id, name: $name, group: $group, action: $action, defaults: &[$($key),*] }, )* )* ]
    };
}

use Action as A;

/// Every bindable command, in the order the editor lists them.
pub const COMMANDS: &[Command] = commands! {
    "File" {
        "new", "New project", A::New, ["Ctrl+N"];
        "open", "Open project…", A::Open, ["Ctrl+O"];
        "open_recent", "Open recent project…", A::OpenRecent, ["Ctrl+Shift+O"];
        "import", "Import audio…", A::Import, ["Ctrl+I"];
        "save", "Save project", A::Save, ["Ctrl+S"];
        "save_as", "Save project as…", A::SaveAs, ["Ctrl+Shift+S"];
        "export_mix", "Export mix…", A::Export(ExportKind::Mix), ["Ctrl+E"];
        "export_selection", "Export selection…", A::Export(ExportKind::Selection), [];
        "export_markers", "Export split at markers…", A::Export(ExportKind::Markers), ["Ctrl+Shift+E"];
        "export_stems", "Export tracks as stems…", A::Export(ExportKind::Stems), [];
        "export_cue", "Export CUE sheet…", A::Export(ExportKind::CueSheet), [];
        "album", "Album…", A::Album, ["Ctrl+Shift+A"];
        "audio_cache", "Audio cache…", A::AudioCache, [];
        "quit", "Quit", A::Quit, ["Ctrl+Q"];
    }
    "Edit" {
        "undo", "Undo", A::Undo, ["Ctrl+Z"];
        "redo", "Redo", A::Redo, ["Ctrl+Shift+Z", "Ctrl+Y"];
        "cut", "Cut", A::Cut, ["Ctrl+X"];
        "copy", "Copy", A::Copy, ["Ctrl+C"];
        "paste", "Paste", A::Paste, ["Ctrl+V"];
        "delete", "Delete (close gap)", A::Delete, ["Delete", "Backspace"];
        "silence", "Silence (keep gap)", A::Silence, ["Ctrl+L"];
        "insert_silence", "Insert silence…", A::InsertSilence, [];
        "crop", "Trim to selection", A::Crop, ["Ctrl+T"];
        "split", "Split", A::Split, ["S"];
        "select_all", "Select all", A::SelectAll, ["Ctrl+A"];
        "select_none", "Select none", A::SelectNone, ["Ctrl+D", "Escape"];
        "select_song", "Select song at cursor", A::SelectSong, [];
        "zero_crossings", "Selection to zero crossings", A::ZeroCrossings, ["Shift+Z"];
        "zero_crossing_snap", "Snap edits to zero crossings", A::ToggleZeroCrossingSnap, [];
        "preferences", "Preferences…", A::Preferences, ["Ctrl+Comma"];
        "shortcuts", "Keyboard shortcuts…", A::Shortcuts, [];
    }
    "Process" {
        "normalize", "Normalize…", A::Normalize, ["Ctrl+M"];
        "gain", "Gain…", A::Gain, ["Ctrl+G"];
        "fade_in", "Fade in", A::FadeIn, ["I"];
        "fade_out", "Fade out", A::FadeOut, ["O"];
        "remove_fades", "Remove fades", A::RemoveFades, [];
        "bounce", "Render in place", A::Bounce, ["Ctrl+R"];
    }
    "Tracks" {
        "add_track", "Add track", A::AddTrack, ["Ctrl+Shift+N"];
        "remove_tracks", "Remove selected tracks", A::RemoveTracks, [];
        "track_up", "Previous track", A::TrackUp, ["ArrowUp"];
        "track_down", "Next track", A::TrackDown, ["ArrowDown"];
        "extend_track_up", "Add previous track to selection", A::ExtendTrackUp, ["Shift+ArrowUp"];
        "extend_track_down", "Add next track to selection", A::ExtendTrackDown, ["Shift+ArrowDown"];
        "taller_tracks", "Taller tracks", A::TallerTracks, ["Alt+ArrowUp"];
        "shorter_tracks", "Shorter tracks", A::ShorterTracks, ["Alt+ArrowDown"];
        "fit_tracks", "Fit tracks to window", A::FitTracks, [];
        "reset_track_heights", "Reset track heights", A::ResetTrackHeights, [];
    }
    "Transport" {
        "play_stop", "Play / stop", A::PlayStop, ["Space"];
        "stop_at_playhead", "Stop at playhead (or return)", A::StopAtPlayhead, ["Shift+Space"];
        "preview", "Preview around cursor", A::PreviewCursor, ["P"];
        "shuttle_forward", "Shuttle forward (faster)", A::ShuttleForward, ["L"];
        "shuttle_reverse", "Shuttle backward (faster)", A::ShuttleReverse, ["J"];
        "shuttle_stop", "Stop shuttle (hold for half speed)", A::ShuttleStop, ["K"];
        "loop", "Loop playback", A::ToggleLoop, ["Shift+L"];
    }
    "Navigate" {
        "cursor_left", "Nudge cursor left", A::CursorLeft, ["ArrowLeft"];
        "cursor_right", "Nudge cursor right", A::CursorRight, ["ArrowRight"];
        "extend_left", "Extend selection left", A::ExtendLeft, ["Shift+ArrowLeft"];
        "extend_right", "Extend selection right", A::ExtendRight, ["Shift+ArrowRight"];
        "prev_edit_point", "Previous edit point", A::PrevEditPoint, ["Ctrl+ArrowLeft"];
        "next_edit_point", "Next edit point", A::NextEditPoint, ["Ctrl+ArrowRight"];
        "extend_prev_edit_point", "Extend to previous edit point", A::ExtendPrevEditPoint, ["Ctrl+Shift+ArrowLeft"];
        "extend_next_edit_point", "Extend to next edit point", A::ExtendNextEditPoint, ["Ctrl+Shift+ArrowRight"];
        "go_start", "Go to start", A::GoStart, ["Home"];
        "go_end", "Go to end", A::GoEnd, ["End"];
        "extend_to_start", "Extend selection to start", A::ExtendToStart, ["Shift+Home"];
        "extend_to_end", "Extend selection to end", A::ExtendToEnd, ["Shift+End"];
        "go_selection_start", "Go to selection start", A::GoSelectionStart, ["OpenBracket"];
        "go_selection_end", "Go to selection end", A::GoSelectionEnd, ["CloseBracket"];
        "page_left", "Scroll a screen left", A::PageLeft, ["PageUp"];
        "page_right", "Scroll a screen right", A::PageRight, ["PageDown"];
    }
    "Markers" {
        "add_marker", "Add marker", A::AddMarker, ["M"];
        "prev_marker", "Previous marker", A::PrevMarker, ["Comma"];
        "next_marker", "Next marker", A::NextMarker, ["Period"];
        "detect_songs", "Detect song starts…", A::DetectSongs, [];
        "snap_markers_to_sectors", "Snap markers to CD sectors", A::SnapMarkersToSectors, [];
    }
    "View" {
        "zoom_in", "Zoom in", A::ZoomIn, ["Plus", "Equals"];
        "zoom_out", "Zoom out", A::ZoomOut, ["Minus"];
        "zoom_fit", "Zoom to fit", A::ZoomFit, ["Ctrl+F"];
        "zoom_selection", "Zoom to selection", A::ZoomSelection, ["Z"];
        "vzoom_in", "Vertical zoom in", A::VZoomIn, ["Ctrl+ArrowUp"];
        "vzoom_out", "Vertical zoom out", A::VZoomOut, ["Ctrl+ArrowDown"];
        "vzoom_reset", "Reset vertical zoom", A::VZoomReset, [];
        "db_scale", "Waveform in dB", A::ToggleDbScale, ["D"];
        "snap", "Snap to edges and markers", A::ToggleSnap, ["N"];
        "follow_page", "Follow the playhead by pages", A::FollowPage, [];
        "follow_center", "Keep the playhead centred", A::FollowCenter, [];
        "markers_panel", "Markers panel", A::ToggleMarkersPanel, ["Ctrl+Shift+M"];
        "history_panel", "History panel", A::ToggleHistoryPanel, ["Ctrl+Shift+H"];
        "stats_panel", "Statistics panel", A::ToggleStatsPanel, ["Ctrl+Shift+I"];
    }
    "Help" {
        "about", "About Kerf", A::About, [];
    }
};

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

fn defaults(c: &Command) -> Vec<Binding> {
    c.defaults
        .iter()
        .filter_map(|s| Binding::parse(s))
        .collect()
}

/// User changes: command id → its keys (empty: no shortcut). Commands not
/// listed keep their defaults.
pub type Overrides = BTreeMap<String, Vec<String>>;

/// The resolved shortcuts.
#[derive(Clone, Default)]
pub struct Keymap {
    /// Most specific first (egui ignores extra Shift/Alt when matching).
    entries: Vec<(Binding, Action)>,
}

impl Keymap {
    pub fn new(overrides: &Overrides) -> Self {
        let custom: Vec<(&Command, Vec<Binding>)> = COMMANDS
            .iter()
            .filter_map(|c| {
                let keys = overrides.get(c.id)?;
                Some((c, keys.iter().filter_map(|s| Binding::parse(s)).collect()))
            })
            .collect();
        let mut entries: Vec<(Binding, Action)> = Vec::new();
        // Custom keys first: they take a key away from a default that had it.
        for (c, keys) in &custom {
            for b in keys {
                if !entries.iter().any(|(e, _)| e == b) {
                    entries.push((*b, c.action));
                }
            }
        }
        for c in COMMANDS {
            if overrides.contains_key(c.id) {
                continue;
            }
            for b in defaults(c) {
                if !entries.iter().any(|(e, _)| *e == b) {
                    entries.push((b, c.action));
                }
            }
        }
        entries.sort_by_key(|(b, _)| std::cmp::Reverse(b.modifier_count()));
        Self { entries }
    }

    pub fn entries(&self) -> &[(Binding, Action)] {
        &self.entries
    }

    pub fn bindings(&self, a: Action) -> impl Iterator<Item = Binding> + '_ {
        self.entries
            .iter()
            .filter(move |(_, x)| *x == a)
            .map(|(b, _)| *b)
    }

    /// The command a key runs.
    pub fn action(&self, b: Binding) -> Option<Action> {
        self.entries.iter().find(|(e, _)| *e == b).map(|(_, a)| *a)
    }

    /// The first shortcut of `a`, for menus ("" if none).
    pub fn label(&self, a: Action) -> String {
        self.bindings(a)
            .next()
            .map(Binding::label)
            .unwrap_or_default()
    }

    /// `text (shortcut)`, or just `text`.
    pub fn tip(&self, text: &str, a: Action) -> String {
        match self.bindings(a).next() {
            Some(b) => format!("{text} ({})", b.label()),
            None => text.to_string(),
        }
    }
}

/// Give `id` the keys `keys` in `overrides`, taking each away from any other
/// command that had it. Returns the names of the commands that lost a key.
pub fn assign(overrides: &mut Overrides, id: &str, keys: Vec<Binding>) -> Vec<&'static str> {
    let current = Keymap::new(overrides);
    let mut losers = Vec::new();
    for b in &keys {
        let Some(other) = current.action(*b) else {
            continue;
        };
        let Some(oc) = COMMANDS.iter().find(|c| c.action == other && c.id != id) else {
            continue;
        };
        let kept: Vec<String> = current
            .bindings(other)
            .filter(|x| x != b)
            .map(Binding::to_stored)
            .collect();
        overrides.insert(oc.id.to_string(), kept);
        losers.push(oc.name);
    }
    set(overrides, id, keys);
    losers
}

/// Store `keys` for `id`, dropping the entry when they equal the defaults.
fn set(overrides: &mut Overrides, id: &str, keys: Vec<Binding>) {
    let Some(c) = command(id) else { return };
    if keys == defaults(c) {
        overrides.remove(id);
    } else {
        overrides.insert(
            id.to_string(),
            keys.into_iter().map(Binding::to_stored).collect(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_parses_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert_eq!(defaults(c).len(), c.defaults.len(), "{}", c.id);
            for b in defaults(c) {
                assert!(seen.insert(b), "{} is bound twice", b.to_stored());
                assert_eq!(Binding::parse(&b.to_stored()), Some(b));
            }
        }
        let ids: std::collections::HashSet<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), COMMANDS.len(), "ids are unique");
    }

    #[test]
    fn specific_shortcuts_come_first() {
        let k = Keymap::new(&Overrides::new());
        let counts: Vec<usize> = k
            .entries()
            .iter()
            .map(|(b, _)| b.modifier_count())
            .collect();
        assert!(counts.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(
            k.label(Action::Undo),
            Binding::parse("Ctrl+Z").unwrap().label()
        );
    }

    #[test]
    fn assigning_moves_a_key_and_resetting_drops_the_override() {
        let mut o = Overrides::new();
        let ctrl_d = Binding::parse("Ctrl+D").unwrap();
        let losers = assign(&mut o, "split", vec![ctrl_d]);
        assert_eq!(losers, ["Select none"]);
        let k = Keymap::new(&o);
        assert_eq!(k.action(ctrl_d), Some(Action::Split));
        assert_eq!(k.action(Binding::parse("S").unwrap()), None);
        assert_eq!(
            k.bindings(Action::SelectNone).collect::<Vec<_>>(),
            [Binding::parse("Escape").unwrap()],
            "Select none keeps Esc"
        );
        // Back to the defaults: no override left for Split.
        assign(&mut o, "split", vec![Binding::parse("S").unwrap()]);
        assert!(!o.contains_key("split"));
    }

    #[test]
    fn removed_shortcuts_stay_removed() {
        let mut o = Overrides::new();
        assign(&mut o, "fade_in", vec![]);
        let k = Keymap::new(&o);
        assert_eq!(k.label(Action::FadeIn), "");
        assert_eq!(k.action(Binding::parse("I").unwrap()), None);
    }
}
