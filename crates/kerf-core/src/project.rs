//! The timeline data model.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::buffer::AudioBuffer;
use crate::cache::AudioCache;
use crate::peaks::PeakCache;
use crate::store::{Samples, SourceAudio};
use crate::Frame;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}
id_type!(SourceId);
id_type!(ClipId);
id_type!(TrackId);
id_type!(MarkerId);

/// Monotonic id allocator shared by all object kinds in a project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdGen {
    next: u64,
}

impl IdGen {
    pub fn alloc(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    pub fn clip(&mut self) -> ClipId {
        ClipId(self.alloc())
    }
    pub fn ensure_above(&mut self, id: u64) {
        self.next = self.next.max(id);
    }
}

/// Immutable audio material referenced by clips. Shared between undo snapshots.
pub struct Source {
    pub id: SourceId,
    pub name: String,
    /// Where the audio lives on disk. Unset for rendered material until the
    /// project is saved (which writes it out as 32-bit float WAV).
    pub path: OnceLock<PathBuf>,
    /// Samples at the project rate: in memory, or memory-mapped from the cache.
    pub audio: SourceAudio,
    pub peaks: PeakCache,
}

impl Source {
    /// A source held in memory.
    pub fn new(id: SourceId, name: String, path: Option<PathBuf>, buffer: AudioBuffer) -> Self {
        let peaks = PeakCache::build(&buffer);
        Self::with_peaks(id, name, path, SourceAudio::from_buffer(buffer), peaks)
    }

    /// Construct with a peak cache built elsewhere (e.g. on a worker thread).
    pub fn with_peaks(
        id: SourceId,
        name: String,
        path: Option<PathBuf>,
        audio: SourceAudio,
        peaks: PeakCache,
    ) -> Self {
        let cell = OnceLock::new();
        if let Some(p) = path {
            let _ = cell.set(p);
        }
        Self {
            id,
            name,
            path: cell,
            audio,
            peaks,
        }
    }

    pub fn frames(&self) -> Frame {
        self.audio.frames() as Frame
    }
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("path", &self.path.get())
            .field("frames", &self.audio.frames())
            .field("channels", &self.audio.num_channels())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeShape {
    Linear,
    /// Constant-power (sine) curve; best for crossfades.
    #[default]
    EqualPower,
    /// Smooth S-curve (raised cosine); gentle at both ends.
    SCurve,
    /// Logarithmic-feeling curve (squared); natural for fade-outs to silence.
    Exponential,
}

impl FadeShape {
    pub const ALL: [FadeShape; 4] = [
        FadeShape::Linear,
        FadeShape::EqualPower,
        FadeShape::SCurve,
        FadeShape::Exponential,
    ];

    /// Gain of a rising fade at position `t` in `0..=1`.
    pub fn gain(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeShape::Linear => t,
            FadeShape::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
            FadeShape::SCurve => 0.5 - 0.5 * (t * std::f32::consts::PI).cos(),
            FadeShape::Exponential => t * t,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FadeShape::Linear => "Linear",
            FadeShape::EqualPower => "Equal power",
            FadeShape::SCurve => "S-curve",
            FadeShape::Exponential => "Exponential",
        }
    }
}

/// A fade anchored in **source** frames, so that splitting or trimming a clip
/// never changes how it sounds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fade {
    /// Fade-in: source frame where gain starts rising from 0.
    /// Fade-out: source frame where gain reaches 0 (exclusive).
    pub anchor: i64,
    pub length: Frame,
    pub shape: FadeShape,
    /// Created automatically for an overlap (crossfade); recomputed when clips
    /// move. Fades you make yourself are never touched by that.
    #[serde(default)]
    pub auto: bool,
}

/// A reference to a range of a [`Source`] placed on the timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: SourceId,
    /// First source frame used.
    pub source_start: Frame,
    pub length: Frame,
    /// Timeline position of the first frame.
    pub position: Frame,
    /// Linear clip gain (normalization and gain edits adjust this).
    pub gain: f32,
    pub fade_in: Option<Fade>,
    pub fade_out: Option<Fade>,
}

