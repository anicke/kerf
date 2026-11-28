//! The operations layer: every capability of the engine as a typed
//! [`Request`] and [`Response`] that convert to and from JSON.
//!
//! One implementation shared by the command-line tool, the MCP server and
//! (eventually) the GUI. A [`Session`] holds one live project with its undo
//! history; every edit is a single undoable step, and failed edits leave the
//! project untouched.
//!
//! ```no_run
//! # use kerf_core::ops::Session;
//! let mut s = Session::new(None);
//! println!("{}", s.execute_json(r#"{"op": "open", "path": "show.wav"}"#));
//! println!("{}", s.execute_json(r#"{"op": "detect_songs", "songs": 14, "apply": true}"#));
//! ```

#[cfg(feature = "schema")]
pub mod describe;
mod types;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use types::*;

use crate::album;
use crate::cache::AudioCache;
use crate::compare::{self, CompareOptions, DecoderReader, Empty, ProjectReader, Reader};
use crate::detect::{self, DetectSettings, Placement};
use crate::dither::Dither;
use crate::document::Document;
use crate::edit::{self, Clipboard, NormalizeMode, NormalizeSettings, PasteMode};
use crate::encode::Container;
use crate::io::{ExportSettings, SampleFormat};
use crate::project::{MarkerId, Project, TrackId};
use crate::render::{self, Progress, Region, RenderOptions};
use crate::session;
use crate::util::gain_to_db;
use crate::Frame;

type OpResult<T = Response> = std::result::Result<T, OpError>;

/// Finite dB values only: silence (`-inf`) becomes `None` (JSON has no infinity).
fn db(v: f64) -> Option<f64> {
    v.is_finite().then(|| (v * 100.0).round() / 100.0)
}

fn compare_summary(c: &CompareInfo, rate: u32) -> String {
    let mut s = compare_sentence(c, rate);
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    s
}

fn compare_sentence(c: &CompareInfo, rate: u32) -> String {
    let show = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.1}"));
    let mut s = if c.identical {
        format!(
            "Identical: {} and {} match bit for bit ({})",
            c.a, c.b, c.length_a.time
        )
    } else if c.nulls {
        format!(
            "{} and {} null: no difference above {} dBFS (largest {} dBFS)",
            c.a,
            c.b,
            show(c.threshold_db),
            show(c.peak_db)
        )
    } else if c.differing_frames == 0 {
        format!("{} and {} match where both have audio", c.a, c.b)
    } else {
        let span = match (&c.first_difference, &c.last_difference) {
            (Some(f), Some(l)) if c.region_count > 1 => format!(" from {} to {}", f.time, l.time),
            (Some(f), _) => format!(" at {}", f.time),
            _ => String::new(),
        };
        format!(
            "{} and {} differ in {} region(s){span}: peak difference {} dBFS, null depth {} dB",
            c.a,
            c.b,
            c.region_count,
            show(c.peak_db),
            show(c.null_depth_db)
        )
    };
    let (fa, fb) = (c.length_a.frames, c.length_b.frames);
    if fa != fb {
        let (longer, by) = if fb > fa {
            ("b", fb - fa)
        } else {
            ("a", fa - fb)
        };
        s += &format!(
            "; {longer} is longer by {} ({by} frames)",
            crate::util::format_time(by, rate)
        );
    }
    if let Some(al) = &c.alignment {
        s += &if al.found {
            format!(
                "; b shifted by {} frames ({:.3} s) to line up",
                al.offset_frames, al.offset_secs
            )
        } else {
            "; no alignment found (openings too different or silent), compared unshifted".into()
        };
    }
    s
}

fn done(summary: impl Into<String>) -> Response {
    Response {
        summary: summary.into(),
        data: Data::None,
    }
}

/// One live project with undo history, driven by [`Request`]s.
pub struct Session {
    doc: Document,
    cache: Option<Arc<AudioCache>>,
    clipboard: Option<Clipboard>,
    /// Name of the audio file opened as a project (for export file names).
    source_name: Option<String>,
    /// Prepended to every undo label (e.g. "AI: ").
    label_prefix: String,
    /// The project file as it was when opened or last saved, to notice
    /// changes made by something else meanwhile.
    file_stamp: Option<FileStamp>,
}

/// A hash of a file's contents (project files are small). Modification times
/// are too coarse on some file systems to tell two quick saves apart.
type FileStamp = u64;

fn file_stamp(path: &Path) -> Option<FileStamp> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

impl Session {
    /// A session with an empty project. With a cache, imported audio is
    /// memory-mapped from disk instead of held in RAM.
    pub fn new(cache: Option<Arc<AudioCache>>) -> Self {
        let mut s = Self {
            doc: Document::default(),
            cache,
            clipboard: None,
            source_name: None,
            label_prefix: String::new(),
            file_stamp: None,
        };
        s.reset(Project::default(), None);
        s
    }

    /// A session around an existing document (e.g. a snapshot of what an
    /// editor has open). Its history, path and saved state are kept.
    pub fn from_document(doc: Document, cache: Option<Arc<AudioCache>>) -> Self {
        Self {
            doc,
            cache,
            clipboard: None,
            source_name: None,
            label_prefix: String::new(),
            file_stamp: None,
        }
    }

    pub fn into_document(self) -> Document {
        self.doc
    }

    /// Prefix every undo step this session makes, e.g. "AI: Split at 1:00".
    pub fn set_label_prefix(&mut self, prefix: impl Into<String>) {
        self.label_prefix = prefix.into();
    }

    /// The clipboard of copy/cut/paste, to carry between sessions.
    pub fn take_clipboard(&mut self) -> Option<Clipboard> {
        self.clipboard.take()
    }

    pub fn set_clipboard(&mut self, clipboard: Option<Clipboard>) {
        self.clipboard = clipboard;
    }

    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn project(&self) -> &Project {
        self.doc.project()
    }

    fn reset(&mut self, mut project: Project, path: Option<PathBuf>) {
        project.cache = self.cache.clone();
        self.doc = Document::new(project);
        self.doc.path = path;
        self.clipboard = None;
        self.source_name = None;
        self.file_stamp = None;
    }

    /// Run a request given as JSON; returns `{"ok": true, "result": …}` or
    /// `{"ok": false, "error": {"code": …, "message": …}}`.
    pub fn execute_json(&mut self, json: &str) -> String {
        let out = match serde_json::from_str::<Request>(json) {
            Ok(req) => self.execute(req),
            Err(e) => Err(OpError::new(ErrorCode::InvalidRequest, e.to_string())),
        };
        envelope(&out).to_string()
    }

    pub fn execute(&mut self, req: Request) -> OpResult {
        self.execute_with_progress(req, &mut |_| true)
    }

