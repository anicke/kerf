//! Requests and responses of the operations layer, and their JSON forms.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::project::AlbumInfo;
use crate::util::{format_time, parse_time, TimeFormat};
use crate::Frame;

// ---------------------------------------------------------------------------
// Times
// ---------------------------------------------------------------------------

/// A time as given by a caller. In JSON either a number of **seconds**
/// (`83.5`) or a string: `"1:23.5"` (m:ss), `"1:02:03"` (h:mm:ss), `"83.5s"`
/// (seconds) or `"4008000smp"` (sample frames, exact).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Time {
    Seconds(f64),
    Text(String),
}

impl Time {
    pub fn secs(s: f64) -> Self {
        Self::Seconds(s)
    }

    pub fn frames(f: Frame) -> Self {
        Self::Text(format!("{f}smp"))
    }

    /// Resolve to sample frames at `sample_rate`.
    pub fn to_frames(&self, sample_rate: u32) -> Option<Frame> {
        match self {
            Time::Seconds(s) if s.is_finite() && *s >= 0.0 => {
                Some((s * sample_rate as f64).round() as Frame)
            }
            Time::Seconds(_) => None,
            Time::Text(t) => {
                let t = t.trim();
                for suffix in ["samples", "sample", "smp"] {
                    if let Some(n) = t.strip_suffix(suffix) {
                        return n.trim().parse().ok();
                    }
                }
                let t = t.strip_suffix('s').unwrap_or(t);
                parse_time(t, sample_rate, TimeFormat::MinSec)
            }
        }
    }
}

impl std::str::FromStr for Time {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Time::Text(s.to_string()))
    }
}

/// A time reported back: exact frames, seconds, and a readable `m:ss.mmm`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimePos {
    pub seconds: f64,
    pub frames: Frame,
    pub time: String,
}