impl Clip {
    pub fn new(
        id: ClipId,
        source: SourceId,
        source_start: Frame,
        length: Frame,
        position: Frame,
    ) -> Self {
        Self {
            id,
            source,
            source_start,
            length,
            position,
            gain: 1.0,
            fade_in: None,
            fade_out: None,
        }
    }

    pub fn end(&self) -> Frame {
        self.position + self.length
    }

    pub fn source_end(&self) -> Frame {
        self.source_start + self.length
    }

    /// Whether `pos` lies strictly inside the clip (a split there makes two non-empty clips).
    pub fn strictly_contains(&self, pos: Frame) -> bool {
        self.position < pos && pos < self.end()
    }

    pub fn overlaps(&self, start: Frame, end: Frame) -> bool {
        self.position < end && start < self.end()
    }

    /// Timeline frame → source frame (may be out of the clip's range).
    pub fn to_source(&self, timeline: Frame) -> i64 {
        timeline as i64 - self.position as i64 + self.source_start as i64
    }

    /// Total gain (clip gain × fades) at source frame `src`.
    #[inline]
    pub fn gain_at_source(&self, src: i64) -> f32 {
        let mut g = self.gain;
        if let Some(f) = &self.fade_in {
            let d = src - f.anchor;
            if d < 0 {
                return 0.0;
            }
            if (d as u64) < f.length {
                g *= f.shape.gain(d as f32 / f.length as f32);
            }
        }
        if let Some(f) = &self.fade_out {
            let d = f.anchor - src;
            if d <= 0 {
                return 0.0;
            }
            if (d as u64) <= f.length {
                g *= f.shape.gain(d as f32 / f.length as f32);
            }
        }
        g
    }

    /// Whether any fade affects frames inside this clip.
    pub fn has_active_fades(&self) -> bool {
        let s = self.source_start as i64;
        let e = self.source_end() as i64;
        let fi = self.fade_in.is_some_and(|f| f.anchor + f.length as i64 > s);
        let fo = self
            .fade_out
            .is_some_and(|f| f.anchor - (f.length as i64) < e);
        fi || fo
    }

    /// Frames at the clip's start covered by its fade-in (0 if none).
    pub fn fade_in_frames(&self) -> Frame {
        self.fade_in.map_or(0, |f| {
            (f.anchor + f.length as i64 - self.source_start as i64).clamp(0, self.length as i64)
                as Frame
        })
    }

    /// Frames at the clip's end covered by its fade-out (0 if none).
    pub fn fade_out_frames(&self) -> Frame {
        self.fade_out.map_or(0, |f| {
            (self.source_end() as i64 - (f.anchor - f.length as i64)).clamp(0, self.length as i64)
                as Frame
        })
    }

    /// Drop fades that no longer touch the clip's range (keeps data tidy after splits).
    pub fn prune_fades(&mut self) {
        let s = self.source_start as i64;
        let e = self.source_end() as i64;
        if self
            .fade_in
            .is_some_and(|f| f.anchor + f.length as i64 <= s && f.anchor <= s)
        {
            self.fade_in = None;
        }
        if self
            .fade_out
            .is_some_and(|f| f.anchor - f.length as i64 >= e && f.anchor >= e)
        {
            self.fade_out = None;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    /// Clips sorted by position. Overlapping clips are summed.
    pub clips: Vec<Clip>,
    /// Linear track gain applied post clip gain.
    pub gain: f32,
    /// Balance, -1 (left) .. 1 (right).
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            clips: Vec::new(),
            gain: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
        }
    }

    pub fn end(&self) -> Frame {
        self.clips.iter().map(Clip::end).max().unwrap_or(0)
    }

    pub fn sort_clips(&mut self) {
        self.clips.sort_by_key(|c| (c.position, c.id));
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }

