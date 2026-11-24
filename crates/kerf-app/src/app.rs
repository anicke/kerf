//! Application state, actions, menus, dialogs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use kerf_core::edit::{self, Clipboard, NormalizeMode, NormalizeSettings, PasteMode};
use kerf_core::io::{ExportSettings, SampleFormat};
use kerf_core::render::{self, RenderOptions};
use kerf_core::session::{self, PROJECT_EXTENSION};
use kerf_core::util::format_time;
use kerf_core::{
    AudioCache, ClipId, Document, FadeShape, Frame, MarkerId, Project, Samples, TrackId,
};

use crate::audio::Player;
use crate::jobs::{Job, JobDone, Prepared};

#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Explicitly selected tracks; empty means "all tracks".
    pub tracks: Vec<TrackId>,
    /// Time range (start < end).
    pub range: Option<(Frame, Frame)>,
    pub clip: Option<ClipId>,
}

pub struct View {
    /// Zoom: samples per pixel (< 1 shows individual samples).
    pub spp: f64,
    /// Frame at the left edge of the lanes.
    pub scroll: f64,
    pub vscroll: f32,
    /// Height of tracks without a height of their own.
    pub track_height: f32,
    /// Tracks resized individually.
    pub track_heights: HashMap<TrackId, f32>,
    pub snap: bool,
    /// Lane width in points, measured each frame.
    pub lane_width: f32,
    /// Height of the track area in points, measured each frame.
    pub lane_height: f32,
    /// Vertical (amplitude) zoom factor for waveforms.
    pub vzoom: f32,
    /// Draw waveforms on a dB scale (quiet detail made visible).
    pub db_scale: bool,
    /// Length of the material in frames (at least a second), set each
    /// frame; zoom and scroll stay within it.
    pub content: f64,
}

pub const DEFAULT_TRACK_H: f32 = 120.0;
pub const MIN_TRACK_H: f32 = 40.0;
pub const MAX_TRACK_H: f32 = 1200.0;

impl View {
    pub fn height_of(&self, id: TrackId) -> f32 {
        self.track_heights
            .get(&id)
            .copied()
            .unwrap_or(self.track_height)
    }

    pub fn set_height(&mut self, id: TrackId, h: f32) {
        self.track_heights
            .insert(id, h.clamp(MIN_TRACK_H, MAX_TRACK_H));
    }

    /// Top of each track, measured from the top of the first, followed by
    /// the total height of all tracks (so `tops.len() == tracks + 1`).
    pub fn track_tops(&self, p: &Project) -> Vec<f32> {
        let mut tops = Vec::with_capacity(p.tracks.len() + 1);
        let mut y = 0.0;
        tops.push(y);
        for t in &p.tracks {
            y += self.height_of(t.id);
            tops.push(y);
        }
        tops
    }

    /// Scale the height of the given tracks.
    pub fn resize_tracks(&mut self, ids: &[TrackId], factor: f32) {
        for &id in ids {
            self.set_height(id, self.height_of(id) * factor);
        }
    }

    /// Give every track the same height, filling the visible track area.
    pub fn fit_tracks(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.track_heights.clear();
        self.track_height = (self.lane_height / n as f32).clamp(MIN_TRACK_H, MAX_TRACK_H);
        self.vscroll = 0.0;
    }

    pub fn reset_track_heights(&mut self) {
        self.track_heights.clear();
        self.track_height = DEFAULT_TRACK_H;
    }
}

impl Default for View {
    fn default() -> Self {
        Self {
            spp: 512.0,
            scroll: 0.0,
            vscroll: 0.0,
            track_height: DEFAULT_TRACK_H,
            track_heights: HashMap::new(),
            snap: true,
            lane_width: 1000.0,
            lane_height: 400.0,
            vzoom: 1.0,
            db_scale: false,
            content: 48_000.0,
        }
    }
}

impl View {
    pub const MIN_SPP: f64 = 1.0 / 64.0;
    /// Room shown after the end of the material, as a part of the view.
    pub const END_MARGIN: f64 = 0.1;

    /// Most zoomed out: all the material plus the end margin fills the view.
    pub fn max_spp(&self) -> f64 {
        (self.content / ((1.0 - Self::END_MARGIN) * self.lane_width.max(1.0) as f64))
            .max(Self::MIN_SPP)
    }

    /// Furthest scroll: the end of the material at the end margin.
    pub fn max_scroll(&self) -> f64 {
        (self.content - (1.0 - Self::END_MARGIN) * self.visible_frames()).max(0.0)
    }

    /// The length the scrollbar represents.
    pub fn scroll_extent(&self) -> f64 {
        (self.max_scroll() + self.visible_frames()).max(1.0)
    }

    /// Keep zoom and scroll within the material.
    pub fn clamp(&mut self) {
        self.spp = self.spp.clamp(Self::MIN_SPP, self.max_spp());
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    /// Zoom by `factor` keeping the frame under lane x-offset `anchor_px` fixed
    /// (as far as the limits allow).
    pub fn zoom(&mut self, factor: f64, anchor_px: f32) {
        let anchor_frame = self.scroll + anchor_px as f64 * self.spp;
        self.spp = (self.spp * factor).clamp(Self::MIN_SPP, self.max_spp());
        self.scroll = anchor_frame - anchor_px as f64 * self.spp;
        self.clamp();
    }

    pub fn show_range(&mut self, start: Frame, end: Frame) {
        let len = end.saturating_sub(start).max(1) as f64;
        self.spp = (len / (self.lane_width as f64 * 0.96)).clamp(Self::MIN_SPP, self.max_spp());
        self.scroll = start as f64 - self.lane_width as f64 * 0.02 * self.spp;
        self.clamp();
    }

    pub fn visible_frames(&self) -> f64 {
        self.lane_width as f64 * self.spp
    }
}

#[cfg(test)]
mod view_tests {
    use super::View;

    /// A 1000-pixel view of 60 s at 48 kHz.
    fn view() -> View {
        View {
            lane_width: 1000.0,
            content: 60.0 * 48_000.0,
            ..View::default()
        }
    }

    #[test]
    fn zooming_out_stops_when_everything_fits() {
        let mut v = view();
        for _ in 0..40 {
            v.zoom(2.0, 500.0);
        }
        assert_eq!(v.spp, v.max_spp());
        assert_eq!(v.scroll, 0.0);
        // All 60 s are visible, with the end 90% of the way across.
        assert!((v.visible_frames() * 0.9 - v.content).abs() < 1.0);
    }

    #[test]
    fn scrolling_stops_at_the_end_margin() {
        let mut v = view();
        v.spp = 512.0;
        v.scroll = 1e12; // flung far past the end
        v.clamp();
        let end_x = (v.content - v.scroll) / v.spp;
        assert!((end_x - 900.0).abs() < 1e-6, "end at {end_x} px");
        v.scroll = -5.0;
        v.clamp();
        assert_eq!(v.scroll, 0.0);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_mouse_where_possible() {
        let mut v = view();
        v.spp = 512.0;
        v.scroll = 100_000.0;
        let under = v.scroll + 300.0 * v.spp;
        v.zoom(0.5, 300.0);
        assert!((v.scroll + 300.0 * v.spp - under).abs() < 1e-6);
        // Zooming out near the end can't leave the material behind.
        v.scroll = v.max_scroll();
        for _ in 0..5 {
            v.zoom(1.5, 990.0);
            assert!(v.scroll <= v.max_scroll() && v.scroll >= 0.0);
        }
    }

    #[test]
    fn fit_and_selection_zoom_stay_in_bounds() {
        let mut v = view();
        v.show_range(0, 60 * 48_000);
        assert_eq!(v.scroll, 0.0);
        assert!(v.spp <= v.max_spp());
        v.show_range(59 * 48_000, 60 * 48_000);
        assert!(v.scroll <= v.max_scroll());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKind {
    Mix,
    Selection,
    Markers,
    Stems,
    CueSheet,
}

impl ExportKind {
    fn title(self) -> &'static str {
        match self {
            Self::Mix => "Export Mix",
            Self::Selection => "Export Selection",
            Self::Markers => "Export Split at Markers",
            Self::Stems => "Export Tracks as Stems",
            Self::CueSheet => "Export Mix with CUE Sheet",
        }
    }
}

pub enum Dialog {
    Normalize(NormalizeSettings),
    Gain(f64),
    InsertSilence(f64),
    Export(ExportKind),
    RenameMarker(MarkerId, String),
    ConfirmDiscard(Pending),
    AudioCache(CacheView),
    DetectSongs(Box<crate::detect_ui::DetectState>),
    Preferences(Box<PrefsDraft>),
    Album(Box<crate::album_ui::AlbumState>),
    Recover(Vec<crate::recovery::Orphan>),
    Recent(crate::recent::QuickOpen),
    About,
}

/// Preferences being edited, plus the devices available to choose from.
pub struct PrefsDraft {
    prefs: crate::prefs::Prefs,
    devices: Vec<(String, String)>,
    /// Text of the custom FFmpeg path field.
    ffmpeg_path: String,
    /// What the FFmpeg setting finds, refreshed when it changes.
    ffmpeg_status: Option<(kerf_core::decode::FfmpegSetting, String)>,
    /// Assistant folders, one per line.
    assistant_folders: String,
}

/// Snapshot shown in the Audio Cache dialog.
pub struct CacheView {
    stats: Option<kerf_core::cache::CacheStats>,
    note: Option<String>,
}

#[derive(Clone)]
pub enum Pending {
    New,
    Open(Option<PathBuf>),
    Quit,
}

/// Every user command, so menus, shortcuts and context menus share one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    New,
    Open,
    OpenRecent,
    Import,
    Save,
    SaveAs,
    Export(ExportKind),
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    Silence,
    Split,
    Crop,
    InsertSilence,
    SelectAll,
    SelectNone,
    Normalize,
    Gain,
    FadeIn,
    FadeOut,
    RemoveFades,
    Bounce,
    AddMarker,
    PrevMarker,
    NextMarker,
    AddTrack,
    RemoveTracks,
    PlayStop,
    StopAtPlayhead,
    PreviewCursor,
    /// L / J: play forwards / backwards, faster with each press.
    ShuttleForward,
    ShuttleReverse,
    /// The same at half speed, while K is held.
    ShuttleSlowForward,
    ShuttleSlowReverse,
    /// K: stop shuttling at the playhead.
    ShuttleStop,
    CursorLeft,
    CursorRight,
    ExtendLeft,
    ExtendRight,
    PrevEditPoint,
    NextEditPoint,
    ExtendPrevEditPoint,
    ExtendNextEditPoint,
    TrackUp,
    TrackDown,
    ExtendTrackUp,
    ExtendTrackDown,
    PageLeft,
    PageRight,
    GoSelectionStart,
    GoSelectionEnd,
    ExtendToStart,
    ExtendToEnd,
    ToggleMarkersPanel,
    ToggleHistoryPanel,
    ToggleStatsPanel,
    Preferences,
    Album,
    ZeroCrossings,
    ToggleZeroCrossingSnap,
    SelectSong,
    VZoomIn,
    VZoomOut,
    VZoomReset,
    TallerTracks,
    ShorterTracks,
    FitTracks,
    ResetTrackHeights,
    ToggleDbScale,
    GoStart,
    GoEnd,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomSelection,
    ToggleLoop,
    ToggleSnap,
    FollowPage,
    FollowCenter,
    AudioCache,
    DetectSongs,
    SnapMarkersToSectors,
    About,
}

pub struct KerfApp {
    pub doc: Document,
    pub view: View,
    pub sel: Selection,
    pub cursor: Frame,
    pub clipboard: Option<Clipboard>,
    pub player: Player,
    pub play_revision: u64,
    pub loop_playback: bool,
    /// J or L, while held with K for half-speed play.
    shuttle_hold: Option<Key>,
    pub fade_shape: FadeShape,
    pub paste_mode: PasteMode,
    pub export_settings: ExportSettings,
    /// Snap split/CUE track boundaries to CD sectors (no sector boundary errors).
    pub cd_safe: bool,
    pub status: Option<(String, bool)>,
    pub dialog: Option<Dialog>,
    pub job: Option<Job>,
    pub drag: Option<crate::timeline::Drag>,
    /// AI assistant access (MCP), when turned on in Preferences.
    pub assistant: Option<crate::assistant::Assistant>,
    pub meters: [f32; 2],
    pub stats: crate::stats_panel::Stats,
    pub last_dir: Option<PathBuf>,
    /// Disk cache for decoded audio; `None` falls back to keeping audio in RAM.
    pub cache: Option<Arc<AudioCache>>,
    title: String,
    allow_close: bool,
    screenshot: Option<ScreenshotHook>,
    pub prefs: crate::prefs::Prefs,
    recovery: Option<crate::recovery::Recovery>,
    /// Text of the time field being typed into.
    time_edit: Option<(egui::Id, String)>,
}

impl KerfApp {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_theme(egui::Theme::Dark);
        let cache = match AudioCache::open_default() {
            Ok(c) => {
                let c = Arc::new(c);
                let pruner = c.clone();
                // Housekeeping: drop cache entries unused for 30 days.
                std::thread::spawn(move || {
                    pruner.prune(std::time::Duration::from_secs(30 * 86_400))
                });
                Some(c)
            }
            Err(e) => {
                eprintln!("audio cache unavailable, keeping audio in memory: {e}");
                None
            }
        };
        let mut app = Self {
            doc: Document::new(Project {
                cache: cache.clone(),
                ..Project::default()
            }),
            view: View::default(),
            sel: Selection::default(),
            cursor: 0,
            clipboard: None,
            player: Player::default(),
            play_revision: 0,
            loop_playback: false,
            shuttle_hold: None,
            fade_shape: FadeShape::EqualPower,
            paste_mode: PasteMode::Insert,
            export_settings: ExportSettings::default(),
            cd_safe: true,
            status: None,
            dialog: None,
            job: None,
            drag: None,
            assistant: None,
            meters: [0.0; 2],
            stats: Default::default(),
            last_dir: None,
            cache,
            title: String::new(),
            allow_close: false,
            screenshot: std::env::var_os("KERF_SCREENSHOT").map(|p| ScreenshotHook {
                path: p.into(),
                frames: 0,
                after: std::env::var("KERF_SCREENSHOT_AFTER")
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| std::time::Instant::now() + std::time::Duration::from_secs_f64(s)),
                done: Default::default(),
            }),
            prefs: crate::prefs::Prefs::load(),
            recovery: None,
            time_edit: None,
        };
        app.player
            .configure(app.prefs.output_device.clone(), app.prefs.buffer_frames);
        kerf_core::decode::configure(&app.prefs.ffmpeg);
        if app.prefs.assistant_enabled {
            app.set_assistant(&cc.egui_ctx, true);
        }
        app.recovery = crate::recovery::Recovery::new(app.prefs.autosave_secs);
        let orphans = app
            .recovery
            .as_ref()
            .map(|r| r.orphans())
            .unwrap_or_default();
        if !orphans.is_empty() {
            app.dialog = Some(Dialog::Recover(orphans));
        }
        app.open_paths(ctx, files);
        app
    }

    pub fn project(&self) -> &Project {
        self.doc.project()
    }

