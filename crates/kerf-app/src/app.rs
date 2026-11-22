//! Application state, actions, menus, dialogs.

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
    pub track_height: f32,
    pub follow: bool,
    pub snap: bool,
    /// Lane width in points, measured each frame.
    pub lane_width: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            spp: 512.0,
            scroll: 0.0,
            vscroll: 0.0,
            track_height: 120.0,
            follow: true,
            snap: true,
            lane_width: 1000.0,
        }
    }
}

impl View {
    pub const MIN_SPP: f64 = 1.0 / 64.0;

    /// Zoom by `factor` keeping the frame under lane x-offset `anchor_px` fixed.
    pub fn zoom(&mut self, factor: f64, anchor_px: f32) {
        let anchor_frame = self.scroll + anchor_px as f64 * self.spp;
        self.spp = (self.spp * factor).clamp(Self::MIN_SPP, 1e7);
        self.scroll = (anchor_frame - anchor_px as f64 * self.spp).max(0.0);
    }

    pub fn show_range(&mut self, start: Frame, end: Frame) {
        let len = end.saturating_sub(start).max(1) as f64;
        self.spp = (len / (self.lane_width as f64 * 0.96)).clamp(Self::MIN_SPP, 1e7);
        self.scroll = (start as f64 - self.lane_width as f64 * 0.02 * self.spp).max(0.0);
    }