    pub fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }

    /// Clip under a timeline position (topmost = last in order).
    pub fn clip_at(&self, pos: Frame) -> Option<&Clip> {
        self.clips
            .iter()
            .rev()
            .find(|c| c.position <= pos && pos < c.end())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: MarkerId,
    pub position: Frame,
    pub name: String,
    /// Placed by automatic detection; re-running detection replaces these
    /// but never touches markers you placed yourself.
    #[serde(default)]
    pub auto: bool,
    /// Track artist when it differs from the album artist (exported as a tag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// International Standard Recording Code for this track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

/// Album-level descriptive data, written as tags and into CUE sheets.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumInfo {
    pub artist: String,
    pub title: String,
    pub year: String,
    pub genre: String,
    pub comment: String,
    /// Front cover image (JPEG or PNG).
    pub cover: Option<PathBuf>,
}

/// The whole editable state. Cheap to clone (audio is shared via `Arc`).
#[derive(Clone, Debug)]
pub struct Project {
    pub sample_rate: u32,
    /// Output (mix/export) channel count.
    pub channels: u16,
    pub tracks: Vec<Track>,
    /// Sorted by position.
    pub markers: Vec<Marker>,
    pub sources: BTreeMap<SourceId, Arc<Source>>,
    pub ids: IdGen,
    /// Crossfade automatically where clips on a track overlap after a move or trim.
    pub auto_crossfade: bool,
    /// Where new audio (imports, renders) is stored. `None` keeps it in memory.
    pub cache: Option<Arc<AudioCache>>,
    pub album: AlbumInfo,
    /// Linear gain applied to the final mix (album loudness).
    pub master_gain: f32,
}

impl PartialEq for Project {
    fn eq(&self, other: &Self) -> bool {
        self.sample_rate == other.sample_rate
            && self.channels == other.channels
            && self.auto_crossfade == other.auto_crossfade
            && self.album == other.album
            && self.master_gain == other.master_gain
            && self.tracks == other.tracks
            && self.markers == other.markers
            && self.sources.len() == other.sources.len()
            && self
                .sources
                .iter()
                .zip(&other.sources)
                .all(|((a, sa), (b, sb))| a == b && Arc::ptr_eq(sa, sb))
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new(48_000, 2)
    }
}