    /// Tracks an edit applies to, in project order.
    pub fn target_tracks(&self) -> Vec<TrackId> {
        let p = self.project();
        if self.sel.tracks.is_empty() {
            p.all_track_ids()
        } else {
            p.tracks
                .iter()
                .map(|t| t.id)
                .filter(|id| self.sel.tracks.contains(id))
                .collect()
        }
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false));
    }

    pub fn error(&mut self, msg: impl std::fmt::Display) {
        self.status = Some((msg.to_string(), true));
    }

    /// Run an undoable edit, reporting errors in the status bar.
    pub fn edit<T>(
        &mut self,
        label: &str,
        f: impl FnOnce(&mut Project) -> kerf_core::Result<T>,
    ) -> Option<T> {
        match self.doc.apply(label, f) {
            Ok(v) => {
                self.validate_selection();
                Some(v)
            }
            Err(e) => {
                self.error(e);
                None
            }
        }
    }

    /// Drop selection entries that no longer exist (after undo, delete, …).
    pub fn validate_selection(&mut self) {
        let p = self.doc.project();
        let tracks: Vec<TrackId> = self
            .sel
            .tracks
            .iter()
            .copied()
            .filter(|t| p.track(*t).is_some())
            .collect();
        let clip_ok = self.sel.clip.is_some_and(|c| p.clip(c).is_some());
        self.sel.tracks = tracks;
        if !clip_ok {
            self.sel.clip = None;
        }
    }

    fn require_range(&mut self) -> Option<(Frame, Frame)> {
        let r = self.sel.range.filter(|(a, b)| b > a);
        if r.is_none() {
            self.error("Select a time range first (drag across a track)");
        }
        r
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    pub fn perform(&mut self, ctx: &egui::Context, action: Action) {
        use Action::*;
        match action {
            New => self.guard_discard(ctx, Pending::New),
            Open => self.guard_discard(ctx, Pending::Open(None)),
            OpenRecent => self.dialog = Some(Dialog::Recent(Default::default())),
            Import => {
                let mut dlg = rfd::FileDialog::new()
                    .add_filter("Audio", kerf_core::io::IMPORT_EXTENSIONS)
                    .add_filter("All files", &["*"]);
                if let Some(d) = &self.last_dir {
                    dlg = dlg.set_directory(d);
                }
                if let Some(files) = dlg.pick_files() {
                    self.import(ctx, files);
                }
            }
            Save => {
                self.save(false);
            }
            SaveAs => {
                self.save(true);
            }
            Export(kind) => {
                if kind == ExportKind::Selection && self.require_range().is_none() {
                    return;
                }
                if self.project().length() == 0 {
                    self.error("Nothing to export");
                    return;
                }
                self.dialog = Some(Dialog::Export(kind));
            }
            Quit => self.guard_discard(ctx, Pending::Quit),
            Undo => match self.doc.undo() {
                Some(l) => {
                    self.validate_selection();
                    self.info(format!("Undid {l}"));
                }
                None => self.info("Nothing to undo"),
            },
            Redo => match self.doc.redo() {
                Some(l) => {
                    self.validate_selection();
                    self.info(format!("Redid {l}"));
                }
                None => self.info("Nothing to redo"),
            },
            Cut => {
                let Some((a, b)) = self.require_range() else {
                    return;
                };
                let tracks = self.target_tracks();
                if let Ok(cb) = edit::copy(self.project(), &tracks, a, b) {
                    self.clipboard = Some(cb);
                    self.edit("Cut", |p| edit::ripple_delete(p, &tracks, a, b));
                    self.sel.range = None;
                    self.cursor = a;
                }
            }
            Copy => {
                let Some((a, b)) = self.require_range() else {
                    return;
                };
                match edit::copy(self.project(), &self.target_tracks(), a, b) {
                    Ok(cb) => {
                        self.info(format!(
                            "Copied {}",
                            format_time(cb.length, self.project().sample_rate)
                        ));
                        self.clipboard = Some(cb);
                    }
                    Err(e) => self.error(e),
                }
            }
            Paste => {
                let Some(cb) = self.clipboard.clone() else {
                    self.error("Clipboard is empty");
                    return;
                };
                let tracks = self.target_tracks();
                let range = self.sel.range.filter(|(a, b)| b > a);
                let pos = range.map_or(self.cursor, |r| r.0);
                let mode = self.paste_mode;
                let done = self.edit("Paste", |p| {
                    // Pasting over a selection replaces it.
                    if let Some((a, b)) = range {
                        edit::ripple_delete(p, &tracks, a, b)?;
                    }
                    edit::paste(p, &cb, &tracks, pos, mode)
                });
                if done.is_some() {
                    self.sel.range = Some((pos, pos + cb.length));
                    self.cursor = pos;
                }
            }
            Delete => {
                if let Some((a, b)) = self.sel.range.filter(|(a, b)| b > a) {
                    let tracks = self.target_tracks();
                    self.edit("Delete", |p| edit::ripple_delete(p, &tracks, a, b));
                    self.sel.range = None;
                    self.cursor = a;
                } else if let Some(c) = self.sel.clip {
                    self.edit("Delete clip", |p| edit::remove_clips(p, &[c]));
                } else {
                    self.error("Select a range or a clip to delete");
                }
            }
            Silence => {
                let Some((a, b)) = self.require_range() else {
                    return;
                };
                let tracks = self.target_tracks();
                self.edit("Silence", |p| edit::clear_range(p, &tracks, a, b));
            }
            Split => {
                let tracks = self.target_tracks();
                let points: Vec<Frame> = match self.sel.range.filter(|(a, b)| b > a) {
                    // (selection edges are already on zero crossings when snapping is on)
                    Some((a, b)) => vec![a, b],
                    None => {
                        let at = if self.player.is_playing() {
                            self.player.position()
                        } else {
                            self.cursor
                        };
                        vec![self.zero_snap(at)]
                    }
                };
                self.edit("Split", |p| {
                    points
                        .iter()
                        .try_for_each(|&pos| edit::split_at(p, &tracks, pos))
                });
            }
            Crop => {
                let Some((a, b)) = self.require_range() else {
                    return;
                };
                let tracks = self.target_tracks();
                self.edit("Trim to selection", |p| edit::crop(p, &tracks, a, b));
            }
            InsertSilence => self.dialog = Some(Dialog::InsertSilence(1.0)),
            SelectAll => {
                self.sel.tracks.clear();
                self.sel.range = Some((0, self.project().length()));
            }
            SelectNone => {
                self.sel = Selection::default();
            }
            Normalize => {
                if self.project().tracks.is_empty() {
                    self.error("Nothing to normalize");
                    return;
                }
                self.dialog = Some(Dialog::Normalize(NormalizeSettings::default()));
            }
            Gain => self.dialog = Some(Dialog::Gain(0.0)),
            FadeIn | FadeOut => {
                let Some((a, b)) = self.require_range() else {
                    return;
                };
                let tracks = self.target_tracks();
                let shape = self.fade_shape;
                if action == FadeIn {
                    self.edit("Fade in", |p| edit::fade_in(p, &tracks, a, b, shape));
                } else {
                    self.edit("Fade out", |p| edit::fade_out(p, &tracks, a, b, shape));
                }
            }
            RemoveFades => {
                let (a, b) = self.sel.range.unwrap_or((0, Frame::MAX));
                let tracks = self.target_tracks();
                self.edit("Remove fades", |p| edit::remove_fades(p, &tracks, a, b));
            }
            Bounce => {
                let tracks = self.target_tracks();
                let range = self.sel.range.filter(|(a, b)| b > a);
                self.edit("Render tracks", |p| {
                    for t in &tracks {
                        if p.track(*t).is_some_and(|t| !t.clips.is_empty()) {
                            edit::bounce_track(p, *t, range)?;
                        }
                    }
                    Ok(())
                });
            }
            AddMarker => {
                let pos = if self.player.is_playing() {
                    self.player.position()
                } else {
                    self.cursor
                };
                let n = self.project().markers.len() + 1;
                self.edit("Add marker", |p| {
                    Ok(p.add_marker(pos, format!("Marker {n}")))
                });
            }
            PrevMarker => {
                let target = self
                    .project()
                    .markers
                    .iter()
                    .rev()
                    .find(|m| m.position < self.cursor)
                    .map(|m| m.position);
                self.cursor = target.unwrap_or(0);
                self.sel.range = None;
            }
            NextMarker => {
                if let Some(m) = self
                    .project()
                    .markers
                    .iter()
                    .find(|m| m.position > self.cursor)
                {
                    self.cursor = m.position;
                    self.sel.range = None;
                }
            }
            AddTrack => {
                let n = self.project().tracks.len() + 1;
                if let Some(id) = self.edit("Add track", |p| Ok(p.add_track(format!("Track {n}"))))
                {
                    self.sel.tracks = vec![id];
                }
            }
            RemoveTracks => {
                if self.sel.tracks.is_empty() {
                    self.error("Select the tracks to remove (click a track header)");
                    return;
                }
                let tracks = self.sel.tracks.clone();
                self.edit("Remove tracks", |p| edit::remove_tracks(p, &tracks));
            }
            PlayStop => self.play_stop(),
            StopAtPlayhead => {
                if self.player.is_playing() {
                    // The opposite of what Space does.
                    if self.prefs.space_returns_to_start {
                        self.stop_at_playhead();
                    } else {
                        self.player.stop();
                    }
                } else {
                    self.play_stop();
                }
            }
            ShuttleForward => self.shuttle(1.0, false),
            ShuttleReverse => self.shuttle(-1.0, false),
            ShuttleSlowForward => self.shuttle(1.0, true),
            ShuttleSlowReverse => self.shuttle(-1.0, true),
            ShuttleStop => {
                if self.player.is_playing() && !self.player.is_scrubbing() {
                    self.stop_at_playhead();
                }
            }
            PreviewCursor => {
                let sr = self.project().sample_rate as f64;
                let before = (self.prefs.preview_before * sr) as Frame;
                let after = (self.prefs.preview_after * sr) as Frame;
                let end = (self.cursor + after).min(self.project().length());
                let start = self.cursor.saturating_sub(before);
                if end > start {
                    self.start_playback(start, end, None);
                }
            }
            CursorLeft | CursorRight => {
                let forward = action == CursorRight;
                if self.player.is_playing() {
                    // While playing, arrows skip 5 seconds.
                    let jump = self.project().sample_rate as Frame * 5;
                    let pos = self.player.position();
                    self.seek(if forward {
                        pos + jump
                    } else {
                        pos.saturating_sub(jump)
                    });
                } else {
                    let step = crate::nav::nudge_step(self.view.spp);
                    let to = if forward {
                        self.cursor + step
                    } else {
                        self.cursor.saturating_sub(step)
                    };
                    self.sel.range = None;
                    self.cursor = to;
                    self.ensure_visible(to);
                }
            }
            ExtendLeft | ExtendRight => {
                let step = crate::nav::nudge_step(self.view.spp);
                let to = if action == ExtendRight {
                    self.cursor + step
                } else {
                    self.cursor.saturating_sub(step)
                };
                self.extend_selection(to);
            }
            PrevEditPoint | NextEditPoint | ExtendPrevEditPoint | ExtendNextEditPoint => {
                let edges: Vec<Frame> = self.sel.range.map(|(a, b)| vec![a, b]).unwrap_or_default();
                let points = crate::nav::edit_points(self.project(), &edges);
                let from = if self.player.is_playing() {
                    self.player.position()
                } else {
                    self.cursor
                };
                let target = if matches!(action, PrevEditPoint | ExtendPrevEditPoint) {
                    crate::nav::prev_point(&points, from)
                } else {
                    crate::nav::next_point(&points, from)
                };
                if let Some(t) = target {
                    if matches!(action, ExtendPrevEditPoint | ExtendNextEditPoint) {
                        self.extend_selection(t);
                    } else {
                        self.sel.range = None;
                        self.seek(t);
                        self.ensure_visible(t);
                    }
                }
            }
            TrackUp | TrackDown | ExtendTrackUp | ExtendTrackDown => {
                let n = self.project().tracks.len();
                if n > 0 {
                    let up = matches!(action, TrackUp | ExtendTrackUp);
                    let current = self
                        .sel
                        .tracks
                        .last()
                        .and_then(|t| self.project().track_index(*t));
                    let i = match current {
                        Some(i) if up => i.saturating_sub(1),
                        Some(i) => (i + 1).min(n - 1),
                        None if up => n - 1,
                        None => 0,
                    };
                    let id = self.project().tracks[i].id;
                    if matches!(action, ExtendTrackUp | ExtendTrackDown) {
                        self.sel.tracks.retain(|t| *t != id);
                        self.sel.tracks.push(id);
                    } else {
                        self.sel.tracks = vec![id];
                    }
                    // Keep the track in view.
                    let tops = self.view.track_tops(self.project());
                    let (top, bottom) = (tops[i], tops[i + 1]);
                    if top < self.view.vscroll {
                        self.view.vscroll = top;
                    } else if bottom > self.view.vscroll + self.view.lane_height {
                        self.view.vscroll = (bottom - self.view.lane_height).min(top);
                    }
                }
            }
            PageLeft => {
                self.view.scroll = (self.view.scroll - self.view.visible_frames() * 0.9).max(0.0)
            }
            PageRight => self.view.scroll += self.view.visible_frames() * 0.9,
            GoSelectionStart | GoSelectionEnd => match self.sel.range {
                Some((a, b)) => {
                    let to = if action == GoSelectionStart { a } else { b };
                    self.cursor = to;
                    self.ensure_visible(to);
                }
                None => self.info("No selection"),
            },
            ExtendToStart => self.extend_selection(0),
            ExtendToEnd => {
                let end = self.project().length();
                self.extend_selection(end);
            }
            ToggleMarkersPanel => {
                self.prefs.show_markers_panel = !self.prefs.show_markers_panel;
                let _ = self.prefs.save();
            }
            ToggleHistoryPanel => {
                self.prefs.show_history_panel = !self.prefs.show_history_panel;
                let _ = self.prefs.save();
            }
            ToggleStatsPanel => {
                self.prefs.show_stats_panel = !self.prefs.show_stats_panel;
                let _ = self.prefs.save();
            }
            ZeroCrossings => {
                let tracks = self.target_tracks();
                let max = self.project().sample_rate as Frame / 50; // 20 ms
                let z = |app: &Self, f| {
                    kerf_core::render::nearest_zero_crossing(app.project(), &tracks, f, max)
                };
                match self.sel.range {
                    Some((a, b)) => {
                        let (za, zb) = (z(self, a), z(self, b));
                        self.sel.range = (zb > za).then_some((za, zb));
                        self.cursor = if self.cursor == a { za } else { zb };
                    }
                    None => self.cursor = z(self, self.cursor),
                }
            }
            ToggleZeroCrossingSnap => {
                self.prefs.snap_zero_crossings = !self.prefs.snap_zero_crossings;
                let _ = self.prefs.save();
                let state = if self.prefs.snap_zero_crossings {
                    "on"
                } else {
                    "off"
                };
                self.info(format!("Snap to zero crossings {state}"));
            }
            SelectSong => self.select_between_markers(self.cursor),
            VZoomIn => self.view.vzoom = (self.view.vzoom * 2.0).min(256.0),
            VZoomOut => self.view.vzoom = (self.view.vzoom / 2.0).max(1.0),
            VZoomReset => self.view.vzoom = 1.0,
            TallerTracks | ShorterTracks => {
                let ids = if self.sel.tracks.is_empty() {
                    self.project().all_track_ids()
                } else {
                    self.sel.tracks.clone()
                };
                let factor = if action == TallerTracks { 1.25 } else { 0.8 };
                self.view.resize_tracks(&ids, factor);
            }
            FitTracks => self.view.fit_tracks(self.project().tracks.len()),
            ResetTrackHeights => self.view.reset_track_heights(),
            ToggleDbScale => self.view.db_scale = !self.view.db_scale,
            Album => self.dialog = Some(Dialog::Album(Box::default())),
            Preferences => {
                let ffmpeg_path = match &self.prefs.ffmpeg {
                    kerf_core::decode::FfmpegSetting::Path(p) => p.display().to_string(),
                    _ => String::new(),
                };
                self.dialog = Some(Dialog::Preferences(Box::new(PrefsDraft {
                    prefs: self.prefs.clone(),
                    devices: crate::audio::output_devices(),
                    ffmpeg_path,
                    ffmpeg_status: None,
                    assistant_folders: self
                        .prefs
                        .assistant_folders
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                })));
            }
            GoStart => self.seek(0),
            GoEnd => {
                let end = self.project().length();
                self.seek(end);
            }
            ZoomIn | ZoomOut | ZoomFit | ZoomSelection => {
                // The project may have just changed (opened, imported).
                self.update_view_content();
                self.zoom_action(action);
            }
            ToggleLoop => self.loop_playback = !self.loop_playback,
            ToggleSnap => self.view.snap = !self.view.snap,
            FollowPage | FollowCenter => {
                use crate::prefs::FollowMode;
                let mode = if action == FollowPage {
                    FollowMode::Page
                } else {
                    FollowMode::Center
                };
                // Clicking the active mode turns following off.
                self.set_follow(if self.prefs.follow == mode {
                    FollowMode::Off
                } else {
                    mode
                });
            }
            DetectSongs => self.detect_songs(ctx),
            SnapMarkersToSectors => {
                if self
                    .edit("Snap markers to CD sectors", edit::snap_markers_to_sectors)
                    .is_some()
                {
                    self.info("Markers moved to the nearest CD sector boundary (1/75 s)");
                }
            }
            AudioCache => {
                self.dialog = Some(Dialog::AudioCache(CacheView {
                    stats: None,
                    note: None,
                }));
                self.refresh_cache_view(None);
            }
            About => self.dialog = Some(Dialog::About),
        }
    }

    // -----------------------------------------------------------------------
    // Transport
    // -----------------------------------------------------------------------

    pub fn play_stop(&mut self) {
        if self.player.is_playing() {
            if self.prefs.space_returns_to_start {
                self.player.stop();
            } else {
                self.stop_at_playhead();
            }
            return;
        }
        let len = self.project().length();
        let (start, end, loop_start) = match self.sel.range.filter(|(a, b)| b > a) {
            Some((a, b)) => (a, b, self.loop_playback.then_some(a)),
            None => {
                let start = if self.cursor >= len { 0 } else { self.cursor };
                (start, len, self.loop_playback.then_some(0))
            }
        };
        if end <= start {
            self.info("Nothing to play");
            return;
        }
        self.start_playback(start, end, loop_start);
    }

    fn start_playback(&mut self, start: Frame, end: Frame, loop_start: Option<Frame>) {
        let snapshot = Arc::new(self.project().clone());
        self.play_revision = self.doc.revision();
        if let Err(e) = self.player.play(snapshot, start, end, loop_start) {
            self.error(format!("Playback failed: {e}"));
        }
    }

    /// J / L shuttle: start in direction `dir` (±1) at normal speed, or at
    /// half speed when `slow`; pressing again in the same direction doubles
    /// the speed (up to 8×), the other direction starts over at 1×.
    fn shuttle(&mut self, dir: f64, slow: bool) {
        let playing = self.player.is_playing() && !self.player.is_scrubbing();
        let current = if playing { self.player.speed() } else { 0.0 };
        let speed = if slow {
            0.5 * dir
        } else if current * dir > 0.0 {
            (current.abs() * 2.0).clamp(1.0, MAX_SHUTTLE_SPEED) * dir
        } else {
            dir
        };
        let from = if playing {
            self.player.position()
        } else {
            self.cursor
        };
        self.start_shuttle(from, speed);
    }

    fn start_shuttle(&mut self, from: Frame, speed: f64) {
        let len = self.project().length();
        if (speed > 0.0 && from >= len) || (speed < 0.0 && from == 0) {
            if self.player.is_playing() {
                self.stop_at_playhead();
            }
            return;
        }
        let snapshot = Arc::new(self.project().clone());
        self.play_revision = self.doc.revision();
        if let Err(e) = self.player.shuttle(snapshot, from, speed) {
            self.error(format!("Playback failed: {e}"));
        }
    }

    /// Start scrubbing at `at` (dragging the playhead).
    pub fn begin_scrub(&mut self, at: Frame) -> crate::timeline::Drag {
        let resume = self.player.is_playing() && !self.player.is_scrubbing();
        let at = at.min(self.project().length());
        let snapshot = Arc::new(self.project().clone());
        self.play_revision = self.doc.revision();
        if let Err(e) = self.player.scrub(snapshot, at) {
            self.error(format!("Playback failed: {e}"));
        }
        self.sel.range = None;
        self.cursor = at;
        crate::timeline::Drag::Scrub { resume }
    }

    pub fn scrub_to(&mut self, at: Frame) {
        let at = at.min(self.project().length());
        self.cursor = at;
        self.player.scrub_to(at);
    }

    /// Stop scrubbing; carry on playing from there if playback was running.
    pub fn end_scrub(&mut self, resume: bool) {
        self.player.stop();
        if resume {
            self.play_stop();
        }
    }

    /// `f` moved to the nearest zero crossing when that snapping is on.
    pub fn zero_snap(&self, f: Frame) -> Frame {
        if !self.prefs.snap_zero_crossings {
            return f;
        }
        let max = self.project().sample_rate as Frame / 50; // 20 ms
        kerf_core::render::nearest_zero_crossing(self.project(), &self.target_tracks(), f, max)
    }

    /// After a selection gesture: put its edges on zero crossings if enabled.
    pub fn finish_selection(&mut self) {
        if let (true, Some((a, b))) = (self.prefs.snap_zero_crossings, self.sel.range) {
            let (za, zb) = (self.zero_snap(a), self.zero_snap(b));
            self.cursor = if self.cursor == a { za } else { zb };
            self.sel.range = (zb > za).then_some((za, zb));
        }
    }

    /// Select from the marker at or before `at` to the next marker (a song).
    pub fn select_between_markers(&mut self, at: Frame) {
        let p = self.project();
        let start = p
            .markers
            .iter()
            .rev()
            .find(|m| m.position <= at)
            .map_or(0, |m| m.position);
        let end = p
            .markers
            .iter()
            .find(|m| m.position > at)
            .map_or(p.length(), |m| m.position);
        if end > start {
            self.sel.tracks.clear();
            self.sel.range = Some((start, end));
            self.cursor = start;
        }
    }

    /// Stop and leave the cursor where playback was.
    fn stop_at_playhead(&mut self) {
        let pos = self.player.position();
        self.player.stop();
        self.cursor = pos;
        self.ensure_visible(pos);
    }

    /// Scroll so `frame` is on screen (if it isn't already).
    fn zoom_action(&mut self, action: Action) {
        match action {
            Action::ZoomIn => self.view.zoom(0.5, self.view.lane_width / 2.0),
            Action::ZoomOut => self.view.zoom(2.0, self.view.lane_width / 2.0),
            Action::ZoomFit => {
                let len = self
                    .project()
                    .length()
                    .max(self.project().sample_rate as Frame);
                self.view.show_range(0, len);
            }
            Action::ZoomSelection => {
                if let Some((a, b)) = self.require_range() {
                    self.view.show_range(a, b);
                }
            }
            _ => {}
        }
    }

    pub fn set_follow(&mut self, mode: crate::prefs::FollowMode) {
        self.prefs.follow = mode;
        let _ = self.prefs.save();
    }

    /// Tell the view how long the material is, so zoom and scroll stay
    /// within it (at least a second, and including where a clip is being
    /// dragged or trimmed to).
    pub fn update_view_content(&mut self) {
        use crate::timeline::Drag;
        let p = self.doc.project();
        let dragged_end = match &self.drag {
            Some(Drag::MoveClip { clip, pos, .. }) => p.clip(*clip).map_or(0, |c| pos + c.length),
            Some(Drag::TrimEnd { pos, .. }) => *pos,
            _ => 0,
        };
        self.view.content = (p.length().max(dragged_end) as f64).max(p.sample_rate as f64);
    }

    pub fn ensure_visible(&mut self, frame: Frame) {
        let visible = self.view.visible_frames();
        let f = frame as f64;
        if f < self.view.scroll || f > self.view.scroll + visible {
            self.view.scroll = (f - visible * 0.1).max(0.0);
        }
    }

    /// Move the selection's moving edge (the cursor) to `to`.
    fn extend_selection(&mut self, to: Frame) {
        // Don't extend past the end of the project (unless already beyond it).
        let to = to.min(self.project().length().max(self.cursor));
        let (range, cursor) = crate::nav::extend(self.sel.range, self.cursor, to);
        self.sel.range = range;
        self.cursor = cursor;
        self.ensure_visible(cursor);
    }

    fn apply_prefs(&mut self, ctx: &egui::Context, new: crate::prefs::Prefs) {
        kerf_core::decode::configure(&new.ffmpeg);
        let assistant_changed = new.assistant_enabled != self.assistant.is_some()
            || (new.assistant_enabled && new.assistant_folders != self.prefs.assistant_folders);
        self.player
            .configure(new.output_device.clone(), new.buffer_frames);
        if let Some(r) = &mut self.recovery {
            r.set_interval(new.autosave_secs);
        }
        self.prefs = new;
        if assistant_changed {
            self.set_assistant(ctx, self.prefs.assistant_enabled);
        }
        match self.prefs.save() {
            Ok(()) => self.info("Preferences saved"),
            Err(e) => self.error(format!("Could not save preferences: {e}")),
        }
    }

    /// Measure the album (every track and the whole) in the background.
    fn analyze_album(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let project = self.project().clone();
        let fingerprint = crate::album_ui::AudioFingerprint::of(&project);
        self.job = Some(Job::spawn(ctx, "Measuring album…", move |ctl| {
            let regions = render::regions_from_markers(&project, project.length());
            let r = kerf_core::album::analyze(&project, &regions, &mut |p| ctl.progress(p))
                .map_err(|e| e.to_string());
            JobDone::AlbumAnalyzed(r, fingerprint)
        }));
    }

    fn recover(&mut self, ctx: &egui::Context, orphan: crate::recovery::Orphan) {
        if self.busy() {
            return;
        }
        let cache = self.cache.clone();
        self.job = Some(Job::spawn(ctx, "Recovering unsaved work…", move |_| {
            let r = session::load(&orphan.snapshot, cache).map_err(|e| e.to_string());
            JobDone::Recovered(orphan, r)
        }));
    }

    /// Move the cursor; if playing, continue playing from there.
    pub fn seek(&mut self, frame: Frame) {
        self.cursor = frame;
        if self.player.is_scrubbing() {
            return;
        }
        if self.player.is_playing() && self.player.speed() != 1.0 {
            self.start_shuttle(frame, self.player.speed());
        } else if self.player.is_playing() {
            let len = self.project().length();
            if frame < len {
                self.start_playback(frame, len, self.loop_playback.then_some(0));
            } else {
                self.player.stop();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Files
    // -----------------------------------------------------------------------

    fn guard_discard(&mut self, ctx: &egui::Context, then: Pending) {
        if self.doc.is_dirty() && !self.project().tracks.is_empty() {
            self.dialog = Some(Dialog::ConfirmDiscard(then));
        } else {
            self.do_pending(ctx, then);
        }
    }

    fn do_pending(&mut self, ctx: &egui::Context, p: Pending) {
        match p {
            Pending::New => {
                self.player.stop();
                self.doc = Document::new(Project {
                    cache: self.cache.clone(),
                    ..Project::default()
                });
                self.sel = Selection::default();
                self.cursor = 0;
                self.view = View {
                    lane_width: self.view.lane_width,
                    ..View::default()
                };
            }
            Pending::Open(path) => {
                let path = path.or_else(|| {
                    let mut dlg =
                        rfd::FileDialog::new().add_filter("Kerf project", &[PROJECT_EXTENSION]);
                    if let Some(d) = &self.last_dir {
                        dlg = dlg.set_directory(d);
                    }
                    dlg.pick_file()
                });
                if let Some(path) = path {
                    self.load_project(ctx, path);
                }
            }
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Open project files and import audio files (command line, drag & drop).
    pub fn open_paths(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        let (projects, audio): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION))
        });
        if let Some(p) = projects.into_iter().next() {
            self.guard_discard(ctx, Pending::Open(Some(p)));
        } else if !audio.is_empty() {
            self.import(ctx, audio);
        }
    }

    fn busy(&mut self) -> bool {
        if self.job.is_some() {
            self.error("Please wait for the current task to finish");
            true
        } else {
            false
        }
    }

    pub fn import(&mut self, ctx: &egui::Context, files: Vec<PathBuf>) {
        if self.busy() || files.is_empty() {
            return;
        }
        self.last_dir = files[0].parent().map(Path::to_path_buf);
        let fixed_rate = (!self.project().is_empty()).then_some(self.project().sample_rate);
        let cache = self.cache.clone();
        let n = files.len();
        self.job = Some(Job::spawn(
            ctx,
            format!("Importing {n} file(s)…"),
            move |ctl| {
                // The first file fixes the rate for the rest when the project is empty.
                let mut rate = fixed_rate;
                let mut out = Vec::new();
                for (i, path) in files.into_iter().enumerate() {
                    let mut progress = |p: f64| ctl.progress((i as f64 + p) / n as f64);
                    match session::prepare_audio(cache.as_deref(), &path, rate, &mut progress) {
                        Ok((audio, peaks)) => {
                            rate.get_or_insert(audio.sample_rate());
                            let name = path
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let limitation = kerf_core::io::decode_limitation(&path);
                            out.push(Ok(Prepared {
                                path,
                                name,
                                audio,
                                peaks,
                                limitation,
                            }));
                        }
                        Err(kerf_core::Error::Cancelled) => break,
                        Err(e) => out.push(Err(format!("{}: {e}", path.display()))),
                    }
                    if !ctl.progress((i + 1) as f64 / n as f64) {
                        break;
                    }
                }
                JobDone::Imported(out)
            },
        ));
    }

    fn load_project(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.busy() {
            return;
        }
        self.last_dir = path.parent().map(Path::to_path_buf);
        let cache = self.cache.clone();
        self.job = Some(Job::spawn(ctx, "Opening project…", move |_| {
            let r = session::load(&path, cache).map_err(|e| e.to_string());
            JobDone::Loaded(path, r)
        }));
    }

    pub fn remember_recent(&mut self, path: &Path) {
        crate::recent::remember(&mut self.prefs.recent_projects, path);
        let _ = self.prefs.save();
    }

    fn clear_recent(&mut self) {
        self.prefs.recent_projects.clear();
        let _ = self.prefs.save();
    }

    /// Returns `true` if saved.
    fn save(&mut self, save_as: bool) -> bool {
        let path = match (&self.doc.path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let mut dlg = rfd::FileDialog::new()
                    .add_filter("Kerf project", &[PROJECT_EXTENSION])
                    .set_file_name(format!("Untitled.{PROJECT_EXTENSION}"));
                if let Some(d) = &self.last_dir {
                    dlg = dlg.set_directory(d);
                }
                match dlg.save_file() {
                    Some(mut p) => {
                        if p.extension().is_none() {
                            p.set_extension(PROJECT_EXTENSION);
                        }
                        p
                    }
                    None => return false,
                }
            }
        };
        match session::save(self.project(), &path) {
            Ok(()) => {
                self.remember_recent(&path);
                self.doc.path = Some(path.clone());
                self.doc.mark_saved();
                self.info(format!("Saved {}", path.display()));
                true
            }
            Err(e) => {
                self.error(format!("Save failed: {e}"));
                false
            }
        }
    }

    fn start_export(&mut self, ctx: &egui::Context, kind: ExportKind) {
        if self.busy() {
            return;
        }
        let project = self.project().clone();
        let settings = self.export_settings;
        let base = self
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let len = project.length();
        let range = match kind {
            ExportKind::Selection => match self.require_range() {
                Some(r) => r,
                None => return,
            },
            _ => (0, len),
        };

        let mut dlg = rfd::FileDialog::new();
        if let Some(d) = &self.last_dir {
            dlg = dlg.set_directory(d);
        }
        let target = match kind {
            ExportKind::Mix | ExportKind::Selection | ExportKind::CueSheet => {
                let c = settings.container;
                dlg.add_filter(c.label(), &[c.extension()])
                    .set_file_name(format!("{base}.{}", c.extension()))
                    .save_file()
                    .map(|mut p| {
                        if p.extension().is_none() {
                            p.set_extension(c.extension());
                        }
                        p
                    })
            }
            ExportKind::Markers | ExportKind::Stems => {
                dlg.set_title("Choose output folder").pick_folder()
            }
        };
        let Some(target) = target else { return };
        let regions = self.export_regions_for(&project);

        self.job = Some(Job::spawn(ctx, kind.title(), move |ctl| {
            let mut progress = |p: f64| ctl.progress(p);
            let r = match kind {
                ExportKind::Mix | ExportKind::Selection => render::export_range(
                    &project,
                    range.0,
                    range.1,
                    &RenderOptions::default(),
                    &target,
                    settings,
                    &mut progress,
                )
                .map(|()| vec![target.clone()]),
                ExportKind::Markers => render::export_regions(
                    &project,
                    &regions,
                    &RenderOptions::default(),
                    &target,
                    &base,
                    settings,
                    &mut progress,
                ),
                ExportKind::Stems => {
                    render::export_stems(&project, 0, len, &target, settings, &mut progress)
                }
                ExportKind::CueSheet => render::export_with_cue(
                    &project,
                    &regions,
                    &target,
                    &base,
                    settings,
                    &mut progress,
                ),
            };
            JobDone::Exported(r.map_err(|e| e.to_string()))
        }));
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(done) = self.job.as_ref().and_then(Job::poll) else {
            return;
        };
        self.job = None;
        match done {
            JobDone::Imported(results) => {
                let mut ok = Vec::new();
                let mut errors = Vec::new();
                for r in results {
                    match r {
                        Ok(p) => ok.push(p),
                        Err(e) => errors.push(e),
                    }
                }
                if !ok.is_empty() {
                    let n = ok.len();
                    let warnings: Vec<String> =
                        ok.iter().filter_map(|p| p.limitation.clone()).collect();
                    let was_empty = self.project().is_empty();
                    let added = self.edit("Import", move |p| {
                        if p.is_empty() {
                            p.sample_rate = ok[0].audio.sample_rate();
                            p.channels = ok
                                .iter()
                                .map(|x| x.audio.num_channels())
                                .max()
                                .unwrap_or(2)
                                .clamp(1, 8) as u16;
                        }
                        let mut ids = Vec::new();
                        for prep in ok {
                            let sid = p.add_prepared_source(
                                prep.name.clone(),
                                Some(prep.path),
                                prep.audio,
                                prep.peaks,
                            );
                            ids.extend(p.add_track_with_source(prep.name, sid, 0));
                        }
                        Ok(ids)
                    });
                    if let Some(ids) = added {
                        if was_empty {
                            self.perform(ctx, Action::ZoomFit);
                        }
                        self.sel.tracks = ids;
                        if warnings.is_empty() {
                            self.info(format!("Imported {n} file(s)"));
                        } else {
                            self.error(format!(
                                "Imported {n} file(s). Warning: {}",
                                warnings.join("; ")
                            ));
                        }
                    }
                }
                if !errors.is_empty() {
                    self.error(errors.join("; "));
                }
            }
            JobDone::AlbumAnalyzed(r, fingerprint) => {
                if let Some(Dialog::Album(state)) = &mut self.dialog {
                    state.analyzing = false;
                    match r {
                        Ok(a) => state.analysis = Some((fingerprint, a)),
                        Err(e) => self.status = Some((format!("Album analysis failed: {e}"), true)),
                    }
                }
            }
            JobDone::Recovered(orphan, r) => match r {
                Ok(project) => {
                    self.player.stop();
                    self.doc = Document::new(project);
                    self.doc.path = orphan.original.clone();
                    self.doc.mark_unsaved();
                    self.sel = Selection::default();
                    self.cursor = 0;
                    orphan.discard();
                    self.perform(ctx, Action::ZoomFit);
                    self.info("Recovered unsaved work. Save it to keep it.");
                }
                Err(e) => self.error(format!("Could not recover: {e}")),
            },
            JobDone::Loaded(path, r) => match r {
                Ok(project) => {
                    self.remember_recent(&path);
                    self.player.stop();
                    self.doc = Document::new(project);
                    self.doc.path = Some(path.clone());
                    self.sel = Selection::default();
                    self.cursor = 0;
                    self.perform(ctx, Action::ZoomFit);
                    self.info(format!("Opened {}", path.display()));
                }
                Err(e) => self.error(format!("Could not open {}: {e}", path.display())),
            },
            JobDone::Analyzed(r, tracks, revision) => match r {
                Ok(a) => {
                    let state = crate::detect_ui::DetectState::new(a, tracks, revision);
                    self.dialog = Some(Dialog::DetectSongs(Box::new(state)));
                }
                Err(e) if e == kerf_core::Error::Cancelled.to_string() => {}
                Err(e) => self.error(format!("Analysis failed: {e}")),
            },
            JobDone::Exported(r) => match r {
                Ok(paths) if paths.len() == 1 => {
                    self.info(format!("Exported {}", paths[0].display()))
                }
                Ok(paths) => self.info(format!("Exported {} files", paths.len())),
                Err(e) => self.error(format!("Export failed: {e}")),
            },
        }
    }

    // -----------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.dialog.is_some() {
            return;
        }
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        // More specific shortcuts first: consume_shortcut ignores extra Shift.
        let shift = Modifiers::SHIFT;
        let table: &[(Modifiers, Key, Action)] = &[
            (cmd_shift, Key::ArrowLeft, Action::ExtendPrevEditPoint),
            (cmd_shift, Key::ArrowRight, Action::ExtendNextEditPoint),
            (cmd_shift, Key::M, Action::ToggleMarkersPanel),
            (cmd_shift, Key::H, Action::ToggleHistoryPanel),
            (cmd_shift, Key::I, Action::ToggleStatsPanel),
            (cmd_shift, Key::A, Action::Album),
            (cmd_shift, Key::O, Action::OpenRecent),
            (cmd, Key::ArrowLeft, Action::PrevEditPoint),
            (cmd, Key::ArrowRight, Action::NextEditPoint),
            (cmd, Key::Comma, Action::Preferences),
            (Modifiers::ALT, Key::ArrowUp, Action::TallerTracks),
            (Modifiers::ALT, Key::ArrowDown, Action::ShorterTracks),
            (cmd, Key::ArrowUp, Action::VZoomIn),
            (cmd, Key::ArrowDown, Action::VZoomOut),
            (cmd_shift, Key::Z, Action::Redo),
            (cmd_shift, Key::S, Action::SaveAs),
            (cmd_shift, Key::E, Action::Export(ExportKind::Markers)),
            (cmd_shift, Key::N, Action::AddTrack),
            (cmd, Key::Z, Action::Undo),
            (cmd, Key::Y, Action::Redo),
            (cmd, Key::N, Action::New),
            (cmd, Key::O, Action::Open),
            (cmd, Key::I, Action::Import),
            (cmd, Key::S, Action::Save),
            (cmd, Key::E, Action::Export(ExportKind::Mix)),
            (cmd, Key::Q, Action::Quit),
            (cmd, Key::A, Action::SelectAll),
            (cmd, Key::D, Action::SelectNone),
            (cmd, Key::L, Action::Silence),
            (cmd, Key::T, Action::Crop),
            (cmd, Key::G, Action::Gain),
            (cmd, Key::M, Action::Normalize),
            (cmd, Key::R, Action::Bounce),
            (cmd, Key::F, Action::ZoomFit),
            (cmd, Key::C, Action::Copy),
            (cmd, Key::X, Action::Cut),
            (cmd, Key::V, Action::Paste),
            (shift, Key::ArrowLeft, Action::ExtendLeft),
            (shift, Key::ArrowRight, Action::ExtendRight),
            (shift, Key::ArrowUp, Action::ExtendTrackUp),
            (shift, Key::ArrowDown, Action::ExtendTrackDown),
            (shift, Key::Home, Action::ExtendToStart),
            (shift, Key::End, Action::ExtendToEnd),
            (shift, Key::Space, Action::StopAtPlayhead),
            (shift, Key::Z, Action::ZeroCrossings),
            (shift, Key::L, Action::ToggleLoop),
            (Modifiers::NONE, Key::Space, Action::PlayStop),
            (Modifiers::NONE, Key::ArrowLeft, Action::CursorLeft),
            (Modifiers::NONE, Key::ArrowRight, Action::CursorRight),
            (Modifiers::NONE, Key::ArrowUp, Action::TrackUp),
            (Modifiers::NONE, Key::ArrowDown, Action::TrackDown),
            (Modifiers::NONE, Key::PageUp, Action::PageLeft),
            (Modifiers::NONE, Key::PageDown, Action::PageRight),
            (Modifiers::NONE, Key::OpenBracket, Action::GoSelectionStart),
            (Modifiers::NONE, Key::CloseBracket, Action::GoSelectionEnd),
            (Modifiers::NONE, Key::P, Action::PreviewCursor),
            (Modifiers::NONE, Key::D, Action::ToggleDbScale),
            (Modifiers::NONE, Key::S, Action::Split),
            (Modifiers::NONE, Key::M, Action::AddMarker),
            (Modifiers::NONE, Key::J, Action::ShuttleReverse),
            (Modifiers::NONE, Key::K, Action::ShuttleStop),
            (Modifiers::NONE, Key::L, Action::ShuttleForward),
            (Modifiers::NONE, Key::N, Action::ToggleSnap),
            (Modifiers::NONE, Key::Delete, Action::Delete),
            (Modifiers::NONE, Key::Backspace, Action::Delete),
            (Modifiers::NONE, Key::Home, Action::GoStart),
            (Modifiers::NONE, Key::End, Action::GoEnd),
            (Modifiers::NONE, Key::Comma, Action::PrevMarker),
            (Modifiers::NONE, Key::Period, Action::NextMarker),
            (Modifiers::NONE, Key::Plus, Action::ZoomIn),
            (Modifiers::NONE, Key::Equals, Action::ZoomIn),
            (Modifiers::NONE, Key::Minus, Action::ZoomOut),
            (Modifiers::NONE, Key::I, Action::FadeIn),
            (Modifiers::NONE, Key::O, Action::FadeOut),
            (Modifiers::NONE, Key::Z, Action::ZoomSelection),
            (Modifiers::NONE, Key::Escape, Action::SelectNone),
        ];
        let mut fired = Vec::new();
        let mut k_down = false;
        ctx.input_mut(|i| {
            // J/K/L act on presses, not on key repeat: holding L must not keep
            // speeding up, nor holding K keep stopping.
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: Key::J | Key::K | Key::L,
                        repeat: true,
                        ..
                    }
                )
            });
            k_down = i.key_down(Key::K);
            // egui turns Ctrl+C/X/V into clipboard events rather than key presses.
            i.events.retain(|e| match e {
                egui::Event::Copy => {
                    fired.push(Action::Copy);
                    false
                }
                egui::Event::Cut => {
                    fired.push(Action::Cut);
                    false
                }
                egui::Event::Paste(_) => {
                    fired.push(Action::Paste);
                    false
                }
                _ => true,
            });
            for (m, k, a) in table {
                if i.consume_shortcut(&KeyboardShortcut::new(*m, *k)) {
                    fired.push(*a);
                }
            }
        });
        for mut a in fired {
            // K held with J or L: half speed for as long as J or L is held.
            if k_down && matches!(a, Action::ShuttleForward | Action::ShuttleReverse) {
                let forward = a == Action::ShuttleForward;
                self.shuttle_hold = Some(if forward { Key::L } else { Key::J });
                a = if forward {
                    Action::ShuttleSlowForward
                } else {
                    Action::ShuttleSlowReverse
                };
            }
            self.perform(ctx, a);
        }
        if let Some(key) = self.shuttle_hold {
            if !ctx.input(|i| i.key_down(key)) {
                self.shuttle_hold = None;
                self.perform(ctx, Action::ShuttleStop);
            }
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut act = None;
        let mut pending_open: Option<PathBuf> = None;
        let mut clear_recent = false;
        let item = |ui: &mut egui::Ui,
                    label: &str,
                    shortcut: &str,
                    a: Action,
                    act: &mut Option<Action>| {
            let btn = egui::Button::new(label).shortcut_text(shortcut);
            if ui.add(btn).clicked() {
                *act = Some(a);
                ui.close();
            }
        };
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                item(ui, "New", "Ctrl+N", Action::New, &mut act);
                item(ui, "Open Project…", "Ctrl+O", Action::Open, &mut act);
                ui.menu_button("Open Recent", |ui| {
                    let recent = self.prefs.recent_projects.clone();
                    if recent.is_empty() {
                        ui.label(egui::RichText::new("No recent projects").weak());
                    }
                    for path in recent {
                        let label = crate::recent::name(&path);
                        let resp = ui
                            .add_enabled(path.exists(), egui::Button::new(label))
                            .on_hover_text(path.display().to_string());
                        if resp.clicked() {
                            pending_open = Some(path);
                            ui.close();
                        }
                    }
                    ui.separator();
                    item(ui, "More…", "Ctrl+Shift+O", Action::OpenRecent, &mut act);
                    if ui.button("Clear Recent").clicked() {
                        clear_recent = true;
                        ui.close();
                    }
                });
                item(ui, "Import Audio…", "Ctrl+I", Action::Import, &mut act);
                ui.separator();
                item(ui, "Save Project", "Ctrl+S", Action::Save, &mut act);
                item(
                    ui,
                    "Save Project As…",
                    "Ctrl+Shift+S",
                    Action::SaveAs,
                    &mut act,
                );
                ui.separator();
                item(
                    ui,
                    "Export Mix…",
                    "Ctrl+E",
                    Action::Export(ExportKind::Mix),
                    &mut act,
                );
                item(
                    ui,
                    "Export Selection…",
                    "",
                    Action::Export(ExportKind::Selection),
                    &mut act,
                );
                item(
                    ui,
                    "Export Split at Markers…",
                    "Ctrl+Shift+E",
                    Action::Export(ExportKind::Markers),
                    &mut act,
                );
                item(ui, "Album…", "Ctrl+Shift+A", Action::Album, &mut act);
                item(
                    ui,
                    "Export Mix with CUE Sheet…",
                    "",
                    Action::Export(ExportKind::CueSheet),
                    &mut act,
                );
                item(
                    ui,
                    "Export Tracks as Stems…",
                    "",
                    Action::Export(ExportKind::Stems),
                    &mut act,
                );
                ui.separator();
                item(ui, "Audio Cache…", "", Action::AudioCache, &mut act);
                ui.separator();
                item(ui, "Quit", "Ctrl+Q", Action::Quit, &mut act);
            });
            ui.menu_button("Edit", |ui| {
                let undo = self
                    .doc
                    .undo_label()
                    .map_or("Undo".to_string(), |l| format!("Undo {l}"));
                let redo = self
                    .doc
                    .redo_label()
                    .map_or("Redo".to_string(), |l| format!("Redo {l}"));
                item(ui, &undo, "Ctrl+Z", Action::Undo, &mut act);
                item(ui, &redo, "Ctrl+Shift+Z", Action::Redo, &mut act);
                ui.separator();
                item(ui, "Cut", "Ctrl+X", Action::Cut, &mut act);
                item(ui, "Copy", "Ctrl+C", Action::Copy, &mut act);
                item(ui, "Paste", "Ctrl+V", Action::Paste, &mut act);
                ui.menu_button("Paste mode", |ui| {
                    ui.radio_value(
                        &mut self.paste_mode,
                        PasteMode::Insert,
                        "Insert (push later audio right)",
                    );
                    ui.radio_value(&mut self.paste_mode, PasteMode::Overwrite, "Overwrite");
                    ui.radio_value(&mut self.paste_mode, PasteMode::Mix, "Mix (layer on top)");
                });
                ui.separator();
                item(ui, "Delete (close gap)", "Del", Action::Delete, &mut act);
                item(
                    ui,
                    "Silence (leave gap)",
                    "Ctrl+L",
                    Action::Silence,
                    &mut act,
                );
                item(ui, "Trim to Selection", "Ctrl+T", Action::Crop, &mut act);
                item(ui, "Split", "S", Action::Split, &mut act);
                item(ui, "Insert Silence…", "", Action::InsertSilence, &mut act);
                ui.separator();
                item(ui, "Select All", "Ctrl+A", Action::SelectAll, &mut act);
                item(ui, "Select None", "Esc", Action::SelectNone, &mut act);
                ui.separator();
                item(
                    ui,
                    "Move Selection to Zero Crossings",
                    "Shift+Z",
                    Action::ZeroCrossings,
                    &mut act,
                );
                let mut zc = self.prefs.snap_zero_crossings;
                if ui
                    .checkbox(&mut zc, "Snap Edits to Zero Crossings")
                    .changed()
                {
                    act = Some(Action::ToggleZeroCrossingSnap);
                }
                ui.separator();
                item(ui, "Preferences…", "Ctrl+,", Action::Preferences, &mut act);
            });
            ui.menu_button("Process", |ui| {
                item(ui, "Normalize…", "Ctrl+M", Action::Normalize, &mut act);
                item(ui, "Gain…", "Ctrl+G", Action::Gain, &mut act);
                ui.separator();
                item(ui, "Fade In", "I", Action::FadeIn, &mut act);
                item(ui, "Fade Out", "O", Action::FadeOut, &mut act);
                item(ui, "Remove Fades", "", Action::RemoveFades, &mut act);
                ui.menu_button("Fade shape", |ui| {
                    for s in FadeShape::ALL {
                        ui.radio_value(&mut self.fade_shape, s, s.label());
                    }
                });
                let mut auto_xfade = self.project().auto_crossfade;
                if ui
                    .checkbox(&mut auto_xfade, "Automatic crossfades")
                    .on_hover_text("Crossfade where clips overlap after moving or trimming")
                    .changed()
                {
                    let label = if auto_xfade {
                        "Crossfades on"
                    } else {
                        "Crossfades off"
                    };
                    self.edit(label, |p| edit::set_auto_crossfade(p, auto_xfade));
                }
                ui.separator();
                item(
                    ui,
                    "Render Track(s) in Place",
                    "Ctrl+R",
                    Action::Bounce,
                    &mut act,
                );
            });
            ui.menu_button("Tracks", |ui| {
                item(ui, "Add Track", "Ctrl+Shift+N", Action::AddTrack, &mut act);
                item(
                    ui,
                    "Remove Selected Tracks",
                    "",
                    Action::RemoveTracks,
                    &mut act,
                );
                ui.separator();
                ui.label("Output channels");
                let ch = self.project().channels;
                for (n, label) in [(1u16, "Mono"), (2, "Stereo")] {
                    if ui.radio(ch == n, label).clicked() {
                        self.edit("Output channels", |p| {
                            p.channels = n;
                            Ok(())
                        });
                    }
                }
            });
            ui.menu_button("Transport", |ui| {
                let space = if self.prefs.space_returns_to_start {
                    "Play / Stop"
                } else {
                    "Play / Stop at Playhead"
                };
                let shift_space = if self.prefs.space_returns_to_start {
                    "Stop at Playhead"
                } else {
                    "Stop and Return"
                };
                item(ui, space, "Space", Action::PlayStop, &mut act);
                item(
                    ui,
                    shift_space,
                    "Shift+Space",
                    Action::StopAtPlayhead,
                    &mut act,
                );
                item(
                    ui,
                    "Preview Around Cursor",
                    "P",
                    Action::PreviewCursor,
                    &mut act,
                );
                ui.separator();
                item(
                    ui,
                    "Shuttle Forward (faster)",
                    "L",
                    Action::ShuttleForward,
                    &mut act,
                );
                item(
                    ui,
                    "Shuttle Backward (faster)",
                    "J",
                    Action::ShuttleReverse,
                    &mut act,
                );
                item(ui, "Stop Shuttle", "K", Action::ShuttleStop, &mut act);
                ui.label(
                    egui::RichText::new(
                        "Hold K with J or L for half speed.\nDrag the playhead in the ruler to scrub.",
                    )
                    .weak(),
                );
                ui.separator();
                item(ui, "Go to Start", "Home", Action::GoStart, &mut act);
                item(ui, "Go to End", "End", Action::GoEnd, &mut act);
                item(
                    ui,
                    "Previous Edit Point",
                    "Ctrl+←",
                    Action::PrevEditPoint,
                    &mut act,
                );
                item(
                    ui,
                    "Next Edit Point",
                    "Ctrl+→",
                    Action::NextEditPoint,
                    &mut act,
                );
                item(
                    ui,
                    "Go to Selection Start",
                    "[",
                    Action::GoSelectionStart,
                    &mut act,
                );
                item(
                    ui,
                    "Go to Selection End",
                    "]",
                    Action::GoSelectionEnd,
                    &mut act,
                );
                ui.separator();
                ui.checkbox(&mut self.loop_playback, "Loop (Shift+L)");
            });
            ui.menu_button("Markers", |ui| {
                item(ui, "Add Marker", "M", Action::AddMarker, &mut act);
                item(ui, "Previous Marker", ",", Action::PrevMarker, &mut act);
                item(ui, "Next Marker", ".", Action::NextMarker, &mut act);
                ui.separator();
                item(ui, "Detect Song Starts…", "", Action::DetectSongs, &mut act);
                item(
                    ui,
                    "Snap Markers to CD Sectors",
                    "",
                    Action::SnapMarkersToSectors,
                    &mut act,
                );
                item(
                    ui,
                    "Select Song at Cursor",
                    "double-click",
                    Action::SelectSong,
                    &mut act,
                );
                ui.separator();
                ui.label(
                    egui::RichText::new("Double-click a marker to rename,\nright-click to delete.")
                        .weak(),
                );
            });
            ui.menu_button("View", |ui| {
                item(ui, "Zoom In", "+", Action::ZoomIn, &mut act);
                item(ui, "Zoom Out", "-", Action::ZoomOut, &mut act);
                item(ui, "Zoom to Fit", "Ctrl+F", Action::ZoomFit, &mut act);
                item(
                    ui,
                    "Zoom to Selection",
                    "Z",
                    Action::ZoomSelection,
                    &mut act,
                );
                ui.separator();
                item(
                    ui,
                    "Zoom In Vertically",
                    "Ctrl+↑",
                    Action::VZoomIn,
                    &mut act,
                );
                item(
                    ui,
                    "Zoom Out Vertically",
                    "Ctrl+↓",
                    Action::VZoomOut,
                    &mut act,
                );
                item(ui, "Reset Vertical Zoom", "", Action::VZoomReset, &mut act);
                ui.separator();
                item(ui, "Taller Tracks", "Alt+↑", Action::TallerTracks, &mut act);
                item(
                    ui,
                    "Shorter Tracks",
                    "Alt+↓",
                    Action::ShorterTracks,
                    &mut act,
                );
                item(ui, "Fit Tracks to Window", "", Action::FitTracks, &mut act);
                item(
                    ui,
                    "Reset Track Heights",
                    "",
                    Action::ResetTrackHeights,
                    &mut act,
                );
                ui.checkbox(&mut self.view.db_scale, "Waveform in dB (D)");
                ui.menu_button("Time Format", |ui| {
                    for f in kerf_core::util::TimeFormat::ALL {
                        if ui.radio(self.prefs.time_format == f, f.label()).clicked() {
                            self.prefs.time_format = f;
                            let _ = self.prefs.save();
                        }
                    }
                });
                ui.separator();
                ui.checkbox(&mut self.view.snap, "Snap to edges & markers");
                ui.label("While playing:");
                {
                    use crate::prefs::FollowMode;
                    for (mode, label) in [
                        (FollowMode::Off, "Don't follow the playhead"),
                        (FollowMode::Page, "Follow the playhead by pages"),
                        (FollowMode::Center, "Keep the playhead centred"),
                    ] {
                        if ui.radio(self.prefs.follow == mode, label).clicked() {
                            self.set_follow(mode);
                        }
                    }
                }
                if ui
                    .checkbox(
                        &mut self.prefs.show_markers_panel,
                        "Markers panel (Ctrl+Shift+M)",
                    )
                    .changed()
                {
                    let _ = self.prefs.save();
                }
                if ui
                    .checkbox(
                        &mut self.prefs.show_stats_panel,
                        "Statistics panel (Ctrl+Shift+I)",
                    )
                    .changed()
                {
                    let _ = self.prefs.save();
                }
                if ui
                    .checkbox(
                        &mut self.prefs.show_history_panel,
                        "History panel (Ctrl+Shift+H)",
                    )
                    .changed()
                {
                    let _ = self.prefs.save();
                }
            });
            ui.menu_button("Help", |ui| {
                item(ui, "About Kerf", "", Action::About, &mut act)
            });
        });
        if let Some(a) = act {
            self.perform(&ctx, a);
        }
        if let Some(path) = pending_open {
            self.guard_discard(&ctx, Pending::Open(Some(path)));
        }
        if clear_recent {
            self.clear_recent();
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let inline = ui
            .horizontal(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
                // Items are centred on the row height known when they are added,
                // so make the row as tall as its tallest item (the position
                // field, measured last frame) up front, and the buttons too.
                let field_h = ui
                    .ctx()
                    .data(|d| d.get_temp::<f32>(time_field_height_id("pos")))
                    .unwrap_or_else(|| {
                        ui.fonts_mut(|f| f.row_height(&egui::FontId::monospace(20.0))) + 10.0
                    });
                ui.set_min_height(field_h.max(crate::icons::MIN_HEIGHT));
                let playing = self.player.is_playing();
                use crate::icons::{button, Icon};
                if button(ui, Icon::GoStart, false, field_h)
                    .on_hover_text("Go to start (Home)")
                    .clicked()
                {
                    self.perform(&ctx, Action::GoStart);
                }
                let icon = if playing { Icon::Stop } else { Icon::Play };
                if button(ui, icon, false, field_h)
                    .on_hover_text("Play / Stop (Space)")
                    .clicked()
                {
                    self.perform(&ctx, Action::PlayStop);
                }
                if button(ui, Icon::GoEnd, false, field_h)
                    .on_hover_text("Go to end (End)")
                    .clicked()
                {
                    self.perform(&ctx, Action::GoEnd);
                }
                if button(ui, Icon::Loop, self.loop_playback, field_h)
                    .on_hover_text("Loop playback (Shift+L)")
                    .clicked()
                {
                    self.loop_playback = !self.loop_playback;
                }
                if playing && !self.player.is_scrubbing() && self.player.speed() != 1.0 {
                    ui.label(egui::RichText::new(speed_label(self.player.speed())).monospace())
                        .on_hover_text("Shuttle speed (J / K / L)");
                }
                ui.separator();

                let sr = self.project().sample_rate;
                let pos = if playing {
                    self.player.position()
                } else {
                    self.cursor
                };
                let mut format_changed = false;
                let r = time_field(
                    ui,
                    "pos",
                    Some(pos),
                    150.0,
                    20.0,
                    &mut self.time_edit,
                    sr,
                    &mut self.prefs.time_format,
                );
                format_changed |= r.1;
                if let Some(f) = r.0 {
                    self.sel.range = None;
                    self.seek(f);
                    self.ensure_visible(f);
                }
                ui.separator();
                let range = self.sel.range;
                let small = 104.0;
                ui.label(egui::RichText::new("Start").weak());
                let r = time_field(
                    ui,
                    "sel-start",
                    range.map(|r| r.0),
                    small,
                    13.0,
                    &mut self.time_edit,
                    sr,
                    &mut self.prefs.time_format,
                );
                format_changed |= r.1;
                if let Some(f) = r.0 {
                    let end = range.map_or(self.cursor, |r| r.1);
                    self.sel.range = (end > f).then_some((f, end));
                    self.cursor = f;
                }
                ui.label(egui::RichText::new("End").weak());
                let r = time_field(
                    ui,
                    "sel-end",
                    range.map(|r| r.1),
                    small,
                    13.0,
                    &mut self.time_edit,
                    sr,
                    &mut self.prefs.time_format,
                );
                format_changed |= r.1;
                if let Some(f) = r.0 {
                    let start = range.map_or(self.cursor, |r| r.0);
                    self.sel.range = (f > start).then_some((start, f));
                    self.cursor = f;
                }
                ui.label(egui::RichText::new("Length").weak());
                let r = time_field(
                    ui,
                    "sel-len",
                    range.map(|r| r.1 - r.0),
                    small,
                    13.0,
                    &mut self.time_edit,
                    sr,
                    &mut self.prefs.time_format,
                );
                format_changed |= r.1;
                if let Some(len) = r.0 {
                    let start = range.map_or(self.cursor, |r| r.0);
                    self.sel.range = (len > 0).then_some((start, start + len));
                }
                if format_changed {
                    let _ = self.prefs.save();
                }
                ui.separator();
                use crate::icons::tool;
                let snap = self.view.snap;
                if tool(
                    ui,
                    Icon::Snap,
                    snap,
                    true,
                    field_h,
                    "Snap to clip edges, markers and cursor (N)",
                ) {
                    self.perform(&ctx, Action::ToggleSnap);
                }
                if tool(ui, Icon::ZoomIn, false, true, field_h, "Zoom in (+)") {
                    self.perform(&ctx, Action::ZoomIn);
                }
                if tool(ui, Icon::ZoomOut, false, true, field_h, "Zoom out (−)") {
                    self.perform(&ctx, Action::ZoomOut);
                }
                if tool(
                    ui,
                    Icon::ZoomFit,
                    false,
                    true,
                    field_h,
                    "Zoom to fit (Ctrl+F)",
                ) {
                    self.perform(&ctx, Action::ZoomFit);
                }

                // The tool groups go here when they fit beside the meter,
                // otherwise on a row of their own.
                const METER_W: f32 = 160.0;
                let inline = ui.available_width() - METER_W - 24.0 >= self.tool_groups_width(ui);
                if inline {
                    ui.separator();
                    self.tool_groups(ui, &ctx, field_h);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    meter(ui, self.meters, self.project().channels);
                });
                inline
            })
            .inner;
        if !inline {
            ui.horizontal(|ui| self.tool_groups(ui, &ctx, crate::icons::MIN_HEIGHT + 4.0));
        }
    }

    /// Edit, processing and view buttons, in that order.
    const TOOL_GROUPS: [usize; 3] = [7, 4, 6];

    fn tool_groups_width(&self, ui: &egui::Ui) -> f32 {
        let sp = ui.spacing().item_spacing.x;
        let buttons: usize = Self::TOOL_GROUPS.iter().sum();
        let separators = (Self::TOOL_GROUPS.len() - 1) as f32;
        buttons as f32 * (crate::icons::WIDTH + sp) + separators * (2.0 * sp + 1.0)
    }

    fn tool_groups(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, h: f32) {
        use crate::icons::{tool, Icon};
        let has_range = self.sel.range.is_some_and(|(a, b)| b > a);
        let has_tracks = !self.project().tracks.is_empty();
        let undo = self.doc.undo_label().map(|l| format!("Undo {l} (Ctrl+Z)"));
        let redo = self
            .doc
            .redo_label()
            .map(|l| format!("Redo {l} (Ctrl+Shift+Z)"));
        let mut act = None;
        let mut add = |ui: &mut egui::Ui, icon, on, enabled, tip: &str, a: Action| {
            if tool(ui, icon, on, enabled, h, tip) {
                act = Some(a);
            }
        };

        // Edit
        add(
            ui,
            Icon::Undo,
            false,
            undo.is_some(),
            undo.as_deref().unwrap_or("Undo (Ctrl+Z)"),
            Action::Undo,
        );
        add(
            ui,
            Icon::Redo,
            false,
            redo.is_some(),
            redo.as_deref().unwrap_or("Redo (Ctrl+Shift+Z)"),
            Action::Redo,
        );
        add(
            ui,
            Icon::Split,
            false,
            has_tracks,
            "Split at the cursor or selection edges (S)",
            Action::Split,
        );
        add(ui, Icon::Cut, false, has_range, "Cut (Ctrl+X)", Action::Cut);
        add(
            ui,
            Icon::Copy,
            false,
            has_range,
            "Copy (Ctrl+C)",
            Action::Copy,
        );
        add(
            ui,
            Icon::Paste,
            false,
            self.clipboard.is_some(),
            "Paste at the cursor (Ctrl+V)",
            Action::Paste,
        );
        add(
            ui,
            Icon::Delete,
            false,
            has_range || self.sel.clip.is_some(),
            "Delete the selection and close the gap (Del)",
            Action::Delete,
        );
        ui.separator();

        // Processing
        add(
            ui,
            Icon::FadeIn,
            false,
            has_range,
            "Fade in over the selection (I)",
            Action::FadeIn,
        );
        add(
            ui,
            Icon::FadeOut,
            false,
            has_range,
            "Fade out over the selection (O)",
            Action::FadeOut,
        );
        add(
            ui,
            Icon::Normalize,
            false,
            has_tracks,
            "Normalize… (Ctrl+M)",
            Action::Normalize,
        );
        add(
            ui,
            Icon::Gain,
            false,
            has_tracks,
            "Gain… (Ctrl+G)",
            Action::Gain,
        );
        ui.separator();

        // View
        {
            use crate::prefs::FollowMode;
            let follow = self.prefs.follow;
            add(
                ui,
                Icon::Follow,
                follow == FollowMode::Page,
                true,
                "Follow the playhead by pages while playing",
                Action::FollowPage,
            );
            add(
                ui,
                Icon::Center,
                follow == FollowMode::Center,
                true,
                "Keep the playhead centred while playing: the audio scrolls past it",
                Action::FollowCenter,
            );
        }
        add(
            ui,
            Icon::DbScale,
            self.view.db_scale,
            true,
            "Waveform in dB (D)",
            Action::ToggleDbScale,
        );
        add(
            ui,
            Icon::ZeroCrossings,
            self.prefs.snap_zero_crossings,
            true,
            "Snap edits to zero crossings",
            Action::ToggleZeroCrossingSnap,
        );
        add(
            ui,
            Icon::ZoomSelection,
            false,
            has_range,
            "Zoom to selection (Z)",
            Action::ZoomSelection,
        );
        add(
            ui,
            Icon::MarkersPanel,
            self.prefs.show_markers_panel,
            true,
            "Markers panel (Ctrl+Shift+M)",
            Action::ToggleMarkersPanel,
        );
        add(
            ui,
            Icon::StatsPanel,
            self.prefs.show_stats_panel,
            true,
            "Statistics panel (Ctrl+Shift+I)",
            Action::ToggleStatsPanel,
        );
        add(
            ui,
            Icon::HistoryPanel,
            self.prefs.show_history_panel,
            true,
            "History panel (Ctrl+Shift+H)",
            Action::ToggleHistoryPanel,
        );
        if let Some(a) = act {
            self.perform(ctx, a);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let p = self.doc.project();
            let fmt = format!(
                "{} Hz · {} · 32-bit float · {}",
                p.sample_rate,
                match p.channels {
                    1 => "mono".to_string(),
                    2 => "stereo".to_string(),
                    n => format!("{n} ch"),
                },
                format_time(p.length(), p.sample_rate)
            );
            ui.label(egui::RichText::new(fmt).weak());
            if p.master_gain != 1.0 {
                ui.label(
                    egui::RichText::new(format!(
                        "Album gain {:+.1} dB",
                        kerf_core::album::master_gain_db(p)
                    ))
                    .weak(),
                );
            }
            ui.separator();
            if let Some((msg, is_err)) = &self.status {
                let color = if *is_err {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.label(egui::RichText::new(msg).color(color));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.assistant_indicator(ui);
                if let Some(note) = &self.player.fallback_note {
                    ui.colored_label(ui.visuals().warn_fg_color, note);
                }
                if !self.player.device_name.is_empty() {
                    let dev = &self.player.device_name;
                    let rate = self.player.device_rate;
                    let resampled = if rate != 0 && rate != p.sample_rate {
                        " (resampled)"
                    } else {
                        ""
                    };
                    ui.label(
                        egui::RichText::new(format!("🔊 {dev} @ {rate} Hz{resampled}")).weak(),
                    );
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut open = true;
        let mut close = false;
        let mut run: Option<DeferredAction> = None;

        let title = match &dialog {
            Dialog::Normalize(_) => "Normalize",
            Dialog::Gain(_) => "Gain",
            Dialog::InsertSilence(_) => "Insert Silence",
            Dialog::Export(k) => k.title(),
            Dialog::RenameMarker(..) => "Rename Marker",
            Dialog::ConfirmDiscard(_) => "Unsaved Changes",
            Dialog::AudioCache(_) => "Audio Cache",
            Dialog::DetectSongs(_) => "Detect Song Starts",
            Dialog::Preferences(_) => "Preferences",
            Dialog::Album(_) => "Album",
            Dialog::Recover(_) => "Recover Unsaved Work",
            Dialog::Recent(_) => "Recent Projects",
            Dialog::About => "About Kerf",
        };
        let scope = match self.sel.range.filter(|(a, b)| b > a) {
            Some(_) => format!("selection on {} track(s)", self.target_tracks().len()),
            None => format!("whole of {} track(s)", self.target_tracks().len()),
        };

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .max_width(if matches!(dialog, Dialog::Album(_)) { 760.0 } else { 540.0 })
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| match &mut dialog {
                Dialog::Normalize(s) => {
                    ui.label(format!("Applies to the {scope}."));
                    egui::Grid::new("norm").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Measure");
                        let prev = s.mode;
                        egui::ComboBox::from_id_salt("mode").selected_text(s.mode.label()).show_ui(ui, |ui| {
                            for m in NormalizeMode::ALL {
                                ui.selectable_value(&mut s.mode, m, m.label());
                            }
                        });
                        if s.mode != prev {
                            s.target_db = s.mode.default_target();
                            if s.mode == NormalizeMode::Loudness && s.peak_ceiling_db.is_none() {
                                s.peak_ceiling_db = Some(-1.0);
                            }
                        }
                        ui.end_row();
                        ui.label("Target");
                        ui.add(egui::DragValue::new(&mut s.target_db).range(-80.0..=12.0).speed(0.1).suffix(format!(" {}", s.mode.unit())));
                        ui.end_row();
                        ui.label("Peak ceiling");
                        ui.horizontal(|ui| {
                            let mut on = s.peak_ceiling_db.is_some();
                            ui.checkbox(&mut on, "");
                            let mut v = s.peak_ceiling_db.unwrap_or(-1.0);
                            ui.add_enabled(on, egui::DragValue::new(&mut v).range(-60.0..=12.0).speed(0.1).suffix(" dBFS"));
                            s.peak_ceiling_db = on.then_some(v);
                        });
                        ui.end_row();
                        ui.label("Tracks");
                        ui.vertical(|ui| {
                            ui.radio_value(&mut s.independent, true, "Normalize each independently");
                            ui.radio_value(&mut s.independent, false, "Same gain for all (keep balance)");
                        });
                        ui.end_row();
                    });
                    ui.label(
                        egui::RichText::new("Non-destructive: adjusts clip gain. Audio is 32-bit float, so nothing clips internally.")
                            .weak()
                            .small(),
                    );
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Normalize").clicked() {
                            let s = *s;
                            run = Some(Box::new(move |app, _| app.run_normalize(s)));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Gain(db) => {
                    ui.label(format!("Applies to the {scope}."));
                    ui.add(egui::Slider::new(db, -48.0..=24.0).suffix(" dB").step_by(0.1));
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            let db = *db;
                            run = Some(Box::new(move |app, _| {
                                let tracks = app.target_tracks();
                                let range = app.sel.range.filter(|(a, b)| b > a);
                                app.edit(&format!("Gain {db:+.1} dB"), |p| edit::apply_gain(p, &tracks, range, db));
                            }));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::InsertSilence(secs) => {
                    ui.label("Insert at the cursor on the selected tracks:");
                    ui.add(egui::DragValue::new(secs).range(0.001..=36_000.0).speed(0.05).suffix(" s"));
                    ui.horizontal(|ui| {
                        if ui.button("Insert").clicked() {
                            let secs = *secs;
                            run = Some(Box::new(move |app, _| {
                                let tracks = app.target_tracks();
                                let len = app.project().secs_to_frames(secs);
                                let pos = app.cursor;
                                app.edit("Insert silence", |p| edit::insert_silence(p, &tracks, pos, len));
                            }));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Export(kind) => {
                    let kind = *kind;
                    match kind {
                        ExportKind::Markers => {
                            let regions = self.export_regions_for(self.project());
                            ui.label(format!("Creates {} file(s), one per region between markers:", regions.len()));
                            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                                let sr = self.project().sample_rate;
                                for (i, r) in regions.iter().enumerate() {
                                    ui.label(format!("{:02}  {}  ({})", i + 1, r.name, format_time(r.end - r.start, sr)));
                                }
                            });
                            if self.project().markers.is_empty() {
                                ui.colored_label(ui.visuals().warn_fg_color, "No markers yet — press M to add split points.");
                            }
                        }
                        ExportKind::Stems => {
                            ui.label("Creates one file per unmuted track (with track gain and pan).");
                        }
                        ExportKind::CueSheet => {
                            let n = self.export_regions_for(self.project()).len();
                            ui.label(format!(
                                "Renders the whole mix as one WAV plus a CUE sheet with {n} track(s), one per marker. \
                                 Ideal for live recordings: players and CD burners play it gaplessly."
                            ));
                            if self.project().markers.is_empty() {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    "No markers yet — use Markers → Detect Song Starts… or press M.",
                                );
                            }
                        }
                        _ => {
                            ui.label("Renders all audible tracks with gain, pan, mute and solo.");
                        }
                    }
                    ui.separator();
                    export_settings_ui(
                        ui,
                        self.doc.project(),
                        &mut self.export_settings,
                        matches!(kind, ExportKind::Markers | ExportKind::CueSheet).then_some(&mut self.cd_safe),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Export…").clicked() {
                            run = Some(Box::new(move |app, ctx| app.start_export(ctx, kind)));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::RenameMarker(id, name) => {
                    let r = ui.text_edit_singleline(name);
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("OK").clicked() || enter {
                        let (id, name) = (*id, name.clone());
                        run = Some(Box::new(move |app, _| {
                            app.edit("Rename marker", |p| {
                                if let Some(m) = p.marker_mut(id) {
                                    m.name = name;
                                }
                                Ok(())
                            });
                        }));
                        close = true;
                    }
                }
                Dialog::ConfirmDiscard(then) => {
                    ui.label("The project has unsaved changes.");
                    ui.horizontal(|ui| {
                        let then = then.clone();
                        if ui.button("Save").clicked() {
                            run = Some(Box::new(move |app, ctx| {
                                if app.save(false) {
                                    app.do_pending(ctx, then);
                                }
                            }));
                            close = true;
                        } else if ui.button("Don't Save").clicked() {
                            run = Some(Box::new(move |app, ctx| {
                                app.doc.mark_saved();
                                app.do_pending(ctx, then);
                            }));
                            close = true;
                        } else if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Album(state) => {
                    use crate::album_ui::Outcome;
                    match crate::album_ui::ui(self, ui, state) {
                        Outcome::None => {}
                        Outcome::Analyze => {
                            if self.job.is_none() {
                                state.analyzing = true;
                                run = Some(Box::new(|app, ctx| app.analyze_album(ctx)));
                            }
                        }
                        Outcome::ApplyGain(db) => {
                            run = Some(Box::new(move |app, _| {
                                app.edit(&format!("Album gain {db:+.1} dB"), |p| kerf_core::album::adjust_master_gain(p, db));
                            }));
                        }
                        Outcome::ResetGain => {
                            run = Some(Box::new(|app, _| {
                                app.edit("Reset album gain", |p| {
                                    p.master_gain = 1.0;
                                    Ok(())
                                });
                            }));
                        }
                        Outcome::ChooseCover => {
                            run = Some(Box::new(|app, _| {
                                if let Some(path) =
                                    rfd::FileDialog::new().add_filter("Image", &["jpg", "jpeg", "png"]).pick_file()
                                {
                                    app.edit("Album cover", |p| {
                                        p.album.cover = Some(path);
                                        Ok(())
                                    });
                                }
                            }));
                        }
                        Outcome::Export => {
                            run = Some(Box::new(|app, ctx| app.perform(ctx, Action::Export(ExportKind::Markers))));
                            close = true;
                        }
                        Outcome::Close => close = true,
                    }
                }
                Dialog::Preferences(draft) => {
                    prefs_ui(ui, draft, self.player.fallback_note.as_deref());
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            let new = draft.prefs.clone();
                            run = Some(Box::new(move |app, ctx| app.apply_prefs(ctx, new)));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Recover(orphans) => {
                    ui.label("Kerf didn't close normally last time. This unsaved work can be recovered:");
                    ui.add_space(4.0);
                    let mut recover = None;
                    let mut discard = None;
                    for (i, o) in orphans.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(o.title()).strong());
                            ui.label(egui::RichText::new(ago(o.saved_at)).weak());
                            if ui.button("Recover").clicked() {
                                recover = Some(i);
                            }
                            if ui.button("Discard").clicked() {
                                discard = Some(i);
                            }
                        });
                    }
                    if let Some(i) = discard {
                        orphans.remove(i).discard();
                    }
                    if let Some(i) = recover {
                        let o = orphans.remove(i);
                        run = Some(Box::new(move |app, ctx| app.recover(ctx, o)));
                    }
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("“Later” keeps them; you'll be asked again next time Kerf starts.").weak().small(),
                    );
                    if ui.button("Later").clicked() || orphans.is_empty() {
                        close = true;
                    }
                }
                Dialog::DetectSongs(state) => {
                    use crate::detect_ui::Outcome;
                    match state.ui(ui, self.project().sample_rate, self.doc.revision()) {
                        Outcome::None => {}
                        Outcome::Cancel => close = true,
                        Outcome::Reanalyze => {
                            run = Some(Box::new(|app, ctx| app.detect_songs(ctx)));
                            close = true;
                        }
                        Outcome::Apply { start, end, positions, names } => {
                            run = Some(Box::new(move |app, _| {
                                let n = positions.len();
                                if app
                                    .edit("Detect song starts", |p| {
                                        kerf_core::detect::apply_markers(p, start, end, &positions, &names)
                                    })
                                    .is_some()
                                {
                                    app.info(format!("Placed {n} song markers. Export with File → Export Split at Markers or CUE Sheet."));
                                }
                            }));
                            close = true;
                        }
                    }
                }
                Dialog::AudioCache(view) => {
                    match &self.cache {
                        None => {
                            ui.label("No cache folder is available, so audio is kept in memory.");
                        }
                        Some(cache) => {
                            ui.label("Decoded audio is kept here and memory-mapped, so RAM use stays small.");
                            ui.add(
                                egui::Label::new(egui::RichText::new(cache.dir().display().to_string()).monospace().small())
                                    .wrap(),
                            );
                            ui.horizontal(|ui| {
                                if ui.small_button("Open folder").clicked() {
                                    run = Some(Box::new(|app, _| {
                                        if let Some(c) = &app.cache {
                                            if let Err(e) = open_folder(c.dir()) {
                                                app.error(format!("Could not open folder: {e}"));
                                            }
                                        }
                                    }));
                                }
                            });
                            ui.separator();
                            match &view.stats {
                                Some(st) => {
                                    egui::Grid::new("cache-stats").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                        ui.label("Total");
                                        ui.label(format!("{} in {}", human_bytes(st.bytes), entries(st.entries)));
                                        ui.end_row();
                                        ui.label("In use now");
                                        ui.label(format!("{} in {}", human_bytes(st.in_use_bytes), entries(st.in_use_entries)));
                                        ui.end_row();
                                        ui.label("Unused");
                                        ui.label(human_bytes(st.bytes - st.in_use_bytes));
                                        ui.end_row();
                                    });
                                }
                                None => {
                                    ui.label("Could not read the cache folder.");
                                }
                            }
                            if let Some(note) = &view.note {
                                ui.label(egui::RichText::new(note).strong());
                            }
                            ui.label(
                                egui::RichText::new(
                                    "Clearing is always safe: audio in the open project and its undo history is kept, \
                                     and anything removed is decoded again from the original file when next needed. \
                                     Entries unused for 30 days are removed automatically.",
                                )
                                .weak()
                                .small(),
                            );
                            ui.horizontal(|ui| {
                                let unused = view.stats.map_or(0, |st| st.bytes - st.in_use_bytes);
                                if ui.add_enabled(unused > 0, egui::Button::new("Clear Unused")).clicked() {
                                    run = Some(Box::new(|app, _| app.clear_unused_cache()));
                                }
                                if ui.button("Refresh").clicked() {
                                    run = Some(Box::new(|app, _| app.refresh_cache_view(None)));
                                }
                                if ui.button("Close").clicked() {
                                    close = true;
                                }
                            });
                        }
                    }
                }
                Dialog::Recent(q) => {
                    use crate::recent::Outcome;
                    match q.ui(ui, &self.prefs.recent_projects) {
                        Outcome::None => {}
                        Outcome::Close => close = true,
                        Outcome::Open(path) => {
                            run = Some(Box::new(move |app, ctx| {
                                app.guard_discard(ctx, Pending::Open(Some(path)))
                            }));
                            close = true;
                        }
                        Outcome::Clear => {
                            run = Some(Box::new(|app, _| app.clear_recent()));
                            close = true;
                        }
                    }
                }
                Dialog::About => {
                    ui.heading("Kerf");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("A fast, precise audio editor. 32-bit float throughout, non-destructive, unlimited undo.");
                    ui.separator();
                    ui.label(egui::RichText::new(SHORTCUT_HELP).monospace().small());
                }
            });

        if open && !close {
            self.dialog = Some(dialog);
        }
        if let Some(f) = run {
            f(self, ctx);
        }
    }

    /// Track regions for split/CUE export: one per marker, snapped to CD
    /// sectors when CD-safe is on and the rate allows it.
    fn export_regions_for(&self, p: &Project) -> Vec<render::Region> {
        let regions = render::regions_from_markers(p, p.length());
        match render::sector_len(p.sample_rate).filter(|_| self.cd_safe) {
            Some(sector) => render::snap_to_sectors(&regions, sector),
            None => regions,
        }
    }

    /// Analyse the selection (or everything) in the background, then open the
    /// Detect Song Starts dialog.
    fn detect_songs(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let len = self.project().length();
        if len == 0 {
            self.error("Nothing to analyse");
            return;
        }
        let (start, end) = self.sel.range.filter(|(a, b)| b > a).unwrap_or((0, len));
        let tracks = self.target_tracks();
        let project = self.project().clone();
        let revision = self.doc.revision();
        self.job = Some(Job::spawn(ctx, "Analysing recording…", move |ctl| {
            let r =
                kerf_core::detect::analyze(&project, &tracks, start, end, &mut |p| ctl.progress(p))
                    .map_err(|e| e.to_string());
            JobDone::Analyzed(r, tracks, revision)
        }));
    }

    /// Re-read cache statistics into the open Audio Cache dialog.
    fn refresh_cache_view(&mut self, note: Option<String>) {
        let stats = self.cache.as_ref().and_then(|c| c.stats().ok());
        if let Some(Dialog::AudioCache(view)) = &mut self.dialog {
            view.stats = stats;
            view.note = note;
        }
    }

    fn clear_unused_cache(&mut self) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let note = match cache.clear_unused() {
            Ok(freed) => format!("Freed {}.", human_bytes(freed)),
            Err(e) => format!("Could not clear the cache: {e}"),
        };
        self.info(note.clone());
        self.refresh_cache_view(Some(note));
    }

    fn run_normalize(&mut self, s: NormalizeSettings) {
        let tracks = self.target_tracks();
        let range = self.sel.range.filter(|(a, b)| b > a);
        let label = format!("Normalize to {:.1} {}", s.target_db, s.mode.unit());
        if let Some(results) = self.edit(&label, |p| edit::normalize(p, &tracks, range, &s)) {
            let applied: Vec<String> = results
                .iter()
                .filter_map(|r| r.gain_db.map(|g| format!("{g:+.2} dB")))
                .collect();
            self.info(format!("{label}: applied {}", applied.join(", ")));
        }
    }

    fn job_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        egui::Window::new(job.title.clone())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.add(
                    egui::ProgressBar::new(job.progress())
                        .show_percentage()
                        .desired_width(300.0),
                );
                if job.is_cancelling() {
                    ui.label("Cancelling…");
                } else if ui.button("Cancel").clicked() {
                    job.cancel();
                }
            });
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let name = self
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let dirty = if self.doc.is_dirty() && !self.project().tracks.is_empty() {
            "• "
        } else {
            ""
        };
        let title = format!("{dirty}{name} — Kerf");
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