    pub fn visible_frames(&self) -> f64 {
        self.lane_width as f64 * self.spp
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKind {
    Mix,
    Selection,
    Markers,
    Stems,
}

impl ExportKind {
    fn title(self) -> &'static str {
        match self {
            Self::Mix => "Export Mix",
            Self::Selection => "Export Selection",
            Self::Markers => "Export Split at Markers",
            Self::Stems => "Export Tracks as Stems",
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
    About,
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
    GoStart,
    GoEnd,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomSelection,
    ToggleLoop,
    ToggleSnap,
    AudioCache,
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
    pub fade_shape: FadeShape,
    pub paste_mode: PasteMode,
    pub export_settings: ExportSettings,
    pub status: Option<(String, bool)>,
    pub dialog: Option<Dialog>,
    pub job: Option<Job>,
    pub drag: Option<crate::timeline::Drag>,
    pub meters: [f32; 2],
    pub last_dir: Option<PathBuf>,
    /// Disk cache for decoded audio; `None` falls back to keeping audio in RAM.
    pub cache: Option<Arc<AudioCache>>,
    title: String,
    allow_close: bool,
    screenshot: Option<ScreenshotHook>,
}

impl KerfApp {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_theme(egui::Theme::Dark);
        // KERF_CACHE_DIR overrides the platform cache location (e.g. a faster drive).
        let opened = match std::env::var_os("KERF_CACHE_DIR") {
            Some(dir) => AudioCache::open(dir),
            None => AudioCache::open_default(),
        };
        let cache = match opened {
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
            fade_shape: FadeShape::EqualPower,
            paste_mode: PasteMode::Insert,
            export_settings: ExportSettings::default(),
            status: None,
            dialog: None,
            job: None,
            drag: None,
            meters: [0.0; 2],
            last_dir: None,
            cache,
            title: String::new(),
            allow_close: false,
            screenshot: std::env::var_os("KERF_SCREENSHOT").map(|p| ScreenshotHook {
                path: p.into(),
                frames: 0,
                done: Default::default(),
            }),
        };
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
    fn validate_selection(&mut self) {
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
                    Some((a, b)) => vec![a, b],
                    None => vec![if self.player.is_playing() {
                        self.player.position()
                    } else {
                        self.cursor
                    }],
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
            GoStart => self.seek(0),
            GoEnd => {
                let end = self.project().length();
                self.seek(end);
            }
            ZoomIn => self.view.zoom(0.5, self.view.lane_width / 2.0),
            ZoomOut => self.view.zoom(2.0, self.view.lane_width / 2.0),
            ZoomFit => {
                let len = self
                    .project()
                    .length()
                    .max(self.project().sample_rate as Frame);
                self.view.show_range(0, len);
            }
            ZoomSelection => {
                if let Some((a, b)) = self.require_range() {
                    self.view.show_range(a, b);
                }
            }
            ToggleLoop => self.loop_playback = !self.loop_playback,
            ToggleSnap => self.view.snap = !self.view.snap,
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
            self.player.stop();
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

    /// Move the cursor; if playing, continue playing from there.
    pub fn seek(&mut self, frame: Frame) {
        self.cursor = frame;
        if self.player.is_playing() {
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
                            out.push(Ok(Prepared {
                                path,
                                name,
                                audio,
                                peaks,
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
            ExportKind::Mix | ExportKind::Selection => dlg
                .add_filter("WAV audio", &["wav"])
                .set_file_name(format!("{base}.wav"))
                .save_file()
                .map(|mut p| {
                    if p.extension().is_none() {
                        p.set_extension("wav");
                    }
                    p
                }),
            ExportKind::Markers | ExportKind::Stems => {
                dlg.set_title("Choose output folder").pick_folder()
            }
        };
        let Some(target) = target else { return };

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
                ExportKind::Markers => {
                    let regions = render::regions_from_markers(&project, len);
                    render::export_regions(
                        &project,
                        &regions,
                        &RenderOptions::default(),
                        &target,
                        &base,
                        settings,
                        &mut progress,
                    )
                }
                ExportKind::Stems => {
                    render::export_stems(&project, 0, len, &target, settings, &mut progress)
                }
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
                        self.info(format!("Imported {n} file(s)"));
                    }
                }
                if !errors.is_empty() {
                    self.error(errors.join("; "));
                }
            }
            JobDone::Loaded(path, r) => match r {
                Ok(project) => {
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
        let table: &[(Modifiers, Key, Action)] = &[
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
            (Modifiers::NONE, Key::Space, Action::PlayStop),
            (Modifiers::NONE, Key::S, Action::Split),
            (Modifiers::NONE, Key::M, Action::AddMarker),
            (Modifiers::NONE, Key::L, Action::ToggleLoop),
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
        ctx.input_mut(|i| {
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
        for a in fired {
            self.perform(ctx, a);
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut act = None;
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
            ui.menu_button("Markers", |ui| {
                item(ui, "Add Marker", "M", Action::AddMarker, &mut act);
                item(ui, "Previous Marker", ",", Action::PrevMarker, &mut act);
                item(ui, "Next Marker", ".", Action::NextMarker, &mut act);
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
                ui.checkbox(&mut self.view.snap, "Snap to edges & markers");
                ui.checkbox(&mut self.view.follow, "Follow playhead");
                ui.add(
                    egui::Slider::new(&mut self.view.track_height, 60.0..=400.0)
                        .text("Track height"),
                );
            });
            ui.menu_button("Help", |ui| {
                item(ui, "About Kerf", "", Action::About, &mut act)
            });
        });
        if let Some(a) = act {
            self.perform(&ctx, a);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
            let playing = self.player.is_playing();
            if ui.button("⏮").on_hover_text("Go to start (Home)").clicked() {
                self.perform(&ctx, Action::GoStart);
            }
            let play = if playing { "⏹" } else { "▶" };
            if ui
                .button(play)
                .on_hover_text("Play / Stop (Space)")
                .clicked()
            {
                self.perform(&ctx, Action::PlayStop);
            }
            if ui.button("⏭").on_hover_text("Go to end (End)").clicked() {
                self.perform(&ctx, Action::GoEnd);
            }
            ui.toggle_value(&mut self.loop_playback, "🔁")
                .on_hover_text("Loop playback (L)");
            ui.separator();

            let sr = self.project().sample_rate;
            let pos = if playing {
                self.player.position()
            } else {
                self.cursor
            };
            ui.label(
                egui::RichText::new(format_time(pos, sr))
                    .monospace()
                    .size(20.0),
            );
            ui.separator();
            if let Some((a, b)) = self.sel.range {
                ui.label(
                    egui::RichText::new(format!(
                        "Sel {} – {}  ({})",
                        format_time(a, sr),
                        format_time(b, sr),
                        format_time(b - a, sr)
                    ))
                    .monospace(),
                );
                ui.separator();
            }
            ui.toggle_value(&mut self.view.snap, "🧲 Snap")
                .on_hover_text("Snap to clip edges, markers and cursor");
            if ui.button("🔍+").on_hover_text("Zoom in (+)").clicked() {
                self.perform(&ctx, Action::ZoomIn);
            }
            if ui.button("🔍−").on_hover_text("Zoom out (-)").clicked() {
                self.perform(&ctx, Action::ZoomOut);
            }
            if ui
                .button("⬌ Fit")
                .on_hover_text("Zoom to fit (Ctrl+F)")
                .clicked()
            {
                self.perform(&ctx, Action::ZoomFit);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                meter(ui, self.meters, self.project().channels);
            });
        });
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
            .max_width(540.0)
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
                            let regions = render::regions_from_markers(self.project(), self.project().length());
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
                        _ => {
                            ui.label("Renders all audible tracks with gain, pan, mute and solo.");
                        }
                    }
                    ui.separator();
                    egui::Grid::new("exp").num_columns(2).show(ui, |ui| {
                        ui.label("Format");
                        egui::ComboBox::from_id_salt("fmt").selected_text(self.export_settings.format.label()).show_ui(ui, |ui| {
                            for f in SampleFormat::ALL {
                                ui.selectable_value(&mut self.export_settings.format, f, f.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Dither");
                        ui.add_enabled(
                            !self.export_settings.format.is_float(),
                            egui::Checkbox::new(&mut self.export_settings.dither, "TPDF dither"),
                        );
                        ui.end_row();
                    });
                    if self.export_settings.format.is_float() {
                        ui.label(egui::RichText::new("32-bit float preserves the full mix, including peaks above 0 dBFS.").weak().small());
                    }
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
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if self.doc.is_dirty() && !self.project().tracks.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Some(Dialog::ConfirmDiscard(Pending::Quit));
            } else {
                self.player.stop();
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
        self.shortcuts(&ctx);

        // Keep playback in sync with edits and update meters.
        if self.player.is_playing() {
            if self.doc.revision() != self.play_revision {
                self.play_revision = self.doc.revision();
                self.player.update(Arc::new(self.project().clone()));
            }
            ctx.request_repaint();
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
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0.0))
            .show(ui, |ui| crate::timeline::show(self, ui));

        self.dialogs(&ctx);
        self.job_window(&ctx);
        self.update_title(&ctx);
        self.screenshot_hook(&ctx);

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
        if hook.frames == 10 && std::env::var("KERF_SCREENSHOT_DIALOG").as_deref() == Ok("cache") {
            self.perform(ctx, Action::AudioCache);
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
Space        play / stop          S       split at cursor
Del / ⌫      delete (close gap)   Ctrl+L  silence (keep gap)
Ctrl+X/C/V   cut / copy / paste   Ctrl+T  trim to selection
Ctrl+Z       undo                 Ctrl+⇧Z redo
Ctrl+M       normalize            Ctrl+G  gain
I / O        fade in / out        Ctrl+R  render in place
M            add marker           , / .   prev / next marker
+ / -        zoom                 Z       zoom to selection
L            loop                 N       snap on/off
Ctrl+E       export mix           Ctrl+⇧E split at markers

Mouse: drag in a track to select · drag a clip's title bar to move it ·
drag clip edges to trim · drag the squares at
a clip's top corners to set fades (overlaps crossfade automatically) · Ctrl+wheel zoom · Shift+wheel scroll ·
click track headers to select tracks (Ctrl/Shift to add).";