impl Project {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels: channels.max(1),
            tracks: Vec::new(),
            markers: Vec::new(),
            sources: BTreeMap::new(),
            ids: IdGen::default(),
            auto_crossfade: true,
            cache: None,
            album: AlbumInfo::default(),
            master_gain: 1.0,
        }
    }

    /// True if nothing has been added yet (lets the first import set the rate).
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.sources.is_empty()
    }

    /// Register audio (already at the project rate) as a new source.
    pub fn add_source(
        &mut self,
        name: impl Into<String>,
        path: Option<PathBuf>,
        buffer: AudioBuffer,
    ) -> SourceId {
        debug_assert_eq!(buffer.sample_rate(), self.sample_rate);
        let id = SourceId(self.ids.alloc());
        self.sources
            .insert(id, Arc::new(Source::new(id, name.into(), path, buffer)));
        id
    }

    /// Like [`add_source`](Self::add_source) with a precomputed peak cache.
    pub fn add_prepared_source(
        &mut self,
        name: impl Into<String>,
        path: Option<PathBuf>,
        audio: SourceAudio,
        peaks: PeakCache,
    ) -> SourceId {
        debug_assert_eq!(audio.sample_rate(), self.sample_rate);
        let id = SourceId(self.ids.alloc());
        self.sources.insert(
            id,
            Arc::new(Source::with_peaks(id, name.into(), path, audio, peaks)),
        );
        id
    }

    /// Create a track holding the whole of `source` at `position`.
    pub fn add_track_with_source(
        &mut self,
        name: impl Into<String>,
        source: SourceId,
        position: Frame,
    ) -> Option<TrackId> {
        let frames = self.sources.get(&source)?.frames();
        let track = self.add_track(name);
        let clip = Clip::new(self.ids.clip(), source, 0, frames, position);
        self.track_mut(track)?.clips.push(clip);
        Some(track)
    }

    pub fn source(&self, id: SourceId) -> Option<&Arc<Source>> {
        self.sources.get(&id)
    }

    pub fn add_track(&mut self, name: impl Into<String>) -> TrackId {
        let id = TrackId(self.ids.alloc());
        self.tracks.push(Track::new(id, name));
        id
    }

    /// Add a source and place it on a new track at `position`.
    pub fn add_source_as_track(
        &mut self,
        name: impl Into<String>,
        path: Option<PathBuf>,
        buffer: AudioBuffer,
        position: Frame,
    ) -> TrackId {
        let name = name.into();
        let source = self.add_source(name.clone(), path, buffer);
        self.add_track_with_source(name, source, position)
            .expect("source was just added")
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    pub fn track_index(&self, id: TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// `(track index, clip index)` of a clip.
    pub fn find_clip(&self, id: ClipId) -> Option<(usize, usize)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.clips.iter().position(|c| c.id == id).map(|ci| (ti, ci)))
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.find_clip(id).map(|(t, c)| &self.tracks[t].clips[c])
    }

    /// End of the last clip on any track.
    pub fn length(&self) -> Frame {
        self.tracks.iter().map(Track::end).max().unwrap_or(0)
    }

    pub fn any_solo(&self) -> bool {
        self.tracks.iter().any(|t| t.solo)
    }

    /// Whether a track is heard in the mix given mute/solo state.
    pub fn is_audible(&self, track: &Track) -> bool {
        !track.muted && (track.solo || !self.any_solo())
    }

    pub fn all_track_ids(&self) -> Vec<TrackId> {
        self.tracks.iter().map(|t| t.id).collect()
    }

    pub fn add_marker(&mut self, position: Frame, name: impl Into<String>) -> MarkerId {
        self.push_marker(position, name.into(), false)
    }

    /// Add a marker flagged as automatically placed (see [`Marker::auto`]).
    pub fn add_auto_marker(&mut self, position: Frame, name: impl Into<String>) -> MarkerId {
        self.push_marker(position, name.into(), true)
    }

    fn push_marker(&mut self, position: Frame, name: String, auto: bool) -> MarkerId {
        let id = MarkerId(self.ids.alloc());
        self.markers.push(Marker {
            id,
            position,
            name,
            auto,
            artist: None,
            isrc: None,
        });
        self.sort_markers();
        id
    }

    pub fn sort_markers(&mut self) {
        self.markers.sort_by_key(|m| (m.position, m.id));
    }

    pub fn marker_mut(&mut self, id: MarkerId) -> Option<&mut Marker> {
        self.markers.iter_mut().find(|m| m.id == id)
    }

    pub fn secs_to_frames(&self, secs: f64) -> Frame {
        (secs.max(0.0) * self.sample_rate as f64).round() as Frame
    }

    pub fn frames_to_secs(&self, frames: Frame) -> f64 {
        frames as f64 / self.sample_rate as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_shapes_hit_endpoints() {
        for s in FadeShape::ALL {
            assert!(s.gain(0.0).abs() < 1e-6, "{s:?}");
            assert!((s.gain(1.0) - 1.0).abs() < 1e-6, "{s:?}");
        }
    }

    #[test]
    fn clip_fade_gain() {
        let mut c = Clip::new(ClipId(1), SourceId(1), 100, 1000, 0);
        c.fade_in = Some(Fade {
            anchor: 100,
            length: 100,
            shape: FadeShape::Linear,
            auto: false,
        });
        c.fade_out = Some(Fade {
            anchor: 1100,
            length: 100,
            shape: FadeShape::Linear,
            auto: false,
        });
        assert_eq!(c.gain_at_source(100), 0.0);
        assert!((c.gain_at_source(150) - 0.5).abs() < 1e-6);
        assert_eq!(c.gain_at_source(500), 1.0);
        assert!((c.gain_at_source(1050) - 0.5).abs() < 1e-6);
        assert!(c.has_active_fades());
    }
}