impl eframe::App for KerfApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Close requests (window X) go through the unsaved-changes check.
        if ctx.input(|i| i.viewport().close_requested()) {
            if !self.allow_close && self.doc.is_dirty() && !self.project().tracks.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Some(Dialog::ConfirmDiscard(Pending::Quit));
            } else {
                // A clean exit: nothing to recover next time.
                self.player.stop();
                if let Some(r) = &mut self.recovery {
                    r.clear();
                }
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.open_paths(&ctx, dropped);
        }

        self.poll_job(&ctx);
        self.poll_assistant(&ctx);
        self.shortcuts(&ctx);

        // A scrub lasts only as long as its drag.
        if self.player.is_scrubbing()
            && !matches!(self.drag, Some(crate::timeline::Drag::Scrub { .. }))
        {
            self.player.stop();
        }

        // Keep playback in sync with edits and update meters.
        if self.player.is_playing() {
            if self.doc.revision() != self.play_revision {
                self.play_revision = self.doc.revision();
                self.player.update(Arc::new(self.project().clone()));
            }
            ctx.request_repaint();
        }
        self.stats.poll_live(&mut self.player);
        if self.prefs.show_stats_panel && self.player.is_playing() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let m = self.player.take_meters();
        for (cur, new) in self.meters.iter_mut().zip(m) {
            *cur = new.max(*cur * 0.85);
        }
        if self.meters.iter().any(|&v| v > 1e-4) {
            ctx.request_repaint();
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        let (markers, history, stats) = (
            self.prefs.show_markers_panel,
            self.prefs.show_history_panel,
            self.prefs.show_stats_panel,
        );
        if markers || history || stats {
            egui::Panel::right("side")
                .resizable(true)
                .default_size(340.0)
                .show(ui, |ui| {
                    // Statistics on top, sized to fit; Markers fill the middle;
                    // History below them, or filling the rest on its own.
                    if stats && (markers || history) {
                        egui::Panel::top("stats")
                            .resizable(false)
                            .show(ui, |ui| crate::stats_panel::show(self, ui));
                    }
                    if markers && history {
                        egui::Panel::bottom("history")
                            .resizable(true)
                            .default_size(ui.available_height() * 0.4)
                            .show(ui, |ui| crate::history_panel::show(self, ui));
                    }
                    if markers {
                        crate::markers_panel::show(self, ui);
                    } else if history {
                        crate::history_panel::show(self, ui);
                    } else {
                        crate::stats_panel::show(self, ui);
                    }
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0.0))
            .show(ui, |ui| crate::timeline::show(self, ui));

        self.dialogs(&ctx);
        self.job_window(&ctx);
        self.update_title(&ctx);
        self.screenshot_hook(&ctx);
        if let Some(r) = &mut self.recovery {
            r.tick(&self.doc);
            if self.prefs.autosave_secs > 0 {
                ctx.request_repaint_after(std::time::Duration::from_secs(self.prefs.autosave_secs));
            }
        }

        // Finish coalesced gestures (fader drags, name edits) once input settles.
        if !ctx.input(|i| i.pointer.any_down()) && !ctx.egui_wants_keyboard_input() {
            self.doc.end_merge();
        }
    }
}