    /// Run a request, reporting progress (0..=1) of long operations
    /// (analysis, export). Return `false` from `progress` to cancel.
    pub fn execute_with_progress(&mut self, req: Request, progress: Progress) -> OpResult {
        use Request as R;
        match req {
            R::New { discard } => {
                self.check_discard(discard)?;
                self.reset(Project::default(), None);
                Ok(done("New empty project"))
            }
            R::Open { path, discard } => {
                self.check_discard(discard)?;
                self.open(&path)
            }
            R::Import { path, at } => {
                let at = self.opt_time(at.as_ref())?.unwrap_or(0);
                let tid = self.apply("Import", |p| session::import_file(p, &path, at))?;
                let p = self.project();
                let warning = crate::io::decode_limitation(&path)
                    .map_or_else(String::new, |w| format!(". Warning: {w}"));
                Ok(Response {
                    summary: format!(
                        "Imported {} as track {} ({}, {} Hz, {} ch){warning}",
                        path.display(),
                        tid.0,
                        crate::util::format_time(
                            p.track(tid).map_or(0, |t| t.end() - at),
                            p.sample_rate
                        ),
                        p.sample_rate,
                        p.channels
                    ),
                    data: Data::Track { track: tid.0 },
                })
            }
            R::Save { path, force } => self.save(path, force),
            R::Undo => match self.doc.undo() {
                Some(l) => Ok(done(format!("Undid \"{l}\""))),
                None => Err(OpError::new(ErrorCode::NothingToUndo, "nothing to undo")),
            },
            R::Redo => match self.doc.redo() {
                Some(l) => Ok(done(format!("Redid \"{l}\""))),
                None => Err(OpError::new(ErrorCode::NothingToUndo, "nothing to redo")),
            },
            R::History => {
                let undo: Vec<String> = self.doc.history().map(String::from).collect();
                Ok(Response {
                    summary: format!("{} undoable step(s)", undo.len()),
                    data: Data::History(HistoryInfo {
                        undo,
                        redo: self.doc.redo_label().map(String::from),
                        dirty: self.doc.is_dirty(),
                    }),
                })
            }
            R::Info => Ok(self.info()),
            R::Analyze { start, end, tracks } => {
                let (a, b) = self.range(start.as_ref(), end.as_ref())?;
                let tracks = self.tracks(tracks.as_deref())?;
                self.analyze(a, b, tracks, progress)
            }
            R::DetectSongs {
                start,
                end,
                tracks,
                songs,
                sensitivity,
                min_song_secs,
                min_gap_secs,
                pre_roll_secs,
                placement,
                setlist,
                apply,
            } => {
                let defaults = DetectSettings::default();
                let settings = DetectSettings {
                    expected_songs: songs.filter(|&n| n > 0),
                    sensitivity: sensitivity.unwrap_or(defaults.sensitivity).clamp(0.0, 1.0),
                    min_song_secs: min_song_secs.unwrap_or(defaults.min_song_secs),
                    min_gap_secs: min_gap_secs.unwrap_or(defaults.min_gap_secs),
                    pre_roll_secs: pre_roll_secs.unwrap_or(defaults.pre_roll_secs),
                    placement: match placement {
                        SongPlacement::MusicStart => Placement::MusicStart,
                        SongPlacement::QuietestPoint => Placement::QuietestPoint,
                    },
                    marker_at_start: true,
                };
                let (a, b) = self.range(start.as_ref(), end.as_ref())?;
                let tracks = self.tracks(tracks.as_deref())?;
                self.detect(
                    a,
                    b,
                    &tracks,
                    &settings,
                    &setlist.join("\n"),
                    apply,
                    progress,
                )
            }

            req @ R::Compare { .. } => self.compare(req, progress),
            R::FindSilences {
                threshold_db,
                min_secs,
                start,
                end,
                tracks,
            } => {
                let (a, b) = self.range(start.as_ref(), end.as_ref())?;
                let t = self.tracks(tracks.as_deref())?;
                self.find_silences(
                    a,
                    b,
                    t,
                    threshold_db.unwrap_or(-50.0),
                    min_secs.unwrap_or(1.0).max(0.0),
                    progress,
                )
            }
            R::Waveform {
                start,
                end,
                tracks,
                width,
                height,
                db_scale,
            } => {
                let (a, b) = self.range(start.as_ref(), end.as_ref())?;
                let t = self.tracks(tracks.as_deref())?;
                let o = crate::waveform::Options {
                    width: width.unwrap_or(1200).clamp(200, 4000),
                    height: height.unwrap_or(300).clamp(100, 2000),
                    db_scale,
                };
                self.waveform(a, b, &t, &o, progress)
            }

            // --- Edit -------------------------------------------------------
            R::Split { at, tracks } => {
                let at = self.time(&at)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Split at {}", self.fmt(at)), |p| {
                    edit::split_at(p, &t, at)
                })
            }
            R::Delete { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Delete {}", self.fmt_span(a, b)), |p| {
                    edit::ripple_delete(p, &t, a, b)
                })
            }
            R::Clear { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Silence {}", self.fmt_span(a, b)), |p| {
                    edit::clear_range(p, &t, a, b)
                })
            }
            R::Crop { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Crop to {}", self.fmt_span(a, b)), |p| {
                    edit::crop(p, &t, a, b)
                })
            }
            R::InsertSilence { at, length, tracks } => {
                let at = self.time(&at)?;
                let len = self.time(&length)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(
                    format!("Insert {} of silence at {}", self.fmt(len), self.fmt(at)),
                    |p| edit::insert_silence(p, &t, at, len),
                )
            }
            R::Copy { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.clipboard = Some(edit::copy(self.project(), &t, a, b)?);
                Ok(done(format!("Copied {}", self.fmt_span(a, b))))
            }
            R::Cut { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                let clip = edit::copy(self.project(), &t, a, b)?;
                let r = self.edit(format!("Cut {}", self.fmt_span(a, b)), |p| {
                    edit::ripple_delete(p, &t, a, b)
                })?;
                self.clipboard = Some(clip);
                Ok(r)
            }
            R::Paste { at, tracks, mode } => {
                let at = self.time(&at)?;
                let t = self.tracks(tracks.as_deref())?;
                let clip = self.clipboard.clone().ok_or_else(|| {
                    OpError::new(ErrorCode::NothingSelected, "clipboard is empty")
                })?;
                let mode = match mode {
                    Paste::Insert => PasteMode::Insert,
                    Paste::Overwrite => PasteMode::Overwrite,
                    Paste::Mix => PasteMode::Mix,
                };
                let len = self.fmt(clip.length);
                self.edit(format!("Paste {len} at {}", self.fmt(at)), |p| {
                    edit::paste(p, &clip, &t, at, mode).map(|_| ())
                })
            }
            R::Gain {
                db: gain,
                start,
                end,
                tracks,
            } => {
                let range = self.opt_range(start.as_ref(), end.as_ref())?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Gain {gain:+.2} dB"), |p| {
                    edit::apply_gain(p, &t, range, gain)
                })
            }
            R::Normalize {
                mode,
                target,
                ceiling_db,
                independent,
                start,
                end,
                tracks,
            } => {
                let range = self.opt_range(start.as_ref(), end.as_ref())?;
                let t = self.tracks(tracks.as_deref())?;
                let mode = match mode {
                    Level::Peak => NormalizeMode::Peak,
                    Level::Rms => NormalizeMode::Rms,
                    Level::Loudness => NormalizeMode::Loudness,
                };
                let s = NormalizeSettings {
                    mode,
                    target_db: target.unwrap_or(mode.default_target()),
                    peak_ceiling_db: ceiling_db,
                    independent,
                };
                let label = format!("Normalize to {} {}", s.target_db, mode.unit());
                let results = self.apply(label.clone(), |p| edit::normalize(p, &t, range, &s))?;
                Ok(Response {
                    summary: label,
                    data: Data::Normalized {
                        tracks: results
                            .into_iter()
                            .map(|r| NormalizedTrack {
                                track: r.track.0,
                                measured: r.measured.and_then(db),
                                gain_db: r.gain_db.and_then(db),
                            })
                            .collect(),
                    },
                })
            }
            R::FadeIn {
                start,
                end,
                shape,
                tracks,
            } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Fade in {}", self.fmt_span(a, b)), |p| {
                    edit::fade_in(p, &t, a, b, shape.into())
                })
            }
            R::FadeOut {
                start,
                end,
                shape,
                tracks,
            } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Fade out {}", self.fmt_span(a, b)), |p| {
                    edit::fade_out(p, &t, a, b, shape.into())
                })
            }
            R::RemoveFades { start, end, tracks } => {
                let (a, b) = self.span(&start, &end)?;
                let t = self.tracks(tracks.as_deref())?;
                self.edit(format!("Remove fades in {}", self.fmt_span(a, b)), |p| {
                    edit::remove_fades(p, &t, a, b)
                })
            }
            R::SetAutoCrossfade { on } => self.edit(
                format!("Auto crossfade {}", if on { "on" } else { "off" }),
                |p| edit::set_auto_crossfade(p, on),
            ),

            // --- Markers ----------------------------------------------------
            R::AddMarker { at, name } => {
                let at = self.time(&at)?;
                let label = format!("Add marker \"{name}\" at {}", self.fmt(at));
                let id = self.apply(label.clone(), |p| Ok(p.add_marker(at, name)))?;
                Ok(Response {
                    summary: label,
                    data: Data::Marker { marker: id.0 },
                })
            }
            R::UpdateMarker {
                id,
                at,
                name,
                artist,
                isrc,
            } => {
                let at = self.opt_time(at.as_ref())?;
                let mid = MarkerId(id);
                self.edit(format!("Update marker {id}"), |p| {
                    if let Some(at) = at {
                        edit::move_marker(p, mid, at)?;
                    }
                    let m = p.marker_mut(mid).ok_or(crate::Error::NotFound("marker"))?;
                    if let Some(n) = name {
                        m.name = n;
                    }
                    if let Some(a) = artist {
                        m.artist = (!a.is_empty()).then_some(a);
                    }
                    if let Some(c) = isrc {
                        m.isrc = (!c.is_empty()).then_some(c);
                    }
                    Ok(())
                })
            }
            R::RemoveMarker { id } => self.edit(format!("Remove marker {id}"), |p| {
                edit::remove_marker(p, MarkerId(id))
            }),
            R::SetMarkers { markers, replace } => {
                let at = markers
                    .iter()
                    .map(|m| self.time(&m.at))
                    .collect::<OpResult<Vec<_>>>()?;
                let n = markers.len();
                let old = self.project().markers.len();
                let label = if replace && old > 0 {
                    format!("Set {n} marker(s), replacing {old}")
                } else if replace {
                    format!("Set {n} marker(s)")
                } else {
                    format!("Add {n} marker(s)")
                };
                let ids = self.apply(label.clone(), |p| {
                    if replace {
                        p.markers.clear();
                    }
                    Ok(markers
                        .into_iter()
                        .zip(at)
                        .map(|(m, at)| {
                            let id = p.add_marker(at, m.name);
                            let mk = p.marker_mut(id).expect("just added");
                            mk.artist = m.artist.filter(|a| !a.is_empty());
                            mk.isrc = m.isrc.filter(|c| !c.is_empty());
                            id.0
                        })
                        .collect::<Vec<_>>())
                })?;
                Ok(Response {
                    summary: label,
                    data: Data::Markers { markers: ids },
                })
            }
            R::ClearMarkers => {
                let n = self.project().markers.len();
                self.edit(format!("Remove {n} marker(s)"), |p| {
                    p.markers.clear();
                    Ok(())
                })
            }

            // --- Album ------------------------------------------------------
            R::SetAlbum {
                artist,
                title,
                year,
                genre,
                comment,
                cover,
            } => {
                if let Some(c) = cover.as_ref().filter(|c| !c.as_os_str().is_empty()) {
                    if !c.is_file() {
                        return Err(OpError::new(
                            ErrorCode::NotFound,
                            format!("cover image {} not found", c.display()),
                        ));
                    }
                }
                self.edit("Album details", |p| {
                    let a = &mut p.album;
                    for (field, v) in [
                        (&mut a.artist, artist),
                        (&mut a.title, title),
                        (&mut a.year, year),
                        (&mut a.genre, genre),
                        (&mut a.comment, comment),
                    ] {
                        if let Some(v) = v {
                            *field = v;
                        }
                    }
                    if let Some(c) = cover {
                        a.cover = (!c.as_os_str().is_empty()).then_some(c);
                    }
                    Ok(())
                })
            }
            R::AlbumSummary { cd_safe } => self.album_summary(cd_safe, progress),
            R::AlbumLoudness {
                target_lufs,
                ceiling_db,
            } => self.album_loudness(target_lufs, ceiling_db, progress),

            // --- Export -----------------------------------------------------
            R::Export {
                path,
                options,
                dry_run,
                force,
            } => self.export(&path, &options, dry_run, force, progress),
        }
    }

    // -----------------------------------------------------------------------
    // Argument helpers
    // -----------------------------------------------------------------------

    fn sr(&self) -> u32 {
        self.project().sample_rate
    }

    fn fmt(&self, f: Frame) -> String {
        crate::util::format_time(f, self.sr())
    }

    fn fmt_span(&self, a: Frame, b: Frame) -> String {
        format!("{}–{}", self.fmt(a), self.fmt(b))
    }

    fn time(&self, t: &Time) -> OpResult<Frame> {
        t.to_frames(self.sr())
            .ok_or_else(|| OpError::arg(format!("invalid time {t:?}")))
    }

    fn opt_time(&self, t: Option<&Time>) -> OpResult<Option<Frame>> {
        t.map(|t| self.time(t)).transpose()
    }

    /// A required, non-empty range.
    fn span(&self, a: &Time, b: &Time) -> OpResult<(Frame, Frame)> {
        let (a, b) = (self.time(a)?, self.time(b)?);
        if b <= a {
            return Err(OpError::new(
                ErrorCode::InvalidRange,
                "end must be after start",
            ));
        }
        Ok((a, b))
    }

    /// An optional range, defaulting to the whole project.
    fn range(&self, a: Option<&Time>, b: Option<&Time>) -> OpResult<(Frame, Frame)> {
        let len = self.project().length();
        let a = self.opt_time(a)?.unwrap_or(0);
        let b = self.opt_time(b)?.unwrap_or(len);
        if b <= a {
            return Err(OpError::new(
                ErrorCode::InvalidRange,
                if len == 0 {
                    "the project is empty".to_string()
                } else {
                    "end must be after start".to_string()
                },
            ));
        }
        Ok((a, b))
    }

    /// `None` when neither end is given (whole tracks).
    fn opt_range(&self, a: Option<&Time>, b: Option<&Time>) -> OpResult<Option<(Frame, Frame)>> {
        if a.is_none() && b.is_none() {
            return Ok(None);
        }
        self.range(a, b).map(Some)
    }

    fn tracks(&self, ids: Option<&[u64]>) -> OpResult<Vec<TrackId>> {
        let p = self.project();
        match ids {
            None => Ok(p.all_track_ids()),
            Some(ids) => ids
                .iter()
                .map(|&i| {
                    p.track(TrackId(i))
                        .map(|t| t.id)
                        .ok_or_else(|| OpError::new(ErrorCode::NotFound, format!("no track {i}")))
                })
                .collect(),
        }
    }

    /// [`Document::apply`] with the label prefix.
    fn apply<T>(
        &mut self,
        label: impl Into<String>,
        f: impl FnOnce(&mut Project) -> crate::Result<T>,
    ) -> crate::Result<T> {
        let label = format!("{}{}", self.label_prefix, label.into());
        self.doc.apply(label, f)
    }

    /// Run an undoable edit and report it. Edits that change nothing say so.
    fn edit(
        &mut self,
        label: impl Into<String>,
        f: impl FnOnce(&mut Project) -> crate::Result<()>,
    ) -> OpResult {
        let label = label.into();
        let rev = self.doc.revision();
        self.apply(label.clone(), f)?;
        Ok(done(if self.doc.revision() == rev {
            format!("{label}: nothing changed")
        } else {
            label
        }))
    }

    /// Base name for exported files.
    fn base_name(&self) -> String {
        self.doc
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| self.source_name.clone())
            .or_else(|| Some(self.project().album.title.clone()).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| "Untitled".into())
    }

    // -----------------------------------------------------------------------
    // Session
    // -----------------------------------------------------------------------

    fn check_discard(&self, discard: bool) -> OpResult<()> {
        if self.doc.is_dirty() && !discard {
            return Err(OpError::new(
                ErrorCode::UnsavedChanges,
                "the project has unsaved changes: save it first, or pass discard to drop them",
            ));
        }
        Ok(())
    }

    fn open(&mut self, path: &Path) -> OpResult {
        if !path.exists() {
            return Err(OpError::new(
                ErrorCode::NotFound,
                format!("{} not found", path.display()),
            ));
        }
        let is_project = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(session::PROJECT_EXTENSION));
        if is_project {
            let mut doc = session::load_document(path, self.cache.clone())?;
            doc.path = Some(path.to_path_buf());
            self.doc = doc;
            self.clipboard = None;
            self.source_name = None;
            self.file_stamp = file_stamp(path);
        } else {
            let mut project = Project {
                cache: self.cache.clone(),
                ..Default::default()
            };
            session::import_file(&mut project, path, 0)?;
            self.reset(project, None);
            self.source_name = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        }
        let p = self.project();
        let warning = (!is_project)
            .then(|| crate::io::decode_limitation(path))
            .flatten()
            .map_or_else(String::new, |w| format!(". Warning: {w}"));
        Ok(done(format!(
            "Opened {}: {} track(s), {} markers, {}, {} Hz, {} ch{warning}",
            path.display(),
            p.tracks.len(),
            p.markers.len(),
            self.fmt(p.length()),
            p.sample_rate,
            p.channels
        )))
    }

    fn save(&mut self, path: Option<PathBuf>, force: bool) -> OpResult {
        let path = path
            .or_else(|| self.doc.path.clone())
            .ok_or_else(|| OpError::arg("no path: this project has not been saved before"))?;
        let path = if path.extension().is_none() {
            path.with_extension(session::PROJECT_EXTENSION)
        } else {
            path
        };
        let own = self.doc.path.as_deref() == Some(path.as_path());
        if path.exists() && !own && !force {
            return Err(OpError::new(
                ErrorCode::Exists,
                format!("{} exists (use force to replace it)", path.display()),
            ));
        }
        if own && !force && self.file_stamp.is_some() && file_stamp(&path) != self.file_stamp {
            return Err(OpError::new(
                ErrorCode::Modified,
                format!(
                    "{} changed on disk since it was opened; open it again to see the \
                     changes (unsaved work here is lost), or use force to replace it",
                    path.display()
                ),
            ));
        }
        session::save_document(&self.doc, &path)?;
        self.doc.path = Some(path.clone());
        self.doc.mark_saved();
        self.file_stamp = file_stamp(&path);
        Ok(done(format!("Saved {}", path.display())))
    }

    // -----------------------------------------------------------------------
    // Inspect
    // -----------------------------------------------------------------------

    fn info(&self) -> Response {
        let p = self.project();
        let sr = p.sample_rate;
        let pos = |f| TimePos::new(f, sr);
        let fade_secs = |f: Frame| f as f64 / sr as f64;
        let info = ProjectInfo {
            path: self.doc.path.clone(),
            dirty: self.doc.is_dirty(),
            sample_rate: sr,
            channels: p.channels,
            length: pos(p.length()),
            master_gain_db: db(album::master_gain_db(p)),
            auto_crossfade: p.auto_crossfade,
            album: p.album.clone(),
            tracks: p
                .tracks
                .iter()
                .map(|t| TrackInfo {
                    id: t.id.0,
                    name: t.name.clone(),
                    gain_db: db(gain_to_db(t.gain as f64)),
                    pan: t.pan,
                    muted: t.muted,
                    solo: t.solo,
                    clips: t
                        .clips
                        .iter()
                        .map(|c| ClipInfo {
                            id: c.id.0,
                            source: p
                                .source(c.source)
                                .map_or_else(String::new, |s| s.name.clone()),
                            start: pos(c.position),
                            end: pos(c.end()),
                            gain_db: db(gain_to_db(c.gain as f64)),
                            fade_in_secs: fade_secs(c.fade_in_frames()),
                            fade_out_secs: fade_secs(c.fade_out_frames()),
                        })
                        .collect(),
                })
                .collect(),
            markers: p
                .markers
                .iter()
                .map(|m| MarkerInfo {
                    id: m.id.0,
                    position: pos(m.position),
                    name: m.name.clone(),
                    auto: m.auto,
                    artist: m.artist.clone(),
                    isrc: m.isrc.clone(),
                })
                .collect(),
        };
        Response {
            summary: format!(
                "{} track(s), {} marker(s), {}, {} Hz, {} ch{}",
                info.tracks.len(),
                info.markers.len(),
                info.length.time,
                sr,
                p.channels,
                if info.dirty { ", unsaved changes" } else { "" }
            ),
            data: Data::Project(Box::new(info)),
        }
    }

    fn analyze(&self, a: Frame, b: Frame, tracks: Vec<TrackId>, progress: Progress) -> OpResult {
        let p = self.project();
        let meter =
            crate::analysis::measure_range(p, a, b, &RenderOptions::tracks(tracks), progress)?;
        let clipped = meter.clipped_samples();
        let l = meter.loudness();
        let levels = Levels {
            start: TimePos::new(a, p.sample_rate),
            end: TimePos::new(b, p.sample_rate),
            peak_db: db(meter.peak_db()),
            true_peak_db: meter.true_peak_db().and_then(db),
            rms_db: db(meter.rms_db()),
            lufs: meter.integrated_lufs().and_then(db),
            max_momentary_lufs: l.max_momentary().and_then(db),
            max_short_term_lufs: l.max_short_term().and_then(db),
            loudness_range_lu: l.range().and_then(db),
            clipped_samples: clipped,
        };
        let show = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.1}"));
        Ok(Response {
            summary: format!(
                "{}: peak {} dBFS, true peak {} dBTP, RMS {} dBFS, {} LUFS{}",
                self.fmt_span(a, b),
                show(levels.peak_db),
                show(levels.true_peak_db),
                show(levels.rms_db),
                show(levels.lufs),
                if clipped > 0 {
                    format!(", {clipped} samples over 0 dBFS")
                } else {
                    String::new()
                }
            ),
            data: Data::Levels(levels),
        })
    }

    /// A version to compare, as a reader over `start..end`, with a description
    /// and the frame it starts at.
    fn version_reader(
        &self,
        v: &Version,
        start: Option<&Time>,
        end: Option<&Time>,
        tracks: Option<&[u64]>,
    ) -> OpResult<(Box<dyn Reader>, String, Frame)> {
        let project = |p: Project, what: String| -> OpResult<(Box<dyn Reader>, String, Frame)> {
            if p.is_empty() {
                // Its format is unknown: `compare` gives it the other side's.
                return Ok((Box::new(Empty::new(0, 0)), format!("{what} (empty)"), 0));
            }
            let rate = p.sample_rate;
            let at = |t: Option<&Time>| {
                t.map(|t| {
                    t.to_frames(rate)
                        .ok_or_else(|| OpError::arg(format!("invalid time {t:?}")))
                })
                .transpose()
            };
            let a = at(start)?.unwrap_or(0);
            let b = at(end)?.unwrap_or(p.length());
            if b <= a {
                return Err(OpError::new(
                    ErrorCode::InvalidRange,
                    if p.length() == 0 {
                        format!("{what} is empty")
                    } else {
                        "end must be after start".to_string()
                    },
                ));
            }
            let opts = match tracks {
                Some(ids) => {
                    RenderOptions::tracks(ids.iter().map(|&i| TrackId(i)).collect::<Vec<_>>())
                }
                None => RenderOptions::default(),
            };
            Ok((Box::new(ProjectReader::new(p, a, b, opts)), what, a))
        };
        match v {
            Version::Project => project(self.project().clone(), "the project".into()),
            Version::History(i) => {
                let states = self.doc.history_states();
                let p = self.doc.state(*i).ok_or_else(|| {
                    OpError::new(
                        ErrorCode::NotFound,
                        format!(
                            "no history state {i}: there are {} (0 to {}; now at {})",
                            states.len(),
                            states.len() - 1,
                            self.doc.history_position()
                        ),
                    )
                })?;
                let what = match states[*i].label {
                    Some(l) => format!("history state {i} (after \"{l}\")"),
                    None => format!("history state {i} (before any edit)"),
                };
                project(p.clone(), what)
            }
            Version::File(path) => {
                if !path.exists() {
                    return Err(OpError::new(
                        ErrorCode::NotFound,
                        format!("{} not found", path.display()),
                    ));
                }
                let what = path.display().to_string();
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case(session::PROJECT_EXTENSION))
                {
                    return project(session::load(path, self.cache.clone())?, what);
                }
                let r: Box<dyn Reader> = Box::new(DecoderReader::new(crate::decode::open(path)?));
                let rate = r.sample_rate();
                let at = |t: &Time| {
                    t.to_frames(rate)
                        .ok_or_else(|| OpError::arg(format!("invalid time {t:?}")))
                };
                let a = start.map(at).transpose()?.unwrap_or(0);
                let b = end.map(at).transpose()?;
                if b.is_some_and(|b| b <= a) {
                    return Err(OpError::new(
                        ErrorCode::InvalidRange,
                        "end must be after start",
                    ));
                }
                Ok((compare::slice(r, a, b)?, what, a))
            }
        }
    }

    fn compare(&self, req: Request, progress: Progress) -> OpResult {
        let Request::Compare {
            a,
            b,
            start,
            end,
            tracks,
            align,
            max_offset_secs,
            threshold_db,
        } = req
        else {
            unreachable!("only called for compare")
        };
        let (start, end) = (start.as_ref(), end.as_ref());
        let (ra, name_a, start_a) = self.version_reader(&a, start, end, tracks.as_deref())?;
        let (rb, name_b, _) = self.version_reader(&b, start, end, tracks.as_deref())?;
        // An empty project takes the format of what it is compared with.
        let like = |r: &dyn Reader| -> Box<dyn Reader> {
            Box::new(Empty::new(r.sample_rate(), r.channels()))
        };
        let (ra, rb) = match (ra.sample_rate() == 0, rb.sample_rate() == 0) {
            (true, true) => {
                return Err(OpError::new(
                    ErrorCode::InvalidRange,
                    format!("{name_a} and {name_b} are both empty"),
                ))
            }
            (true, false) => (like(rb.as_ref()), rb),
            (false, true) => {
                let empty = like(ra.as_ref());
                (ra, empty)
            }
            (false, false) => (ra, rb),
        };
        let rate = ra.sample_rate();
        let mut opts = CompareOptions::new(rate);
        opts.align = align;
        if let Some(s) = max_offset_secs {
            if !(0.0..=60.0).contains(&s) {
                return Err(OpError::arg("max_offset_secs must be 0 to 60"));
            }
            opts.max_offset = (s * rate as f64).round() as u64;
        }
        if let Some(t) = threshold_db {
            if !t.is_finite() || t > 0.0 {
                return Err(OpError::arg(
                    "threshold_db must be a level in dBFS, at most 0",
                ));
            }
            opts.threshold = crate::util::db_to_gain(t) as f32;
        }
        let r = compare::compare(ra, rb, &opts, progress)?;

        let skip_a = r
            .alignment
            .as_ref()
            .map_or(0, |al| (-al.offset).max(0) as Frame);
        let at = |f: Frame| TimePos::new(start_a + skip_a + f, rate);
        let lin_db = |v: f64| db(gain_to_db(v));
        const LISTED: usize = 100;
        let info = CompareInfo {
            a: name_a.clone(),
            b: name_b.clone(),
            identical: r.identical,
            nulls: r.nulls(),
            threshold_db: threshold_db.and_then(db),
            length_a: TimePos::new(r.frames_a, rate),
            length_b: TimePos::new(r.frames_b, rate),
            alignment: r.alignment.as_ref().map(|al| AlignmentInfo {
                offset_frames: al.offset,
                offset_secs: (al.offset as f64 / rate as f64 * 1e6).round() / 1e6,
                correlation: al.correlation,
                found: al.found,
            }),
            differing_frames: r.differing_frames,
            first_difference: r.first_difference.map(at),
            last_difference: r.last_difference.map(at),
            peak_db: lin_db(r.peak as f64),
            rms_db: lin_db(r.rms),
            null_depth_db: r.null_depth_db().and_then(db),
            channels: r
                .per_channel
                .iter()
                .enumerate()
                .map(|(i, c)| ChannelDifference {
                    channel: i + 1,
                    peak_db: lin_db(c.peak as f64),
                    rms_db: lin_db(c.rms),
                    differing_samples: c.differing_samples,
                })
                .collect(),
            regions: r
                .regions
                .iter()
                .take(LISTED)
                .map(|g| DiffSpan {
                    start: at(g.start),
                    end: at(g.end),
                    length: TimePos::new(g.end - g.start, rate),
                    peak_db: lin_db(g.peak as f64),
                })
                .collect(),
            truncated: r.region_count > LISTED,
            region_count: r.region_count,
        };
        Ok(Response {
            summary: compare_summary(&info, rate),
            data: Data::Compare(Box::new(info)),
        })
    }

    fn find_silences(
        &self,
        a: Frame,
        b: Frame,
        tracks: Vec<TrackId>,
        threshold_db: f64,
        min_secs: f64,
        progress: Progress,
    ) -> OpResult {
        const MAX_RANGES: usize = 500;
        let p = self.project();
        let sr = p.sample_rate;
        let threshold = crate::util::db_to_gain(threshold_db) as f32;
        let min_len = (min_secs * sr as f64).round() as Frame;
        let mut ranges: Vec<(Frame, Frame)> = Vec::new();
        let mut run_start: Option<Frame> = None;
        let mut pos = a;
        let total = (b - a) as f64;
        let close = |from: Frame, to: Frame, ranges: &mut Vec<(Frame, Frame)>| {
            if to - from >= min_len.max(1) {
                ranges.push((from, to));
            }
        };
        render::for_each_chunk(p, a, b, &RenderOptions::tracks(tracks), |chunk, n| {
            for i in 0..n {
                let quiet = chunk.iter().all(|c| c[i].abs() < threshold);
                match (quiet, run_start) {
                    (true, None) => run_start = Some(pos),
                    (false, Some(s)) => {
                        close(s, pos, &mut ranges);
                        run_start = None;
                    }
                    _ => {}
                }
                pos += 1;
            }
            Ok(progress((pos - a) as f64 / total))
        })?;
        if let Some(s) = run_start {
            close(s, b, &mut ranges);
        }
        let truncated = ranges.len() > MAX_RANGES;
        let total_secs: f64 = ranges.iter().map(|(s, e)| (e - s) as f64).sum::<f64>() / sr as f64;
        let ranges: Vec<Span> = ranges
            .into_iter()
            .take(MAX_RANGES)
            .map(|(s, e)| Span {
                start: TimePos::new(s, sr),
                end: TimePos::new(e, sr),
                length: TimePos::new(e - s, sr),
            })
            .collect();
        Ok(Response {
            summary: format!(
                "{} silent range(s) below {threshold_db} dBFS, {:.1} s in total",
                ranges.len(),
                total_secs
            ),
            data: Data::Silences(Silences {
                threshold_db,
                ranges,
                truncated,
            }),
        })
    }

    fn waveform(
        &self,
        a: Frame,
        b: Frame,
        tracks: &[TrackId],
        o: &crate::waveform::Options,
        progress: Progress,
    ) -> OpResult {
        use base64::Engine;
        let p = self.project();
        let sr = p.sample_rate;
        let img = crate::waveform::render_png(p, tracks, a, b, o, progress)?;
        let markers = p
            .markers
            .iter()
            .filter(|m| m.position >= a && m.position < b)
            .map(|m| WaveformMarker {
                x: crate::waveform::column_of(m.position, a, b, o.width),
                name: m.name.clone(),
                position: TimePos::new(m.position, sr),
            })
            .collect();
        let secs_per_px = (b - a) as f64 / sr as f64 / o.width as f64;
        Ok(Response {
            summary: format!(
                "Waveform of {}: {}×{} px, {:.4} s per pixel, grid every {} s",
                self.fmt_span(a, b),
                o.width,
                o.height,
                secs_per_px,
                img.grid_secs
            ),
            data: Data::Waveform(WaveformInfo {
                png_base64: base64::engine::general_purpose::STANDARD.encode(&img.png),
                width: o.width,
                height: o.height,
                start: TimePos::new(a, sr),
                end: TimePos::new(b, sr),
                channels: p.channels,
                seconds_per_pixel: secs_per_px,
                grid_secs: img.grid_secs,
                db_scale: o.db_scale,
                markers,
            }),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn detect(
        &mut self,
        a: Frame,
        b: Frame,
        tracks: &[TrackId],
        settings: &DetectSettings,
        setlist: &str,
        apply: bool,
        progress: Progress,
    ) -> OpResult {
        let analysis = detect::analyze(self.project(), tracks, a, b, progress)?;
        let found = detect::find_boundaries(&analysis, settings);
        let mut positions = vec![analysis.start];
        let mut confidence = vec![None];
        for f in &found {
            positions.push(f.position);
            confidence.push(Some((f.strength * 100.0).round() / 100.0));
        }
        let names = detect::marker_names(setlist, positions.len());
        if apply {
            self.apply("Detect song starts", |p| {
                detect::apply_markers(p, a, b, &positions, &names)
            })?;
        }
        let sr = self.sr();
        let songs: Vec<SongStart> = positions
            .iter()
            .zip(&names)
            .zip(confidence)
            .map(|((&pos, name), confidence)| SongStart {
                position: TimePos::new(pos, sr),
                name: name.clone(),
                confidence,
            })
            .collect();
        let n_titles = setlist.lines().filter(|l| !l.trim().is_empty()).count();
        let mut summary = format!(
            "Found {} song(s){}",
            songs.len(),
            if apply { ", markers placed" } else { "" }
        );
        if n_titles > 0 && n_titles != songs.len() {
            summary += &format!(" (the setlist has {n_titles} titles)");
        }
        Ok(Response {
            summary,
            data: Data::Songs(SongsReport {
                reference_lufs: (analysis.reference as f64 * 10.0).round() / 10.0,
                songs,
                applied: apply,
            }),
        })
    }

    // -----------------------------------------------------------------------
    // Album
    // -----------------------------------------------------------------------

    fn regions(&self, cd_safe: bool) -> Vec<Region> {
        let p = self.project();
        let regions = render::regions_from_markers(p, p.length());
        match render::sector_len(p.sample_rate).filter(|_| cd_safe) {
            Some(sector) => render::snap_to_sectors(&regions, sector),
            None => regions,
        }
    }

    fn album_summary(&self, cd_safe: bool, progress: Progress) -> OpResult {
        let p = self.project();
        let sr = p.sample_rate;
        let regions = self.regions(cd_safe);
        if regions.is_empty() {
            return Err(OpError::new(
                ErrorCode::NothingSelected,
                "the project is empty",
            ));
        }
        let a = album::analyze(p, &regions, progress)?;
        let tracks = regions
            .iter()
            .zip(&a.tracks)
            .enumerate()
            .map(|(i, (r, l))| {
                let secs = (r.end - r.start) as f64 / sr as f64;
                let mut flags = Vec::new();
                if l.peak_db > -0.1 {
                    flags.push("near_clipping".to_string());
                }
                if secs < 30.0 {
                    flags.push("short".to_string());
                }
                if secs > 20.0 * 60.0 {
                    flags.push("long".to_string());
                }
                AlbumTrack {
                    number: i + 1,
                    title: r.name.clone(),
                    marker: p
                        .markers
                        .iter()
                        .find(|m| m.position == r.start)
                        .map(|m| m.id.0),
                    start: TimePos::new(r.start, sr),
                    length: TimePos::new(r.end - r.start, sr),
                    lufs: l.lufs.and_then(db),
                    peak_db: db(l.peak_db),
                    flags,
                }
            })
            .collect::<Vec<_>>();
        let flagged = tracks.iter().filter(|t| !t.flags.is_empty()).count();
        let report = AlbumReport {
            album: p.album.clone(),
            lufs: a.lufs.and_then(db),
            peak_db: db(a.peak_db),
            master_gain_db: db(album::master_gain_db(p)),
            sector_errors: render::sector_len(sr).map(|s| render::sector_errors(&regions, s)),
            tracks,
        };
        Ok(Response {
            summary: format!(
                "{} track(s), album {} LUFS, peak {} dBFS{}",
                report.tracks.len(),
                report.lufs.map_or("–".into(), |v| format!("{v:.1}")),
                report.peak_db.map_or("–".into(), |v| format!("{v:.1}")),
                if flagged > 0 {
                    format!(", {flagged} flagged")
                } else {
                    String::new()
                }
            ),
            data: Data::Album(report),
        })
    }

    fn album_loudness(
        &mut self,
        target: f64,
        ceiling: Option<f64>,
        progress: Progress,
    ) -> OpResult {
        let regions = self.regions(false);
        if regions.is_empty() {
            return Err(OpError::new(
                ErrorCode::NothingSelected,
                "the project is empty",
            ));
        }
        let a = album::analyze(self.project(), &regions, progress)?;
        let before = a.lufs.ok_or_else(|| {
            OpError::new(
                ErrorCode::Silent,
                "the album is silent or too short to measure",
            )
        })?;
        let gain = album::album_gain_db(&a, target, ceiling).unwrap_or(0.0);
        let limited = (gain - (target - before)).abs() > 1e-9;
        let gain = (gain * 100.0).round() / 100.0;
        self.apply(format!("Album gain {gain:+.2} dB"), |p| {
            album::adjust_master_gain(p, gain)
        })?;
        let change = LoudnessChange {
            before_lufs: (before * 100.0).round() / 100.0,
            after_lufs: ((before + gain) * 100.0).round() / 100.0,
            before_peak_db: db(a.peak_db),
            after_peak_db: db(a.peak_db + gain),
            gain_db: gain,
            limited_by_ceiling: limited,
        };
        Ok(Response {
            summary: format!(
                "Album gain {gain:+.2} dB: {:.1} → {:.1} LUFS{}",
                change.before_lufs,
                change.after_lufs,
                if limited {
                    " (limited by the peak ceiling)"
                } else {
                    ""
                }
            ),
            data: Data::AlbumLoudness(change),
        })
    }

    // -----------------------------------------------------------------------
    // Export
    // -----------------------------------------------------------------------

    fn export(
        &self,
        path: &Path,
        o: &ExportOptions,
        dry_run: bool,
        force: bool,
        progress: Progress,
    ) -> OpResult {
        let p = self.project();
        if p.length() == 0 {
            return Err(OpError::new(
                ErrorCode::NothingSelected,
                "the project is empty",
            ));
        }
        let settings = export_settings(o)?;
        let sr = p.sample_rate;
        let pos = |f| TimePos::new(f, sr);
        let mut warnings = Vec::new();
        let is_cd = settings.is_audio_cd(sr, p.channels);
        let cd_safe = o.cd_safe.unwrap_or(o.preset == Preset::Cd);
        let sector = render::sector_len(sr);
        if cd_safe && sector.is_none() && matches!(o.kind, ExportKind::Tracks | ExportKind::Cue) {
            warnings.push(format!(
                "{sr} Hz is not a multiple of 75 Hz, so track boundaries can't be made CD-safe"
            ));
        }
        let regions = || self.regions(cd_safe);
        let with_ext = |path: &Path| {
            let mut path = path.to_path_buf();
            if path.extension().is_none() {
                path.set_extension(settings.container.extension());
            }
            path
        };
        let file =
            |path: PathBuf, title: Option<String>, span: Option<(Frame, Frame)>| ExportFile {
                exists: path.exists(),
                path,
                title,
                start: span.map(|(a, _)| pos(a)),
                length: span.map(|(a, b)| pos(b - a)),
            };

        let mut sector_errors = None;
        let mut track_paths = Vec::new();
        let files: Vec<ExportFile> = match o.kind {
            ExportKind::Mix => {
                let (a, b) = self.range(o.start.as_ref(), o.end.as_ref())?;
                vec![file(with_ext(path), None, Some((a, b)))]
            }
            ExportKind::Tracks => {
                let regions = regions();
                if is_cd {
                    sector_errors = sector.map(|s| render::sector_errors(&regions, s));
                }
                let base = o.base_name.clone().unwrap_or_else(|| self.base_name());
                let names = o
                    .names
                    .as_deref()
                    .unwrap_or(crate::naming::DEFAULT_TRACK_NAMES);
                track_paths =
                    render::region_paths(p, &regions, path, names, &base, settings.container)?;
                track_paths
                    .iter()
                    .cloned()
                    .zip(&regions)
                    .map(|(f, r)| file(f, Some(r.name.clone()), Some((r.start, r.end))))
                    .collect()
            }
            ExportKind::Cue => {
                let regions = regions();
                let audio = with_ext(path);
                let end = regions.last().map_or(0, |r| r.end);
                vec![
                    file(audio.clone(), None, Some((0, end))),
                    file(audio.with_extension("cue"), Some("CUE sheet".into()), None),
                ]
            }
            ExportKind::Stems => render::stem_paths(p, path, settings.container)
                .into_iter()
                .map(|(t, f)| file(f, Some(t.name.clone()), Some((0, p.length()))))
                .collect(),
        };
        if files.is_empty() {
            return Err(OpError::new(
                ErrorCode::NothingSelected,
                "nothing to export",
            ));
        }
        if sector_errors.is_some_and(|n| n > 0) {
            warnings.push(
                "some tracks don't end on a CD sector boundary and may click or gap when burned; export with cd_safe"
                    .into(),
            );
        }
        let format = describe(&settings, sr, p.channels);
        let existing: Vec<_> = files.iter().filter(|f| f.exists).collect();
        let report = |written| ExportReport {
            written,
            files: files.clone(),
            format: format.clone(),
            sector_errors,
            warnings: warnings.clone(),
        };

        if dry_run {
            let mut summary = format!("Would write {} file(s) as {format}", files.len());
            if !existing.is_empty() {
                summary += &format!(
                    "; {} already exist{}",
                    existing.len(),
                    if force {
                        " and would be replaced"
                    } else {
                        " (needs force)"
                    }
                );
            }
            return Ok(Response {
                summary,
                data: Data::Export(report(false)),
            });
        }
        if !existing.is_empty() && !force {
            let names: Vec<String> = existing
                .iter()
                .map(|f| f.path.display().to_string())
                .collect();
            return Err(OpError::new(
                ErrorCode::Exists,
                format!(
                    "would replace existing files (use force): {}",
                    names.join(", ")
                ),
            ));
        }
        let dir = match o.kind {
            ExportKind::Tracks | ExportKind::Stems => Some(path),
            ExportKind::Mix | ExportKind::Cue => files[0].path.parent(),
        };
        if let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(d).map_err(crate::Error::from)?;
        }
        match o.kind {
            ExportKind::Mix => {
                let span = files[0].start.as_ref().map_or(0, |s| s.frames);
                let len = files[0].length.as_ref().map_or(0, |l| l.frames);
                render::export_range(
                    p,
                    span,
                    span + len,
                    &RenderOptions::default(),
                    &files[0].path,
                    settings,
                    progress,
                )?;
            }
            ExportKind::Tracks => {
                render::export_regions(
                    p,
                    &regions(),
                    &RenderOptions::default(),
                    &track_paths,
                    settings,
                    progress,
                )?;
            }
            ExportKind::Cue => {
                render::export_with_cue(
                    p,
                    &regions(),
                    &files[0].path,
                    &self.base_name(),
                    settings,
                    progress,
                )?;
            }
            ExportKind::Stems => {
                render::export_stems(p, 0, p.length(), path, settings, progress)?;
            }
        }
        Ok(Response {
            summary: format!("Wrote {} file(s) as {format}", files.len()),
            data: Data::Export(report(true)),
        })
    }
}

/// Turn a preset plus overrides into engine export settings.
pub fn export_settings(o: &ExportOptions) -> OpResult<ExportSettings> {
    let mut s = match o.preset {
        Preset::Wav => ExportSettings {
            dither: Dither::None,
            ..Default::default()
        },
        Preset::Cd => ExportSettings::AUDIO_CD,
        Preset::Flac => ExportSettings::FLAC_ALBUM,
    };
    if let Some(c) = o.container {
        s.container = match c {
            FileType::Wav => Container::Wav,
            FileType::Flac => Container::Flac,
        };
    }
    if let Some(b) = o.format {
        s.format = match b {
            Bits::Int16 => SampleFormat::Int16,
            Bits::Int24 => SampleFormat::Int24,
            Bits::Int32 => SampleFormat::Int32,
            Bits::Float32 => SampleFormat::Float32,
        };
        // Dither by default whenever quantising, unless asked otherwise.
        s.dither = match s.dither {
            _ if s.format.is_float() => Dither::None,
            Dither::None => Dither::Tpdf,
            d => d,
        };
    } else if s.container == Container::Flac && s.format.is_float() {
        s.format = SampleFormat::Int24;
        s.dither = Dither::Tpdf;
    }
    if let Some(d) = o.dither {
        s.dither = d;
    }
    if let Some(r) = o.sample_rate {
        if !(8_000..=384_000).contains(&r) {
            return Err(OpError::arg(format!("unsupported sample rate {r}")));
        }
        s.sample_rate = Some(r);
    }
    if let Some(c) = o.channels {
        if !(1..=8).contains(&c) {
            return Err(OpError::arg(format!("unsupported channel count {c}")));
        }
        s.channels = Some(c);
    }
    if !s.container.supports(s.format) {
        return Err(OpError::arg(format!(
            "{} can't store {}",
            s.container.label(),
            s.format.label()
        )));
    }
    Ok(s)
}

fn describe(s: &ExportSettings, rate: u32, channels: u16) -> String {
    format!(
        "{} {}, {} Hz, {} ch{}",
        s.container.label(),
        s.format.label(),
        s.rate_for(rate),
        s.channels_for(channels),
        match s.dither.effective(s.format.bits(), s.rate_for(rate)) {
            _ if s.format.is_float() => "",
            Dither::None => "",
            Dither::Tpdf => ", dithered",
            Dither::Light => ", dithered with light noise shaping",
            Dither::Strong => ", dithered with strong noise shaping",
        }
    )
}

/// The JSON envelope for a result: `{"ok": true, "result": …}` or
/// `{"ok": false, "error": …}`.
pub fn envelope(r: &std::result::Result<Response, OpError>) -> serde_json::Value {
    match r {
        Ok(v) => serde_json::json!({"ok": true, "result": v}),
        Err(e) => serde_json::json!({"ok": false, "error": e}),
    }
}

#[cfg(test)]
mod tests;