impl TimePos {
    pub fn new(frames: Frame, sample_rate: u32) -> Self {
        let seconds = frames as f64 / sample_rate.max(1) as f64;
        Self {
            seconds: (seconds * 1e6).round() / 1e6,
            frames,
            time: format_time(frames, sample_rate),
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Which tracks an operation applies to (by id); all tracks when absent.
pub type Tracks = Option<Vec<u64>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Linear,
    #[default]
    EqualPower,
    SCurve,
    Exponential,
}

impl From<Shape> for crate::FadeShape {
    fn from(s: Shape) -> Self {
        match s {
            Shape::Linear => Self::Linear,
            Shape::EqualPower => Self::EqualPower,
            Shape::SCurve => Self::SCurve,
            Shape::Exponential => Self::Exponential,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Paste {
    #[default]
    Insert,
    Overwrite,
    Mix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Peak,
    Rms,
    Loudness,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SongPlacement {
    /// Where the music resumes, minus the pre-roll.
    #[default]
    MusicStart,
    /// The quietest moment of the gap.
    QuietestPoint,
}

/// What to export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    /// The mix (or `start..end` of it) as one file. `path` is the file.
    #[default]
    Mix,
    /// One file per marker region (album tracks). `path` is a folder.
    Tracks,
    /// The whole mix as one file plus a CUE sheet. `path` is the audio file.
    Cue,
    /// Every unmuted track as its own file. `path` is a folder.
    Stems,
}

/// Starting points for export settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// 32-bit float WAV at the project's rate and channels (lossless, no dither).
    #[default]
    Wav,
    /// Red Book audio CD: 16-bit 44.1 kHz stereo WAV, dithered; CD-safe track boundaries.
    Cd,
    /// FLAC 24-bit at the project's rate and channels.
    Flac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Bits {
    #[serde(alias = "16")]
    Int16,
    #[serde(alias = "24")]
    Int24,
    #[serde(alias = "32")]
    Int32,
    #[serde(alias = "float")]
    Float32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    Wav,
    Flac,
}

/// Export parameters: a preset plus optional overrides.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ExportOptions {
    pub kind: ExportKind,
    pub preset: Preset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Bits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<FileType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// none, tpdf, light or strong (noise shaping, 16-bit); true/false also work.
    pub dither: Option<crate::dither::Dither>,
    /// Snap track boundaries to CD sectors (tracks and cue exports).
    /// Defaults to on for the `cd` preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cd_safe: Option<bool>,
    /// Mix exports only: the range to export (default: everything).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<Time>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<Time>,
    /// Value of `{project}` in file names (default: the project or source name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_name: Option<String>,
    /// Tracks exports: file name template, default `{nn} - {title}`. Fields:
    /// {nn} {num} {title} {artist} {album} {year} {genre} {project}; `/` makes
    /// folders, e.g. `{artist} - {album}/{nn} {title}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub names: Option<String>,
}

/// A version of the audio to compare. In JSON: `"project"` (as it is now),
/// `{"history": 2}` (a state in the undo history: 0 is before the first step
/// `history` lists, n after the n-th; numbers past the current state are
/// undone steps that can be redone) or `{"file": "mix.wav"}` (an audio file
/// or a saved `.kerf` project).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Version {
    #[default]
    Project,
    History(usize),
    File(PathBuf),
}

/// A marker to place with `set_markers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NewMarker {
    pub at: Time,
    pub name: String,
    /// Track artist when it differs from the album artist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

/// One operation. In JSON: `{"op": "split", "at": "1:23.5"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    // --- Session ------------------------------------------------------------
    /// Start an empty project (it adopts the format of the first import).
    /// Refuses while there are unsaved changes unless `discard` is true.
    New {
        #[serde(default)]
        discard: bool,
    },
    /// Open a `.kerf` project, or an audio file (WAV, FLAC, MP3, OGG, AIFF,
    /// M4A…) as a new project. Refuses while there are unsaved changes unless
    /// `discard` is true.
    Open {
        path: PathBuf,
        #[serde(default)]
        discard: bool,
    },
    /// Add an audio file as a new track (at `at`, default 0).
    Import {
        path: PathBuf,
        #[serde(default)]
        at: Option<Time>,
    },
    /// Save the project with its undo history (to `path`, or where it was
    /// opened from). Existing files other than the project's own, or the
    /// project's own if something else changed it since it was opened, are
    /// only replaced with `force`.
    Save {
        #[serde(default)]
        path: Option<PathBuf>,
        #[serde(default)]
        force: bool,
    },
    /// Undo the last edit.
    Undo,
    /// Redo the last undone edit.
    Redo,
    /// The undoable steps, oldest first, and whether there are unsaved changes.
    History,

    // --- Inspect ------------------------------------------------------------
    /// Tracks, clips, markers, album details, format and length.
    Info,
    /// Peak, true peak, RMS, loudness (integrated, max momentary and short-term, range) and clipping of a range as heard.
    Analyze {
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Find song starts in a live recording; `apply` places them as markers
    /// (replacing earlier detected markers in the range).
    DetectSongs {
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
        /// Exact number of songs, if known (the clearest gaps win).
        #[serde(default)]
        songs: Option<usize>,
        /// 0..1, how readily a gap counts (ignored with `songs`). Default 0.5.
        #[serde(default)]
        sensitivity: Option<f32>,
        #[serde(default)]
        min_song_secs: Option<f64>,
        #[serde(default)]
        min_gap_secs: Option<f64>,
        #[serde(default)]
        pre_roll_secs: Option<f64>,
        #[serde(default)]
        placement: SongPlacement,
        /// Song titles in order (numbering like "1." is removed).
        #[serde(default)]
        setlist: Vec<String>,
        #[serde(default)]
        apply: bool,
    },

    /// Null test: compare two versions sample by sample, e.g. the project now
    /// against an earlier history state (did an edit change anything else?)
    /// or against an exported file (does it match?). Reports whether they are
    /// identical, the peak and RMS of the difference, how far below the
    /// signal it lies, and the time ranges where they differ. `start`/`end`
    /// limit both sides (times in each); `tracks` applies to project versions.
    Compare {
        /// Default: the project as it is now.
        #[serde(default)]
        a: Version,
        b: Version,
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
        /// First shift `b` by whole frames to line it up with `a` (extra
        /// lead-in, encoder delay), searching up to `max_offset_secs` (default 1).
        #[serde(default)]
        align: bool,
        #[serde(default)]
        max_offset_secs: Option<f64>,
        /// Ignore differences at or below this level (dBFS), e.g. -90 to
        /// accept 16-bit rounding or dither. Default: any difference counts.
        #[serde(default)]
        threshold_db: Option<f64>,
    },

    /// Ranges quieter than `threshold_db` (peak, dBFS; default -50) for at
    /// least `min_secs` (default 1). Live recordings rarely have true
    /// silence between songs: use `detect_songs` for those.
    FindSilences {
        #[serde(default)]
        threshold_db: Option<f64>,
        #[serde(default)]
        min_secs: Option<f64>,
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Draw the waveform of a range (default: everything) as a PNG image,
    /// one lane per channel: peaks in blue, RMS lighter, samples over 0 dBFS
    /// in red, markers as yellow lines, faint vertical lines every
    /// `grid_secs`. The response gives the time of each pixel column.
    Waveform {
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
        /// Image width in pixels (200–4000, default 1200).
        #[serde(default)]
        width: Option<u32>,
        /// Image height in pixels (100–2000, default 300).
        #[serde(default)]
        height: Option<u32>,
        /// Draw amplitude on a 60 dB log scale, making quiet passages visible.
        #[serde(default)]
        db_scale: bool,
    },

    // --- Edit ---------------------------------------------------------------
    /// Split clips at a time (non-destructive: the audio is unchanged).
    Split {
        at: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Remove a range and close the gap (markers move along when all tracks are affected).
    Delete {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Silence a range without moving anything.
    Clear {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Keep only the range.
    Crop {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    InsertSilence {
        at: Time,
        length: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    Copy {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Copy, then delete (closing the gap).
    Cut {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    Paste {
        at: Time,
        #[serde(default)]
        tracks: Tracks,
        #[serde(default)]
        mode: Paste,
    },
    /// Change gain by `db` (of a range, or whole tracks).
    Gain {
        db: f64,
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
    },
    Normalize {
        #[serde(default)]
        mode: Level,
        /// dBFS for peak/RMS, LUFS for loudness. Defaults: -1, -20, -23.
        #[serde(default)]
        target: Option<f64>,
        #[serde(default)]
        ceiling_db: Option<f64>,
        /// One gain per track (default) instead of one shared gain.
        #[serde(default = "yes")]
        independent: bool,
        #[serde(default)]
        start: Option<Time>,
        #[serde(default)]
        end: Option<Time>,
        #[serde(default)]
        tracks: Tracks,
    },
    FadeIn {
        start: Time,
        end: Time,
        #[serde(default)]
        shape: Shape,
        #[serde(default)]
        tracks: Tracks,
    },
    FadeOut {
        start: Time,
        end: Time,
        #[serde(default)]
        shape: Shape,
        #[serde(default)]
        tracks: Tracks,
    },
    RemoveFades {
        start: Time,
        end: Time,
        #[serde(default)]
        tracks: Tracks,
    },
    /// Crossfade automatically where clips overlap.
    SetAutoCrossfade {
        on: bool,
    },

    // --- Markers ------------------------------------------------------------
    AddMarker {
        at: Time,
        name: String,
    },
    /// Change any of a marker's fields. Empty `artist`/`isrc` clear them.
    UpdateMarker {
        id: u64,
        #[serde(default)]
        at: Option<Time>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        artist: Option<String>,
        #[serde(default)]
        isrc: Option<String>,
    },
    RemoveMarker {
        id: u64,
    },
    /// Place several markers in one undoable step, e.g. every song start at
    /// once: `[{"at": "3:57.1", "name": "Head Games"}, ...]`. With `replace`
    /// (the default) all existing markers are removed first.
    SetMarkers {
        markers: Vec<NewMarker>,
        #[serde(default = "yes")]
        replace: bool,
    },
    /// Remove all markers.
    ClearMarkers,

    // --- Album --------------------------------------------------------------
    /// Set album details; fields left out are kept. An empty `cover` removes it.
    SetAlbum {
        #[serde(default)]
        artist: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        year: Option<String>,
        #[serde(default)]
        genre: Option<String>,
        #[serde(default)]
        comment: Option<String>,
        #[serde(default)]
        cover: Option<PathBuf>,
    },
    /// Tracks (one per marker) with their levels, plus the album's.
    AlbumSummary {
        #[serde(default)]
        cd_safe: bool,
    },
    /// Level the whole album with one gain to `target_lufs`, keeping the peak
    /// at or below `ceiling_db`.
    AlbumLoudness {
        target_lufs: f64,
        #[serde(default)]
        ceiling_db: Option<f64>,
    },

    // --- Export -------------------------------------------------------------
    /// Render to file(s): `kind` mix (one file, optionally `start`..`end`),
    /// tracks (one file per marker region into the folder `path`, tagged),
    /// cue (one file plus a CUE sheet) or stems (one file per track).
    /// `preset` wav (32-bit float), cd (16-bit 44.1 kHz, CD-safe) or flac
    /// (24-bit). Existing files are only replaced with `force`; run with
    /// `dry_run` first to see the exact files, lengths and any warnings.
    Export {
        path: PathBuf,
        #[serde(flatten)]
        options: ExportOptions,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        force: bool,
    },
}

fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Result of an operation: a one-line summary of what happened, plus data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub summary: String,
    #[serde(flatten)]
    pub data: Data,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    None,
    Track {
        track: u64,
    },
    Marker {
        marker: u64,
    },
    /// Marker ids, in the order the markers were given.
    Markers {
        markers: Vec<u64>,
    },
    History(HistoryInfo),
    Project(Box<ProjectInfo>),
    Levels(Levels),
    Songs(SongsReport),
    Normalized {
        tracks: Vec<NormalizedTrack>,
    },
    Silences(Silences),
    Waveform(WaveformInfo),
    Album(AlbumReport),
    AlbumLoudness(LoudnessChange),
    Export(ExportReport),
    Compare(Box<CompareInfo>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryInfo {
    /// Undoable steps, oldest first.
    pub undo: Vec<String>,
    pub redo: Option<String>,
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub sample_rate: u32,
    pub channels: u16,
    pub length: TimePos,
    pub master_gain_db: Option<f64>,
    pub auto_crossfade: bool,
    pub album: AlbumInfo,
    pub tracks: Vec<TrackInfo>,
    pub markers: Vec<MarkerInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub id: u64,
    pub name: String,
    pub gain_db: Option<f64>,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub clips: Vec<ClipInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipInfo {
    pub id: u64,
    pub source: String,
    pub start: TimePos,
    pub end: TimePos,
    pub gain_db: Option<f64>,
    pub fade_in_secs: f64,
    pub fade_out_secs: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarkerInfo {
    pub id: u64,
    pub position: TimePos,
    pub name: String,
    /// Placed by song detection.
    pub auto: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

/// Levels in dBFS / LUFS; `None` where silent or too short to measure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Levels {
    pub start: TimePos,
    pub end: TimePos,
    /// Highest sample (dBFS).
    pub peak_db: Option<f64>,
    /// Highest peak between samples too (dBTP, BS.1770 true peak).
    #[serde(default)]
    pub true_peak_db: Option<f64>,
    pub rms_db: Option<f64>,
    /// Integrated loudness (LUFS).
    pub lufs: Option<f64>,
    /// Loudest 400 ms (LUFS).
    #[serde(default)]
    pub max_momentary_lufs: Option<f64>,
    /// Loudest 3 s (LUFS).
    #[serde(default)]
    pub max_short_term_lufs: Option<f64>,
    /// Loudness range (LU, EBU Tech 3342): how much the level varies.
    #[serde(default)]
    pub loudness_range_lu: Option<f64>,
    /// Samples above 0 dBFS: these clip when exported to an integer format.
    pub clipped_samples: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongsReport {
    /// Typical song loudness of the analysed range (LUFS).
    pub reference_lufs: f64,
    pub songs: Vec<SongStart>,
    /// Whether markers were placed.
    pub applied: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongStart {
    pub position: TimePos,
    pub name: String,
    /// How clearly this is a song change, 0..1 (`None` for the range start).
    pub confidence: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Silences {
    pub threshold_db: f64,
    pub ranges: Vec<Span>,
    /// More ranges were found than are listed.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: TimePos,
    pub end: TimePos,
    pub length: TimePos,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaveformInfo {
    /// The image (PNG, base64).
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub start: TimePos,
    pub end: TimePos,
    pub channels: u16,
    /// Timeline seconds covered by one pixel column.
    pub seconds_per_pixel: f64,
    /// Spacing of the faint vertical grid lines.
    pub grid_secs: f64,
    pub db_scale: bool,
    /// Markers in view, with their pixel column.
    pub markers: Vec<WaveformMarker>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaveformMarker {
    pub x: u32,
    pub name: String,
    pub position: TimePos,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompareInfo {
    /// What was compared, e.g. "the project", "history state 3 (after \"Split\")".
    pub a: String,
    pub b: String,
    /// Same length and every sample equal, bit for bit.
    pub identical: bool,
    /// No difference above the threshold, over the same length.
    pub nulls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_db: Option<f64>,
    /// Lengths compared (after any alignment). Where one is shorter, the
    /// rest of the other is compared against silence.
    pub length_a: TimePos,
    pub length_b: TimePos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<AlignmentInfo>,
    /// Frames where any channel differs (above the threshold).
    pub differing_frames: u64,
    /// Positions are in `a`'s time.
    pub first_difference: Option<TimePos>,
    pub last_difference: Option<TimePos>,
    /// Largest difference between samples (dBFS); `None` if none.
    pub peak_db: Option<f64>,
    pub rms_db: Option<f64>,
    /// RMS of the difference relative to `a`'s (dB): how deep the null is.
    /// -96 is about 16-bit rounding noise.
    pub null_depth_db: Option<f64>,
    pub channels: Vec<ChannelDifference>,
    /// Where they differ (differences under 0.1 s apart are joined).
    pub regions: Vec<DiffSpan>,
    /// More regions were found than are listed.
    pub truncated: bool,
    pub region_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlignmentInfo {
    /// Frames `b` starts later than `a` (negative: earlier), skipped before comparing.
    pub offset_frames: i64,
    pub offset_secs: f64,
    /// How well the openings matched, 0..1.
    pub correlation: f64,
    /// Whether a match was found; if not, nothing was shifted.
    pub found: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelDifference {
    pub channel: usize,
    pub peak_db: Option<f64>,
    pub rms_db: Option<f64>,
    pub differing_samples: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffSpan {
    pub start: TimePos,
    pub end: TimePos,
    pub length: TimePos,
    pub peak_db: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizedTrack {
    pub track: u64,
    pub measured: Option<f64>,
    pub gain_db: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlbumReport {
    pub album: AlbumInfo,
    pub tracks: Vec<AlbumTrack>,
    pub lufs: Option<f64>,
    pub peak_db: Option<f64>,
    pub master_gain_db: Option<f64>,
    /// CD tracks whose length is not a whole number of sectors (`None` if
    /// the sample rate can't be made CD-safe).
    pub sector_errors: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlbumTrack {
    pub number: usize,
    pub title: String,
    /// Marker the track starts at (`None` for audio before the first marker).
    pub marker: Option<u64>,
    pub start: TimePos,
    pub length: TimePos,
    pub lufs: Option<f64>,
    pub peak_db: Option<f64>,
    /// Things worth checking: `near_clipping`, `short` (< 30 s), `long` (> 20 min).
    pub flags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessChange {
    pub before_lufs: f64,
    pub after_lufs: f64,
    pub before_peak_db: Option<f64>,
    pub after_peak_db: Option<f64>,
    pub gain_db: f64,
    /// Whether the peak ceiling (not the target) set the gain.
    pub limited_by_ceiling: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportReport {
    pub written: bool,
    pub files: Vec<ExportFile>,
    pub format: String,
    /// CD tracks with sector boundary errors (CD-format track exports only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector_errors: Option<usize>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportFile {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Timeline span the file covers (absent for CUE sheets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<TimePos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<TimePos>,
    /// Would replace an existing file.
    pub exists: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidArgument,
    NotFound,
    Exists,
    Io,
    Decode,
    Encode,
    ProjectFormat,
    InvalidRange,
    NothingSelected,
    Silent,
    NothingToUndo,
    /// The project has unsaved changes (pass `discard` to drop them).
    UnsavedChanges,
    Cancelled,
    Unsupported,
    /// The project file changed on disk since it was opened or saved
    /// (pass `force` to replace it anyway).
    Modified,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct OpError {
    pub code: ErrorCode,
    pub message: String,
}

impl OpError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn arg(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }
}

impl From<crate::Error> for OpError {
    fn from(e: crate::Error) -> Self {
        use crate::Error as E;
        let code = match &e {
            E::Io(_) => ErrorCode::Io,
            E::Decode { .. } => ErrorCode::Decode,
            E::Encode(_) | E::Resample(_) => ErrorCode::Encode,
            E::ProjectFormat(_) => ErrorCode::ProjectFormat,
            E::InvalidRange => ErrorCode::InvalidRange,
            E::NothingSelected => ErrorCode::NothingSelected,
            E::NotFound(_) => ErrorCode::NotFound,
            E::Silent => ErrorCode::Silent,
            E::Cancelled => ErrorCode::Cancelled,
            E::Unsupported(_) => ErrorCode::Unsupported,
            E::Naming(_) | E::Mismatch(_) => ErrorCode::InvalidArgument,
        };
        Self::new(code, e.to_string())
    }
}