/// Debug/test aid: with `KERF_SCREENSHOT=out.ppm`, save a screenshot of the
/// window once loading has settled, then quit.
struct ScreenshotHook {
    path: PathBuf,
    frames: u32,
    /// `KERF_SCREENSHOT_AFTER=<secs>`: wait at least this long (e.g. for an
    /// assistant driving the app) before taking the screenshot.
    after: Option<std::time::Instant>,
    done: Arc<std::sync::atomic::AtomicBool>,
}

impl KerfApp {
    fn screenshot_hook(&mut self, ctx: &egui::Context) {
        use std::sync::atomic::Ordering;
        let Some(hook) = &mut self.screenshot else {
            return;
        };
        ctx.request_repaint();
        if hook.done.load(Ordering::Relaxed) {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.job.is_none() {
            hook.frames += 1;
        }
        if hook.frames == 10 {
            let action = match std::env::var("KERF_SCREENSHOT_DIALOG").as_deref() {
                Ok("cache") => Some(Action::AudioCache),
                Ok("detect") => Some(Action::DetectSongs),
                Ok("split") => Some(Action::Export(ExportKind::Markers)),
                Ok("prefs") => Some(Action::Preferences),
                Ok("recent") => Some(Action::OpenRecent),
                Ok("album") => Some(Action::Album),
                Ok("split-flac") => {
                    self.export_settings = ExportSettings::FLAC_ALBUM;
                    Some(Action::Export(ExportKind::Markers))
                }
                Ok("precision") => {
                    self.prefs.time_format = kerf_core::util::TimeFormat::CdFrames;
                    self.view.vzoom = 4.0;
                    // A song selected (as double-clicking between markers does).
                    let p = self.doc.project();
                    let at = 48_000 * 6;
                    let a = p
                        .markers
                        .iter()
                        .rev()
                        .find(|m| m.position <= at)
                        .map_or(0, |m| m.position);
                    let b = p
                        .markers
                        .iter()
                        .find(|m| m.position > at)
                        .map_or(p.length(), |m| m.position);
                    self.sel.range = Some((a, b));
                    self.cursor = a;
                    None
                }
                Ok("db") => {
                    self.view.db_scale = true;
                    None
                }
                Ok("loop") => {
                    self.loop_playback = true;
                    None
                }
                Ok("recover") => {
                    if let Some(Dialog::Recover(orphans)) = &mut self.dialog {
                        let o = orphans.remove(0);
                        self.dialog = None;
                        hook.frames += 1;
                        self.recover(ctx, o);
                        return;
                    }
                    None
                }
                Ok("split-cd") => {
                    self.export_settings = ExportSettings::AUDIO_CD;
                    Some(Action::Export(ExportKind::Markers))
                }
                Ok("split-raw") => {
                    self.cd_safe = false;
                    Some(Action::Export(ExportKind::Markers))
                }
                _ => None,
            };
            if let Some(a) = action {
                hook.frames += 1; // fire once, even though the action starts a job
                self.perform(ctx, a);
                return;
            }
        }
        if hook.frames == 20 && hook.after.is_some_and(|t| std::time::Instant::now() < t) {
            hook.frames = 19; // keep waiting (repaints are requested above)
            return;
        }
        if hook.frames == 20 {
            let (path, done) = (hook.path.clone(), hook.done.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |img| {
                    let [w, h] = img.size;
                    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
                    for px in &img.pixels {
                        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
                    }
                    if let Err(e) = std::fs::write(&path, out) {
                        eprintln!("screenshot failed: {e}");
                    }
                    done.store(true, Ordering::Relaxed);
                }),
            ));
        }
    }
}

#[derive(PartialEq)]
enum Preset {
    AudioCd,
    FlacAlbum,
    Archive,
    Custom,
}

/// Export format controls with presets. `cd_safe` is shown for exports that
/// have track boundaries (split, CUE).
fn export_settings_ui(
    ui: &mut egui::Ui,
    p: &Project,
    s: &mut ExportSettings,
    cd_safe: Option<&mut bool>,
) {
    let (pr, pc) = (p.sample_rate, p.channels);
    use kerf_core::encode::Container;
    let preset = if s.is_audio_cd(pr, pc) {
        Preset::AudioCd
    } else if s.container == Container::Flac
        && s.format == SampleFormat::Int24
        && s.rate_for(pr) == pr
        && s.channels_for(pc) == pc
    {
        Preset::FlacAlbum
    } else if s.container == Container::Wav
        && s.format.is_float()
        && s.rate_for(pr) == pr
        && s.channels_for(pc) == pc
    {
        Preset::Archive
    } else {
        Preset::Custom
    };
    let mut chosen = None;
    egui::Grid::new("exp")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Preset");
            let label = match preset {
                Preset::AudioCd => "Audio CD",
                Preset::FlacAlbum => "Lossless album (FLAC 24-bit)",
                Preset::Archive => "Archive (32-bit float)",
                Preset::Custom => "Custom",
            };
            egui::ComboBox::from_id_salt("preset")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(preset == Preset::AudioCd, "Audio CD")
                        .on_hover_text(
                            "16-bit, 44.1 kHz stereo, dithered, CD-safe track boundaries",
                        )
                        .clicked()
                    {
                        chosen = Some(Preset::AudioCd);
                    }
                    if ui
                        .selectable_label(
                            preset == Preset::FlacAlbum,
                            "Lossless album (FLAC 24-bit)",
                        )
                        .on_hover_text(
                            "FLAC, 24-bit at the project rate, tagged with album and track details",
                        )
                        .clicked()
                    {
                        chosen = Some(Preset::FlacAlbum);
                    }
                    if ui
                        .selectable_label(preset == Preset::Archive, "Archive (32-bit float)")
                        .on_hover_text(
                            "Lossless 32-bit float at the project's own rate and channels",
                        )
                        .clicked()
                    {
                        chosen = Some(Preset::Archive);
                    }
                });
            ui.end_row();

            ui.label("File type");

            egui::ComboBox::from_id_salt("container")
                .selected_text(s.container.label())
                .show_ui(ui, |ui| {
                    for c in Container::ALL {
                        ui.selectable_value(&mut s.container, c, c.label());
                    }
                });

            if !s.container.supports(s.format) {
                s.format = SampleFormat::Int24; // FLAC is 16 or 24-bit
            }

            ui.end_row();

            ui.label("Format");

            egui::ComboBox::from_id_salt("fmt")
                .selected_text(s.format.label())
                .show_ui(ui, |ui| {
                    for f in SampleFormat::ALL
                        .into_iter()
                        .filter(|f| s.container.supports(*f))
                    {
                        ui.selectable_value(&mut s.format, f, f.label());
                    }
                });
            ui.end_row();

            ui.label("Sample rate");
            let rate_label = |r: Option<u32>| match r {
                None => format!("Project ({pr} Hz)"),
                Some(r) => format!("{r} Hz"),
            };
            egui::ComboBox::from_id_salt("rate")
                .selected_text(rate_label(s.sample_rate))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.sample_rate, None, rate_label(None));
                    for r in [44_100, 48_000, 88_200, 96_000] {
                        if r != pr {
                            ui.selectable_value(&mut s.sample_rate, Some(r), rate_label(Some(r)));
                        }
                    }
                });
            ui.end_row();

            ui.label("Channels");
            let ch_name = |n: u16| match n {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                n => format!("{n} channels"),
            };
            egui::ComboBox::from_id_salt("chans")
                .selected_text(match s.channels {
                    None => format!("Project ({})", ch_name(pc)),
                    Some(n) => ch_name(n),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut s.channels,
                        None,
                        format!("Project ({})", ch_name(pc)),
                    );
                    for n in [1u16, 2] {
                        if n != pc {
                            ui.selectable_value(&mut s.channels, Some(n), ch_name(n));
                        }
                    }
                });
            ui.end_row();

            ui.label("Dither");
            ui.add_enabled(
                !s.format.is_float(),
                egui::Checkbox::new(&mut s.dither, "TPDF dither"),
            );
            ui.end_row();
        });
    // Same-as-project choices are stored as "project" so presets stay recognisable.
    if s.sample_rate == Some(pr) {
        s.sample_rate = None;
    }
    if s.channels == Some(pc) {
        s.channels = None;
    }

    let mut cd_safe = cd_safe;
    match chosen {
        Some(Preset::AudioCd) => {
            *s = ExportSettings::AUDIO_CD;
            if let Some(c) = cd_safe.as_deref_mut() {
                *c = true;
            }
        }
        Some(Preset::FlacAlbum) => *s = ExportSettings::FLAC_ALBUM,
        Some(Preset::Archive) => *s = ExportSettings::default(),
        _ => {}
    }

    let out_rate = s.rate_for(pr);
    if s.container == Container::Flac {
        ui.label(
            egui::RichText::new(
                "Tagged with the album details and track titles from File → Album….",
            )
            .weak()
            .small(),
        );
    }
    if s.format.is_float() {
        ui.label(
            egui::RichText::new(
                "32-bit float preserves the full mix, including peaks above 0 dBFS.",
            )
            .weak()
            .small(),
        );
    } else if out_rate != pr {
        ui.label(
            egui::RichText::new(format!(
                "Converted from {pr} Hz to {out_rate} Hz with high-quality resampling."
            ))
            .weak()
            .small(),
        );
    }
    if let Some(c) = cd_safe {
        ui.separator();
        cd_safe_ui(ui, p, c, out_rate);
    }
}

/// Where [`time_field`] records its height, for aligning neighbours.
fn time_field_height_id(name: &str) -> egui::Id {
    egui::Id::unique(("time-field-height", name))
}

/// An editable time display. Shows `value` (or "–") formatted; click to type,
/// Enter applies, Escape or clicking away cancels. Returns the typed time
/// when committed, and whether the time format was changed (right-click menu).
#[allow(clippy::too_many_arguments)]
fn time_field(
    ui: &mut egui::Ui,
    name: &str,
    value: Option<Frame>,
    width: f32,
    size: f32,
    editing: &mut Option<(egui::Id, String)>,
    sample_rate: u32,
    format: &mut kerf_core::util::TimeFormat,
) -> (Option<Frame>, bool) {
    let fmt = *format;
    let id = egui::Id::unique(("time-field", name));
    let shown = value
        .map(|v| kerf_core::util::format_time_as(v, sample_rate, fmt))
        .unwrap_or_default();
    let is_editing = editing.as_ref().is_some_and(|(i, _)| *i == id);
    let mut text = if is_editing {
        editing.as_ref().unwrap().1.clone()
    } else {
        shown.clone()
    };
    let r = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .id(id)
                .font(egui::FontId::monospace(size))
                .desired_width(width)
                .hint_text("–"),
        )
        .on_hover_text(
            "Click to type a time (Enter applies). Right-click to change the time format.",
        );
    let h = r.rect.height();
    ui.ctx()
        .data_mut(|d| d.insert_temp(time_field_height_id(name), h));
    if r.gained_focus() && !is_editing {
        *editing = Some((id, shown));
    } else if r.changed() {
        *editing = Some((id, text.clone()));
    }
    let mut out = None;
    if r.lost_focus() {
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            out = kerf_core::util::parse_time(&text, sample_rate, fmt);
        }
        if editing.as_ref().is_some_and(|(i, _)| *i == id) {
            *editing = None;
        }
    }
    let mut changed = false;
    r.context_menu(|ui| {
        ui.label(egui::RichText::new("Time format").strong());
        for f in kerf_core::util::TimeFormat::ALL {
            if ui.radio(fmt == f, f.label()).clicked() {
                *format = f;
                changed = true;
                ui.close();
            }
        }
    });
    (out, changed)
}

/// The Preferences form.
fn prefs_ui(ui: &mut egui::Ui, draft: &mut PrefsDraft, fallback_note: Option<&str>) {
    let p = &mut draft.prefs;
    ui.heading("Audio");
    egui::Grid::new("prefs-audio")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Output device");
            let current = match &p.output_device {
                None => "System default".to_string(),
                Some(id) => draft
                    .devices
                    .iter()
                    .find(|(d, _)| d == id)
                    .map_or_else(|| format!("{id} (not connected)"), |(_, name)| name.clone()),
            };
            egui::ComboBox::from_id_salt("device")
                .selected_text(current)
                .width(280.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut p.output_device, None, "System default");
                    for (id, name) in &draft.devices {
                        ui.selectable_value(&mut p.output_device, Some(id.clone()), name);
                    }
                });
            ui.end_row();
            ui.label("Buffer size");
            let label =
                |b: Option<u32>| b.map_or("Device default".to_string(), |n| format!("{n} frames"));
            egui::ComboBox::from_id_salt("buffer")
                .selected_text(label(p.buffer_frames))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut p.buffer_frames, None, label(None));
                    for n in [128u32, 256, 512, 1024, 2048] {
                        ui.selectable_value(&mut p.buffer_frames, Some(n), label(Some(n)));
                    }
                });
            ui.end_row();
        });
    ui.label(
        egui::RichText::new(
            "Smaller buffers respond faster; larger ones are safer on busy systems.",
        )
        .weak()
        .small(),
    );
    if let Some(note) = fallback_note {
        ui.colored_label(ui.visuals().warn_fg_color, note);
    }

    ui.separator();
    ui.heading("Playback");
    ui.checkbox(
        &mut p.space_returns_to_start,
        "Space stops and returns to where playback started",
    );
    ui.label(
        egui::RichText::new(if p.space_returns_to_start {
            "Shift+Space stops at the playhead instead."
        } else {
            "Space stops at the playhead; Shift+Space returns to the start."
        })
        .weak()
        .small(),
    );
    ui.horizontal(|ui| {
        ui.label("Preview around cursor (P):");
        ui.add(
            egui::DragValue::new(&mut p.preview_before)
                .range(0.0..=30.0)
                .speed(0.1)
                .suffix(" s before"),
        );
        ui.add(
            egui::DragValue::new(&mut p.preview_after)
                .range(0.0..=30.0)
                .speed(0.1)
                .suffix(" s after"),
        );
    });

    ui.separator();
    ffmpeg_ui(ui, draft);
    ui.separator();
    assistant_ui(ui, draft);

    let p = &mut draft.prefs;
    ui.separator();
    ui.heading("Recovery");
    ui.horizontal(|ui| {
        ui.label("Save recovery snapshots of unsaved work");
        let label = |s: u64| match s {
            0 => "Off".to_string(),
            s if s < 60 => format!("every {s} s"),
            s => format!("every {} min", s / 60),
        };
        egui::ComboBox::from_id_salt("autosave")
            .selected_text(label(p.autosave_secs))
            .show_ui(ui, |ui| {
                for s in [0u64, 30, 60, 300] {
                    ui.selectable_value(&mut p.autosave_secs, s, label(s));
                }
            });
    });
}

/// Preferences: which FFmpeg decodes what the built-in decoder can't.
fn ffmpeg_ui(ui: &mut egui::Ui, draft: &mut PrefsDraft) {
    use kerf_core::decode::FfmpegSetting;
    ui.heading("Decoding");
    ui.label(
        egui::RichText::new(
            "Kerf decodes WAV, FLAC, MP3, Ogg Vorbis, AAC, ALAC and AIFF itself. FFmpeg, if \
             installed, adds HE-AAC at full quality (common for radio recordings), AC-3, WMA, \
             APE, WavPack and more.",
        )
        .weak()
        .small(),
    );
    let p = &mut draft.prefs;
    let custom = matches!(p.ffmpeg, FfmpegSetting::Path(_));
    ui.horizontal(|ui| {
        if ui
            .radio(p.ffmpeg == FfmpegSetting::Auto, "Find FFmpeg automatically")
            .clicked()
        {
            p.ffmpeg = FfmpegSetting::Auto;
        }
        if ui.radio(custom, "Use this ffmpeg:").clicked() && !custom {
            p.ffmpeg = FfmpegSetting::Path(draft.ffmpeg_path.clone().into());
        }
        if ui.radio(p.ffmpeg == FfmpegSetting::Off, "Off").clicked() {
            p.ffmpeg = FfmpegSetting::Off;
        }
    });
    if custom {
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut draft.ffmpeg_path)
                    .desired_width(320.0)
                    .hint_text("/path/to/ffmpeg"),
            );
            if ui.button("Browse…").clicked() {
                if let Some(f) = rfd::FileDialog::new()
                    .set_title("Choose ffmpeg")
                    .pick_file()
                {
                    draft.ffmpeg_path = f.display().to_string();
                }
            }
            if r.changed() || draft.ffmpeg_path != p_path(&p.ffmpeg) {
                p.ffmpeg = FfmpegSetting::Path(draft.ffmpeg_path.trim().into());
            }
        });
    }
    let env = FfmpegSetting::from_env();
    let effective = env.clone().unwrap_or_else(|| p.ffmpeg.clone());
    if draft.ffmpeg_status.as_ref().map(|(s, _)| s) != Some(&effective) {
        let text = match effective.locate() {
            Some(f) => format!(
                "Using {} ({})",
                f.ffmpeg.display(),
                f.version().unwrap_or_else(|| "version unknown".into())
            ),
            None if effective == FfmpegSetting::Off => {
                "Off: HE-AAC imports without its upper frequencies; formats only FFmpeg reads \
                 can't be imported."
                    .into()
            }
            None => "FFmpeg not found (ffmpeg and ffprobe are both needed).".into(),
        };
        draft.ffmpeg_status = Some((effective.clone(), text));
    }
    if let Some((_, text)) = &draft.ffmpeg_status {
        let found = text.starts_with("Using");
        let color = if found {
            ui.visuals().weak_text_color()
        } else {
            ui.visuals().warn_fg_color
        };
        ui.colored_label(color, text);
    }
    if env.is_some() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "The KERF_FFMPEG environment variable overrides this setting.",
        );
    }
}

/// Preferences: AI assistant access over MCP.
fn assistant_ui(ui: &mut egui::Ui, draft: &mut PrefsDraft) {
    ui.heading("Assistant (MCP)");
    ui.checkbox(
        &mut draft.prefs.assistant_enabled,
        "Let AI assistants work on the open project",
    );
    ui.label(
        egui::RichText::new(
            "Assistants such as Claude connect through kerf-mcp. You see every edit on the \
             timeline as it happens, each one an undo step labelled \"AI: …\". Only programs \
             run by you on this computer can connect.",
        )
        .weak()
        .small(),
    );
    if !draft.prefs.assistant_enabled {
        return;
    }
    ui.label("Folders it may read and write (one per line; empty: your home folder):");
    if ui
        .add(
            egui::TextEdit::multiline(&mut draft.assistant_folders)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text(
                    dirs::home_dir()
                        .map(|h| h.display().to_string())
                        .unwrap_or_default(),
                ),
        )
        .changed()
    {
        draft.prefs.assistant_folders = draft
            .assistant_folders
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    ui.label("Add this to your assistant's MCP configuration:");
    let config = crate::assistant::client_config();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&config).monospace().small());
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(config.clone());
        }
    });
}

fn p_path(s: &kerf_core::decode::FfmpegSetting) -> String {
    match s {
        kerf_core::decode::FfmpegSetting::Path(p) => p.display().to_string(),
        _ => String::new(),
    }
}

/// "5 minutes ago" style description.
fn ago(t: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(t)
        .map_or(0, |d| d.as_secs());
    match secs {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} minutes ago", (secs + 30) / 60),
        5400..=172_799 => format!("{} hours ago", (secs + 1800) / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// The CD-safe option with a live count of sector boundary errors.
fn cd_safe_ui(ui: &mut egui::Ui, p: &Project, cd_safe: &mut bool, out_rate: u32) {
    let Some(sector) = render::sector_len(p.sample_rate) else {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "{} Hz has no whole CD sectors, so tracks can't be made CD-safe.",
                p.sample_rate
            ),
        );
        return;
    };
    ui.checkbox(cd_safe, "CD-safe track boundaries")
        .on_hover_text(
        "Moves each split point by at most 1/150 s to a CD sector boundary. Your markers stay put.",
    );
    let regions = render::regions_from_markers(p, p.length());
    let errors = render::sector_errors(&regions, sector);
    let tracks = regions.len().saturating_sub(1);
    let text = if *cd_safe {
        "No sector boundary errors: burns gaplessly as separate tracks or with the CUE sheet."
            .to_string()
    } else if errors == 0 {
        "No sector boundary errors.".to_string()
    } else {
        format!(
            "{errors} of {tracks} track boundaries would cause sector boundary errors: burned as separate \
             files, silence is padded between tracks (audible on gapless live albums)."
        )
    };
    let color = if !*cd_safe && errors > 0 {
        ui.visuals().warn_fg_color
    } else {
        ui.visuals().weak_text_color()
    };
    ui.label(egui::RichText::new(text).color(color).small());
    if out_rate != 44_100 {
        ui.label(
            egui::RichText::new(
                "Audio CDs are 44.1 kHz: choose the Audio CD preset to burn these files directly.",
            )
            .weak()
            .small(),
        );
    }
}

fn entries(n: usize) -> String {
    if n == 1 {
        "1 entry".into()
    } else {
        format!("{n} entries")
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} bytes")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Show a folder in the platform file manager.
fn open_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
}

/// Work requested from inside a dialog, run once the dialog UI is done.
type DeferredAction = Box<dyn FnOnce(&mut KerfApp, &egui::Context)>;

fn meter(ui: &mut egui::Ui, levels: [f32; 2], channels: u16) {
    let n = if channels == 1 { 1 } else { 2 };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(160.0, 18.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let h = rect.height() / n as f32;
    for (i, &level) in levels.iter().take(n).enumerate() {
        let db = kerf_core::util::gain_to_db(level as f64).max(-60.0);
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f32;
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + i as f32 * h + 1.0),
            egui::vec2(rect.width() * frac, h - 2.0),
        );
        let color = if level >= 1.0 {
            egui::Color32::from_rgb(230, 60, 60)
        } else if db > -6.0 {
            egui::Color32::from_rgb(230, 190, 60)
        } else {
            egui::Color32::from_rgb(80, 200, 120)
        };
        painter.rect_filled(bar, 1.0, color);
    }
}

const SHORTCUT_HELP: &str = "\
Space        play / stop          Shift+Space  stop at playhead (or the reverse)
P            preview around cursor            L     loop
← / →        nudge cursor (skip 5 s while playing)
Shift+← / →  extend selection     Ctrl+← / →   previous / next edit point
↑ / ↓        previous / next track (Shift adds to the selection)
Home / End   start / end (Shift extends)      [ / ]  selection start / end
PgUp / PgDn  scroll a screen      + / - / Z    zoom in / out / to selection
S            split                Del / ⌫      delete (close gap)
Ctrl+X/C/V   cut / copy / paste   Ctrl+L       silence (keep gap)
Ctrl+Z       undo                 Ctrl+⇧Z      redo
Ctrl+M       normalize            Ctrl+G       gain
I / O        fade in / out        Ctrl+R       render in place
M            add marker           , / .        previous / next marker
Ctrl+⇧M      markers panel        Ctrl+⇧H      history panel
Ctrl+⇧I      statistics panel     Ctrl+,       preferences
Ctrl+↑ / ↓   vertical zoom        D            waveform in dB
Shift+Z      selection to zero crossings
Ctrl+E       export mix           Ctrl+⇧E      export split at markers
Ctrl+O       open project         Ctrl+⇧O      open a recent project

Mouse: drag in a track to select · drag a clip's title bar to move it ·
drag clip edges to trim · drag the squares at a clip's top corners for fades ·
drag selection edges to adjust · double-click between markers to select a song ·
click the time fields to type a time (right-click for the format) · Alt+wheel vertical zoom ·
Ctrl+wheel zoom · Shift+wheel scroll · Alt disables snapping.";

/// Fastest J / L shuttle speed.
const MAX_SHUTTLE_SPEED: f64 = 8.0;

/// Shuttle speed for the toolbar, e.g. "» 2×" or "« ½×".
fn speed_label(speed: f64) -> String {
    let dir = if speed < 0.0 { '«' } else { '»' };
    let mag = speed.abs();
    if mag == 0.5 {
        format!("{dir} ½×")
    } else {
        format!("{dir} {mag}×")
    }
}
