//! Editing operations on a [`Project`].
//!
//! All operations are non-destructive (they rearrange clips or change clip
//! metadata) and sample-exact: splitting anywhere, or cutting and pasting back
//! in place, renders bit-identical audio. Wrap calls in
//! [`Document::apply`](crate::Document::apply) to get undo/redo.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::project::{
    Clip, ClipId, Fade, FadeShape, IdGen, MarkerId, Project, SourceId, Track, TrackId,
};
use crate::render::{self, RenderOptions};
use crate::util::db_to_gain;
use crate::Frame;

fn check_range(start: Frame, end: Frame) -> Result<()> {
    if end <= start {
        Err(Error::InvalidRange)
    } else {
        Ok(())
    }
}

fn track_indices(p: &Project, tracks: &[TrackId]) -> Result<Vec<usize>> {
    if tracks.is_empty() {
        return Err(Error::NothingSelected);
    }
    tracks
        .iter()
        .map(|id| p.track_index(*id).ok_or(Error::NotFound("track")))
        .collect()
}

fn covers_all_tracks(p: &Project, tracks: &[TrackId]) -> bool {
    p.tracks.iter().all(|t| tracks.contains(&t.id))
}

// ---------------------------------------------------------------------------
// Per-track primitives
// ---------------------------------------------------------------------------

/// Split every clip on `track` that strictly contains `pos`.
fn split_track(track: &mut Track, ids: &mut IdGen, pos: Frame) {
    let mut new = Vec::new();
    for clip in &mut track.clips {
        if clip.strictly_contains(pos) {
            let left_len = pos - clip.position;
            let mut right = clip.clone();
            right.id = ids.clip();
            right.position = pos;
            right.source_start += left_len;
            right.length -= left_len;
            clip.length = left_len;
            clip.prune_fades();
            right.prune_fades();
            new.push(right);
        }
    }
    if !new.is_empty() {
        track.clips.extend(new);
        track.sort_clips();
    }
}

/// Split at both ends so every clip lies entirely inside or outside `start..end`.
fn isolate(track: &mut Track, ids: &mut IdGen, start: Frame, end: Frame) {
    split_track(track, ids, start);
    split_track(track, ids, end);
}

fn inside(c: &Clip, start: Frame, end: Frame) -> bool {
    c.position >= start && c.end() <= end
}

fn clear_track(track: &mut Track, ids: &mut IdGen, start: Frame, end: Frame) {
    isolate(track, ids, start, end);
    track.clips.retain(|c| !inside(c, start, end));
}

fn shift_after(track: &mut Track, from: Frame, delta: i64) {
    for c in &mut track.clips {
        if c.position >= from {
            c.position = (c.position as i64 + delta).max(0) as Frame;
        }
    }
    track.sort_clips();
}

// ---------------------------------------------------------------------------
// Range edits
// ---------------------------------------------------------------------------

/// Split clips on the given tracks at `pos`.
pub fn split_at(p: &mut Project, tracks: &[TrackId], pos: Frame) -> Result<()> {
    for ti in track_indices(p, tracks)? {
        split_track(&mut p.tracks[ti], &mut p.ids, pos);
    }
    Ok(())
}

/// Remove audio in the range, leaving silence (later material does not move).
pub fn clear_range(p: &mut Project, tracks: &[TrackId], start: Frame, end: Frame) -> Result<()> {
    check_range(start, end)?;
    for ti in track_indices(p, tracks)? {
        clear_track(&mut p.tracks[ti], &mut p.ids, start, end);
    }
    Ok(())
}

/// Remove the range and close the gap. Markers move too when every track is affected.
pub fn ripple_delete(p: &mut Project, tracks: &[TrackId], start: Frame, end: Frame) -> Result<()> {
    check_range(start, end)?;
    let len = (end - start) as i64;
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        clear_track(t, &mut p.ids, start, end);
        shift_after(t, end, -len);
    }
    if covers_all_tracks(p, tracks) {
        p.markers
            .retain(|m| !(m.position > start && m.position < end));
        for m in &mut p.markers {
            if m.position >= end {
                m.position -= len as Frame;
            }
        }
    }
    Ok(())
}

/// Insert `len` frames of silence at `pos`, pushing later material right.
pub fn insert_silence(p: &mut Project, tracks: &[TrackId], pos: Frame, len: Frame) -> Result<()> {
    if len == 0 {
        return Err(Error::InvalidRange);
    }
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        split_track(t, &mut p.ids, pos);
        shift_after(t, pos, len as i64);
    }
    if covers_all_tracks(p, tracks) {
        for m in &mut p.markers {
            if m.position >= pos {
                m.position += len;
            }
        }
    }
    Ok(())
}

/// Keep only the range on the given tracks (positions are unchanged).
pub fn crop(p: &mut Project, tracks: &[TrackId], start: Frame, end: Frame) -> Result<()> {
    check_range(start, end)?;
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        isolate(t, &mut p.ids, start, end);
        t.clips.retain(|c| inside(c, start, end));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Copied material: one clip list per track, positions relative to the copy start.
#[derive(Clone, Debug, PartialEq)]
pub struct Clipboard {
    pub length: Frame,
    pub tracks: Vec<Vec<Clip>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PasteMode {
    /// Push existing material right to make room.
    #[default]
    Insert,
    /// Replace whatever is under the pasted range.
    Overwrite,
    /// Layer on top of existing material.
    Mix,
}

pub fn copy(p: &Project, tracks: &[TrackId], start: Frame, end: Frame) -> Result<Clipboard> {
    check_range(start, end)?;
    let mut out = Vec::new();
    for ti in track_indices(p, tracks)? {
        let clips = p.tracks[ti]
            .clips
            .iter()
            .filter(|c| c.overlaps(start, end))
            .map(|c| {
                let mut c = c.clone();
                if c.position < start {
                    let d = start - c.position;
                    c.source_start += d;
                    c.length -= d;
                    c.position = start;
                }
                if c.end() > end {
                    c.length = end - c.position;
                }
                c.position -= start;
                c.prune_fades();
                c
            })
            .collect();
        out.push(clips);
    }
    Ok(Clipboard {
        length: end - start,
        tracks: out,
    })
}

/// Paste at `pos`. Clipboard track *i* goes to `tracks[i]`; new tracks are
/// created if the clipboard has more tracks than targets. Returns new clip ids.
pub fn paste(
    p: &mut Project,
    clip: &Clipboard,
    tracks: &[TrackId],
    pos: Frame,
    mode: PasteMode,
) -> Result<Vec<ClipId>> {
    if clip.tracks.is_empty() || clip.length == 0 {
        return Err(Error::NothingSelected);
    }
    let mut targets: Vec<TrackId> = tracks.iter().copied().take(clip.tracks.len()).collect();
    while targets.len() < clip.tracks.len() {
        let n = p.tracks.len() + 1;
        targets.push(p.add_track(format!("Track {n}")));
    }
    match mode {
        PasteMode::Insert => insert_silence(p, &targets, pos, clip.length)?,
        PasteMode::Overwrite => clear_range(p, &targets, pos, pos + clip.length)?,
        PasteMode::Mix => {}
    }
    let mut new_ids = Vec::new();
    for (tid, clips) in targets.iter().zip(&clip.tracks) {
        let ti = p.track_index(*tid).ok_or(Error::NotFound("track"))?;
        for c in clips {
            let mut c = c.clone();
            c.id = p.ids.clip();
            c.position += pos;
            new_ids.push(c.id);
            p.tracks[ti].clips.push(c);
        }
        p.tracks[ti].sort_clips();
    }
    Ok(new_ids)
}

// ---------------------------------------------------------------------------
// Gain, normalization, fades
// ---------------------------------------------------------------------------

/// Change gain of the range (or whole tracks when `range` is `None`) by `db`.
pub fn apply_gain(
    p: &mut Project,
    tracks: &[TrackId],
    range: Option<(Frame, Frame)>,
    db: f64,
) -> Result<()> {
    let factor = db_to_gain(db) as f32;
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        let (s, e) = range.unwrap_or((0, t.end()));
        if e <= s {
            continue;
        }
        isolate(t, &mut p.ids, s, e);
        for c in t.clips.iter_mut().filter(|c| inside(c, s, e)) {
            c.gain *= factor;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalizeMode {
    /// Sample peak, dBFS.
    #[default]
    Peak,
    /// RMS level, dBFS.
    Rms,
    /// Integrated loudness (ITU-R BS.1770 / EBU R128), LUFS.
    Loudness,
}

impl NormalizeMode {
    pub const ALL: [NormalizeMode; 3] = [Self::Peak, Self::Rms, Self::Loudness];

    pub fn label(self) -> &'static str {
        match self {
            Self::Peak => "Peak",
            Self::Rms => "RMS",
            Self::Loudness => "Loudness (LUFS)",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Loudness => "LUFS",
            _ => "dBFS",
        }
    }

    pub fn default_target(self) -> f64 {
        match self {
            Self::Peak => -1.0,
            Self::Rms => -20.0,
            Self::Loudness => -23.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizeSettings {
    pub mode: NormalizeMode,
    /// Target level in dBFS (peak/RMS) or LUFS (loudness).
    pub target_db: f64,
    /// Never let the sample peak exceed this (dBFS), even if the target asks for more.
    pub peak_ceiling_db: Option<f64>,
    /// Normalize each track on its own. Otherwise one common gain is used so
    /// the loudest track hits the target and the balance is preserved.
    pub independent: bool,
}

impl Default for NormalizeSettings {
    fn default() -> Self {
        Self {
            mode: NormalizeMode::Peak,
            target_db: -1.0,
            peak_ceiling_db: None,
            independent: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NormalizeResult {
    pub track: TrackId,
    /// Level before normalization (`None` if silent).
    pub measured: Option<f64>,
    /// Gain applied in dB (`None` if the track was skipped).
    pub gain_db: Option<f64>,
}

/// Normalize the range (or whole tracks when `range` is `None`) by adjusting
/// clip gain. Measurement is pre-fader: clip gain and fades are included,
/// track gain/pan are not. Nothing is ever clipped internally (32-bit float).
pub fn normalize(
    p: &mut Project,
    tracks: &[TrackId],
    range: Option<(Frame, Frame)>,
    s: &NormalizeSettings,
) -> Result<Vec<NormalizeResult>> {
    let idxs = track_indices(p, tracks)?;
    let mut work = Vec::new();
    for ti in idxs {
        let t = &mut p.tracks[ti];
        let (a, b) = range.unwrap_or((0, t.end()));
        if b > a {
            isolate(t, &mut p.ids, a, b);
        }
        work.push((ti, a, b));
    }

    let mut results = Vec::new();
    let mut gains = Vec::new();
    for &(ti, a, b) in &work {
        let id = p.tracks[ti].id;
        if b <= a {
            results.push(NormalizeResult {
                track: id,
                measured: None,
                gain_db: None,
            });
            gains.push(None);
            continue;
        }
        let m = render::measure(p, a, b, &RenderOptions::pre_fader(vec![id]));
        let level = match s.mode {
            NormalizeMode::Peak => Some(m.peak_db()),
            NormalizeMode::Rms => Some(m.rms_db()),
            NormalizeMode::Loudness => m.integrated_lufs(),
        }
        .filter(|l| l.is_finite());
        let mut gain = level.map(|l| s.target_db - l);
        if let (Some(g), Some(ceiling)) = (gain.as_mut(), s.peak_ceiling_db) {
            let peak = m.peak_db();
            if peak.is_finite() {
                *g = g.min(ceiling - peak);
            }
        }
        results.push(NormalizeResult {
            track: id,
            measured: level,
            gain_db: gain,
        });
        gains.push(gain);
    }

    if gains.iter().all(Option::is_none) {
        return Err(Error::Silent);
    }
    if !s.independent {
        let common = gains
            .iter()
            .flatten()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        for (g, r) in gains.iter_mut().zip(&mut results) {
            if g.is_some() {
                *g = Some(common);
                r.gain_db = Some(common);
            }
        }
    }

    for (&(ti, a, b), g) in work.iter().zip(&gains) {
        if let Some(db) = g {
            let factor = db_to_gain(*db) as f32;
            for c in p.tracks[ti].clips.iter_mut().filter(|c| inside(c, a, b)) {
                c.gain *= factor;
            }
        }
    }
    Ok(results)
}

/// Fade in across `start..end` (continuous across clip boundaries).
pub fn fade_in(
    p: &mut Project,
    tracks: &[TrackId],
    start: Frame,
    end: Frame,
    shape: FadeShape,
) -> Result<()> {
    check_range(start, end)?;
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        isolate(t, &mut p.ids, start, end);
        for c in t.clips.iter_mut().filter(|c| inside(c, start, end)) {
            c.fade_in = Some(Fade {
                anchor: c.to_source(start),
                length: end - start,
                shape,
                auto: false,
            });
        }
    }
    Ok(())
}

/// Fade out across `start..end` (continuous across clip boundaries).
pub fn fade_out(
    p: &mut Project,
    tracks: &[TrackId],
    start: Frame,
    end: Frame,
    shape: FadeShape,
) -> Result<()> {
    check_range(start, end)?;
    for ti in track_indices(p, tracks)? {
        let t = &mut p.tracks[ti];
        isolate(t, &mut p.ids, start, end);
        for c in t.clips.iter_mut().filter(|c| inside(c, start, end)) {
            c.fade_out = Some(Fade {
                anchor: c.to_source(end),
                length: end - start,
                shape,
                auto: false,
            });
        }
    }
    Ok(())
}

/// Remove all fades from clips in the range.
pub fn remove_fades(p: &mut Project, tracks: &[TrackId], start: Frame, end: Frame) -> Result<()> {
    for ti in track_indices(p, tracks)? {
        for c in p.tracks[ti]
            .clips
            .iter_mut()
            .filter(|c| c.overlaps(start, end))
        {
            c.fade_in = None;
            c.fade_out = None;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering in place
// ---------------------------------------------------------------------------

/// Render a track's range (clip gain and fades baked in, pre-fader) into a new
/// source that replaces the clips there. Useful to consolidate many edits.
pub fn bounce_track(
    p: &mut Project,
    track: TrackId,
    range: Option<(Frame, Frame)>,
) -> Result<SourceId> {
    let ti = p.track_index(track).ok_or(Error::NotFound("track"))?;
    let (a, b) = range.unwrap_or((0, p.tracks[ti].end()));
    check_range(a, b)?;
    let opts = RenderOptions::pre_fader(vec![track]);
    let name = format!("{} (rendered)", p.tracks[ti].name);
    let sid = match p.cache.clone() {
        // Stream the render to disk so long renders never sit in RAM.
        Some(cache) => {
            let mut w = cache.writer(p.channels as usize, p.sample_rate)?;
            render::for_each_chunk(p, a, b, &opts, |chunk, n| {
                w.write_frames(chunk, n)?;
                Ok(true)
            })?;
            let (audio, peaks) = w.finish()?;
            p.add_prepared_source(name, None, audio, peaks)
        }
        None => {
            let buf = render::render(p, a, b, &opts);
            p.add_source(name, None, buf)
        }
    };
    let t = &mut p.tracks[ti];
    clear_track(t, &mut p.ids, a, b);
    t.clips.push(Clip::new(p.ids.clip(), sid, 0, b - a, a));
    t.sort_clips();
    Ok(sid)
}

// ---------------------------------------------------------------------------
// Clip operations
// ---------------------------------------------------------------------------

pub fn remove_clips(p: &mut Project, ids: &[ClipId]) -> Result<()> {
    for t in &mut p.tracks {
        t.clips.retain(|c| !ids.contains(&c.id));
    }
    Ok(())
}

/// Move a clip to `position` on `to_track`.
pub fn move_clip(p: &mut Project, id: ClipId, to_track: TrackId, position: Frame) -> Result<()> {
    let to = p.track_index(to_track).ok_or(Error::NotFound("track"))?;
    let (ti, ci) = p.find_clip(id).ok_or(Error::NotFound("clip"))?;
    let mut clip = p.tracks[ti].clips.remove(ci);
    clip.position = position;
    p.tracks[to].clips.push(clip);
    p.tracks[to].sort_clips();
    let on = p.auto_crossfade;
    update_crossfades(&mut p.tracks[ti], on);
    if to != ti {
        update_crossfades(&mut p.tracks[to], on);
    }
    Ok(())
}

/// Move a clip's left edge to timeline `new_start`, revealing or hiding source audio.
pub fn trim_clip_start(p: &mut Project, id: ClipId, new_start: Frame) -> Result<()> {
    let (ti, ci) = p.find_clip(id).ok_or(Error::NotFound("clip"))?;
    let c = &mut p.tracks[ti].clips[ci];
    let earliest = c.position.saturating_sub(c.source_start);
    let new_start = new_start.clamp(earliest, c.end() - 1);
    if new_start < c.position {
        let d = c.position - new_start;
        c.source_start -= d;
        c.length += d;
    } else {
        let d = new_start - c.position;
        c.source_start += d;
        c.length -= d;
    }
    c.position = new_start;
    if let Some(f) = c.fade_in.as_mut() {
        // Revealed audio before the fade start would be silent; start the fade at the new edge.
        if (c.source_start as i64) < f.anchor {
            f.anchor = c.source_start as i64;
        }
    }
    c.prune_fades();
    p.tracks[ti].sort_clips();
    let on = p.auto_crossfade;
    update_crossfades(&mut p.tracks[ti], on);
    Ok(())
}

/// Move a clip's right edge to timeline `new_end`.
pub fn trim_clip_end(p: &mut Project, id: ClipId, new_end: Frame) -> Result<()> {
    let (ti, ci) = p.find_clip(id).ok_or(Error::NotFound("clip"))?;
    let source_frames = p
        .sources
        .get(&p.tracks[ti].clips[ci].source)
        .map(|s| s.frames())
        .ok_or(Error::NotFound("source"))?;
    let c = &mut p.tracks[ti].clips[ci];
    let latest = c.position + (source_frames - c.source_start);
    c.length = new_end.clamp(c.position + 1, latest) - c.position;
    let source_end = c.source_end() as i64;
    if let Some(f) = c.fade_out.as_mut() {
        // Revealed audio after the fade end would be silent; end the fade at the new edge.
        if f.anchor > source_end {
            f.anchor = source_end;
        }
    }
    c.prune_fades();
    let on = p.auto_crossfade;
    update_crossfades(&mut p.tracks[ti], on);
    Ok(())
}

// ---------------------------------------------------------------------------
// Clip-edge fades and automatic crossfades
// ---------------------------------------------------------------------------

/// Shape used for automatic crossfades: equal power keeps the perceived level
/// steady when the overlapping material is unrelated, the common case.
pub const CROSSFADE_SHAPE: FadeShape = FadeShape::EqualPower;

fn clip_mut(p: &mut Project, id: ClipId) -> Result<&mut Clip> {
    let (ti, ci) = p.find_clip(id).ok_or(Error::NotFound("clip"))?;
    Ok(&mut p.tracks[ti].clips[ci])
}

/// Fade in from the clip's left edge over `length` frames (clamped to the
/// clip). A length of 0 removes the fade-in.
pub fn set_clip_fade_in(
    p: &mut Project,
    id: ClipId,
    length: Frame,
    shape: FadeShape,
) -> Result<()> {
    let c = clip_mut(p, id)?;
    let length = length.min(c.length);
    c.fade_in = (length > 0).then_some(Fade {
        anchor: c.source_start as i64,
        length,
        shape,
        auto: false,
    });
    Ok(())
}

/// Fade out to the clip's right edge over `length` frames (clamped to the
/// clip). A length of 0 removes the fade-out.
pub fn set_clip_fade_out(
    p: &mut Project,
    id: ClipId,
    length: Frame,
    shape: FadeShape,
) -> Result<()> {
    let c = clip_mut(p, id)?;
    let length = length.min(c.length);
    c.fade_out = (length > 0).then_some(Fade {
        anchor: c.source_end() as i64,
        length,
        shape,
        auto: false,
    });
    Ok(())
}

/// Change the curve of a clip's existing fades.
pub fn set_clip_fade_shape(p: &mut Project, id: ClipId, shape: FadeShape) -> Result<()> {
    let c = clip_mut(p, id)?;
    for f in [&mut c.fade_in, &mut c.fade_out].into_iter().flatten() {
        f.shape = shape;
    }
    Ok(())
}

/// Turn automatic crossfades on or off, updating every track.
pub fn set_auto_crossfade(p: &mut Project, on: bool) -> Result<()> {
    p.auto_crossfade = on;
    for t in &mut p.tracks {
        update_crossfades(t, on);
    }
    Ok(())
}

/// Recompute automatic crossfades on a track: remove the old ones, then where
/// one clip's tail overlaps the next clip's head, fade the first out and the
/// second in across the overlap. Edges with a fade you made are left alone, as
/// are clips completely inside another (that is layering, not a transition).
#[allow(clippy::needless_range_loop)] // pairwise i/j indexing is clearest here
fn update_crossfades(track: &mut Track, enabled: bool) {
    for c in &mut track.clips {
        if c.fade_in.is_some_and(|f| f.auto) {
            c.fade_in = None;
        }
        if c.fade_out.is_some_and(|f| f.auto) {
            c.fade_out = None;
        }
    }
    if !enabled {
        return;
    }
    let n = track.clips.len();
    let mut fade_in = vec![0; n];
    let mut fade_out = vec![0; n];
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (&track.clips[i], &track.clips[j]);
            if b.position >= a.end() {
                break; // sorted by position: no later clip overlaps `a` either
            }
            if b.position == a.position || b.end() <= a.end() {
                continue; // one clip contains the other
            }
            let overlap = a.end() - b.position;
            fade_out[i] = fade_out[i].max(overlap);
            fade_in[j] = fade_in[j].max(overlap);
        }
    }
    for (c, (fi, fo)) in track
        .clips
        .iter_mut()
        .zip(fade_in.into_iter().zip(fade_out))
    {
        if fi > 0 && c.fade_in.is_none() {
            c.fade_in = Some(Fade {
                anchor: c.source_start as i64,
                length: fi,
                shape: CROSSFADE_SHAPE,
                auto: true,
            });
        }
        if fo > 0 && c.fade_out.is_none() {
            c.fade_out = Some(Fade {
                anchor: c.source_end() as i64,
                length: fo,
                shape: CROSSFADE_SHAPE,
                auto: true,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Tracks and markers
// ---------------------------------------------------------------------------

pub fn remove_tracks(p: &mut Project, ids: &[TrackId]) -> Result<()> {
    p.tracks.retain(|t| !ids.contains(&t.id));
    Ok(())
}

pub fn move_track(p: &mut Project, id: TrackId, new_index: usize) -> Result<()> {
    let i = p.track_index(id).ok_or(Error::NotFound("track"))?;
    let t = p.tracks.remove(i);
    let new_index = new_index.min(p.tracks.len());
    p.tracks.insert(new_index, t);
    Ok(())
}

pub fn remove_marker(p: &mut Project, id: MarkerId) -> Result<()> {
    p.markers.retain(|m| m.id != id);
    Ok(())
}

pub fn move_marker(p: &mut Project, id: MarkerId, position: Frame) -> Result<()> {
    p.marker_mut(id).ok_or(Error::NotFound("marker"))?.position = position;
    p.sort_markers();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::render;
    use crate::AudioBuffer;

    const SR: u32 = 1000;

    /// Two tracks of distinct noise-like content, stereo source on track 1.
    fn project() -> (Project, TrackId, TrackId) {
        let mut p = Project::new(SR, 2);
        let a: Vec<f32> = (0..1000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        let b: Vec<f32> = (0..1000)
            .map(|i| ((i * 104_729) % 997) as f32 / 997.0 - 0.5)
            .collect();
        let t1 = p.add_source_as_track(
            "one",
            None,
            AudioBuffer::new(SR, vec![a.clone(), b.clone()]),
            0,
        );
        let t2 = p.add_source_as_track("two", None, AudioBuffer::new(SR, vec![b]), 100);
        (p, t1, t2)
    }

    fn mix(p: &Project) -> AudioBuffer {
        render(p, 0, p.length().max(1200), &RenderOptions::default())
    }

    #[test]
    fn split_is_sample_exact() {
        let (mut p, t1, t2) = project();
        let before = mix(&p);
        split_at(&mut p, &[t1, t2], 333).unwrap();
        split_at(&mut p, &[t1], 334).unwrap();
        assert_eq!(p.track(t1).unwrap().clips.len(), 3);
        assert_eq!(p.track(t2).unwrap().clips.len(), 2);
        assert_eq!(mix(&p), before);
    }

    #[test]
    fn split_inside_fade_is_sample_exact() {
        let (mut p, t1, _) = project();
        fade_in(&mut p, &[t1], 0, 400, FadeShape::SCurve).unwrap();
        fade_out(&mut p, &[t1], 600, 1000, FadeShape::EqualPower).unwrap();
        let before = mix(&p);
        for pos in [10, 200, 399, 401, 650, 999] {
            split_at(&mut p, &[t1], pos).unwrap();
        }
        assert_eq!(mix(&p), before);
    }

    #[test]
    fn split_outside_clip_is_noop() {
        let (mut p, _, t2) = project();
        let before = p.clone();
        split_at(&mut p, &[t2], 50).unwrap();
        split_at(&mut p, &[t2], 100).unwrap();
        assert_eq!(p, before);
    }

    #[test]
    fn cut_and_paste_back_is_identity() {
        let (mut p, t1, t2) = project();
        let before = mix(&p);
        let cb = copy(&p, &[t1, t2], 250, 600).unwrap();
        ripple_delete(&mut p, &[t1, t2], 250, 600).unwrap();
        assert_eq!(p.track(t1).unwrap().end(), 650);
        paste(&mut p, &cb, &[t1, t2], 250, PasteMode::Insert).unwrap();
        assert_eq!(mix(&p), before);
    }

    #[test]
    fn ripple_delete_shifts_markers_only_for_all_tracks() {
        let (mut p, t1, t2) = project();
        p.add_marker(300, "in range");
        p.add_marker(800, "after");
        let mut q = p.clone();
        ripple_delete(&mut p, &[t1, t2], 200, 500).unwrap();
        assert_eq!(p.markers.len(), 1);
        assert_eq!(p.markers[0].position, 500);
        ripple_delete(&mut q, &[t1], 200, 500).unwrap();
        assert_eq!(q.markers.len(), 2);
    }

    #[test]
    fn clear_leaves_silence() {
        let (mut p, t1, _) = project();
        clear_range(&mut p, &[t1], 100, 200).unwrap();
        let out = render(&p, 0, 1000, &RenderOptions::pre_fader(vec![t1]));
        assert!(out.channel(0)[100..200].iter().all(|&s| s == 0.0));
        assert!(out.channel(0)[200..300].iter().any(|&s| s != 0.0));
        assert_eq!(p.track(t1).unwrap().end(), 1000);
    }

    #[test]
    fn insert_silence_then_ripple_back() {
        let (mut p, t1, t2) = project();
        let before = mix(&p);
        insert_silence(&mut p, &[t1, t2], 500, 250).unwrap();
        assert_eq!(p.track(t1).unwrap().end(), 1250);
        ripple_delete(&mut p, &[t1, t2], 500, 750).unwrap();
        assert_eq!(mix(&p), before);
    }

    #[test]
    fn paste_overwrite_and_new_tracks() {
        let (mut p, t1, t2) = project();
        let cb = copy(&p, &[t1, t2], 0, 100).unwrap();
        paste(&mut p, &cb, &[t1], 500, PasteMode::Overwrite).unwrap();
        assert_eq!(
            p.tracks.len(),
            3,
            "second clipboard track creates a new track"
        );
        assert_eq!(p.track(t1).unwrap().end(), 1000, "overwrite keeps length");
    }

    #[test]
    fn crop_keeps_only_range() {
        let (mut p, t1, t2) = project();
        crop(&mut p, &[t1, t2], 200, 300).unwrap();
        for t in &p.tracks {
            assert!(t.clips.iter().all(|c| c.position >= 200 && c.end() <= 300));
        }
    }

    #[test]
    fn peak_normalize_hits_target() {
        let (mut p, t1, t2) = project();
        let res = normalize(
            &mut p,
            &[t1, t2],
            None,
            &NormalizeSettings {
                target_db: -3.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(res.len(), 2);
        for t in [t1, t2] {
            let m = render::measure(&p, 0, 1200, &RenderOptions::pre_fader(vec![t]));
            assert!((m.peak_db() - -3.0).abs() < 1e-4, "{}", m.peak_db());
        }
    }

    #[test]
    fn linked_normalize_preserves_balance() {
        let (mut p, t1, t2) = project();
        apply_gain(&mut p, &[t2], None, -12.0).unwrap();
        let s = NormalizeSettings {
            target_db: 0.0,
            independent: false,
            ..Default::default()
        };
        let res = normalize(&mut p, &[t1, t2], None, &s).unwrap();
        assert_eq!(res[0].gain_db, res[1].gain_db);
        let m1 = render::measure(&p, 0, 1200, &RenderOptions::pre_fader(vec![t1]));
        let m2 = render::measure(&p, 0, 1200, &RenderOptions::pre_fader(vec![t2]));
        assert!(m1.peak_db().abs() < 1e-3 || m2.peak_db().abs() < 1e-3);
        assert!(m2.peak_db() < -10.0);
    }

    #[test]
    fn normalize_range_only_affects_range() {
        let (mut p, t1, _) = project();
        let before = render(&p, 0, 1000, &RenderOptions::pre_fader(vec![t1]));
        normalize(
            &mut p,
            &[t1],
            Some((200, 400)),
            &NormalizeSettings {
                target_db: -20.0,
                ..Default::default()
            },
        )
        .unwrap();
        let after = render(&p, 0, 1000, &RenderOptions::pre_fader(vec![t1]));
        assert_eq!(&before.channel(0)[..200], &after.channel(0)[..200]);
        assert_eq!(&before.channel(0)[400..], &after.channel(0)[400..]);
        let m = render::measure(&p, 200, 400, &RenderOptions::pre_fader(vec![t1]));
        assert!((m.peak_db() - -20.0).abs() < 1e-4);
    }

    #[test]
    fn loudness_normalize_with_ceiling() {
        let sr = 48_000;
        let mut p = Project::new(sr, 2);
        let sine: Vec<f32> = (0..sr as usize * 5)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin() * 0.05)
            .collect();
        let t = p.add_source_as_track("s", None, AudioBuffer::new(sr, vec![sine.clone(), sine]), 0);
        let s = NormalizeSettings {
            mode: NormalizeMode::Loudness,
            target_db: -16.0,
            ..Default::default()
        };
        normalize(&mut p, &[t], None, &s).unwrap();
        let m = render::measure(&p, 0, p.length(), &RenderOptions::pre_fader(vec![t]));
        assert!((m.integrated_lufs().unwrap() - -16.0).abs() < 0.05);
        // A ceiling of -20 dBFS peak wins over the loudness target.
        let s = NormalizeSettings {
            peak_ceiling_db: Some(-20.0),
            ..s
        };
        normalize(&mut p, &[t], None, &s).unwrap();
        let m = render::measure(&p, 0, p.length(), &RenderOptions::pre_fader(vec![t]));
        assert!((m.peak_db() - -20.0).abs() < 1e-3);
    }

    #[test]
    fn normalize_silence_errors() {
        let mut p = Project::new(SR, 1);
        let t = p.add_source_as_track("z", None, AudioBuffer::silent(SR, 1, 100), 0);
        assert!(matches!(
            normalize(&mut p, &[t], None, &NormalizeSettings::default()),
            Err(Error::Silent)
        ));
    }

    #[test]
    fn trim_edges() {
        let (mut p, _, t2) = project();
        let id = p.track(t2).unwrap().clips[0].id;
        trim_clip_start(&mut p, id, 300).unwrap();
        let c = p.clip(id).unwrap();
        assert_eq!((c.position, c.source_start, c.length), (300, 200, 800));
        trim_clip_start(&mut p, id, 0).unwrap(); // clamps to source start
        let c = p.clip(id).unwrap();
        assert_eq!((c.position, c.source_start, c.length), (100, 0, 1000));
        trim_clip_end(&mut p, id, 5000).unwrap(); // clamps to source end
        assert_eq!(p.clip(id).unwrap().end(), 1100);
        trim_clip_end(&mut p, id, 600).unwrap();
        assert_eq!(p.clip(id).unwrap().end(), 600);
    }

    #[test]
    fn move_clip_between_tracks() {
        let (mut p, t1, t2) = project();
        let id = p.track(t2).unwrap().clips[0].id;
        move_clip(&mut p, id, t1, 2000).unwrap();
        assert!(p.track(t2).unwrap().clips.is_empty());
        assert_eq!(p.track(t1).unwrap().end(), 3000);
    }

    #[test]
    fn bounce_matches_original() {
        let (mut p, t1, _) = project();
        fade_in(&mut p, &[t1], 0, 300, FadeShape::Linear).unwrap();
        apply_gain(&mut p, &[t1], Some((500, 700)), -6.0).unwrap();
        split_at(&mut p, &[t1], 650).unwrap();
        let before = render(&p, 0, 1000, &RenderOptions::pre_fader(vec![t1]));
        bounce_track(&mut p, t1, None).unwrap();
        assert_eq!(p.track(t1).unwrap().clips.len(), 1);
        assert_eq!(
            render(&p, 0, 1000, &RenderOptions::pre_fader(vec![t1])),
            before
        );
    }

    /// One track, two clips of constant 1.0: 0..500 and 500..1000.
    fn two_clips() -> (Project, TrackId, ClipId, ClipId) {
        let mut p = Project::new(SR, 1);
        let t = p.add_source_as_track("ones", None, AudioBuffer::new(SR, vec![vec![1.0; 1000]]), 0);
        split_at(&mut p, &[t], 500).unwrap();
        let clips = &p.track(t).unwrap().clips;
        let (a, b) = (clips[0].id, clips[1].id);
        (p, t, a, b)
    }

    #[test]
    fn clip_edge_fades() {
        let (mut p, t, a, _) = two_clips();
        set_clip_fade_in(&mut p, a, 100, FadeShape::Linear).unwrap();
        set_clip_fade_out(&mut p, a, 5000, FadeShape::Linear).unwrap(); // clamped to clip length
        let c = p.clip(a).unwrap();
        assert_eq!((c.fade_in_frames(), c.fade_out_frames()), (100, 500));
        let out = render(&p, 0, 500, &RenderOptions::pre_fader(vec![t]));
        assert_eq!(out.channel(0)[0], 0.0);
        set_clip_fade_out(&mut p, a, 0, FadeShape::Linear).unwrap();
        assert_eq!(
            p.clip(a).unwrap().fade_out,
            None,
            "length 0 removes the fade"
        );
        set_clip_fade_shape(&mut p, a, FadeShape::SCurve).unwrap();
        assert_eq!(p.clip(a).unwrap().fade_in.unwrap().shape, FadeShape::SCurve);
    }

    #[test]
    fn overlapping_move_creates_crossfade_and_moving_apart_removes_it() {
        let (mut p, t, a, b) = two_clips();
        move_clip(&mut p, b, t, 400).unwrap(); // 100-frame overlap
        let (ca, cb) = (p.clip(a).unwrap(), p.clip(b).unwrap());
        assert_eq!(ca.fade_out_frames(), 100);
        assert_eq!(cb.fade_in_frames(), 100);
        assert!(ca.fade_out.unwrap().auto && cb.fade_in.unwrap().auto);
        // Equal-power crossfade of two unit signals: each at sin(45°) mid-way.
        let out = render(&p, 0, 900, &RenderOptions::pre_fader(vec![t]));
        let mid = 2.0 * (std::f32::consts::FRAC_PI_4).sin();
        assert!(
            (out.channel(0)[450] - mid).abs() < 0.02,
            "{}",
            out.channel(0)[450]
        );
        assert_eq!(out.channel(0)[300], 1.0);
        assert_eq!(out.channel(0)[600], 1.0);

        move_clip(&mut p, b, t, 600).unwrap();
        assert_eq!(p.clip(a).unwrap().fade_out, None);
        assert_eq!(p.clip(b).unwrap().fade_in, None);
    }

    #[test]
    fn crossfade_follows_trims_and_keeps_manual_fades() {
        let (mut p, t, a, b) = two_clips();
        set_clip_fade_out(&mut p, a, 50, FadeShape::Linear).unwrap();
        move_clip(&mut p, b, t, 700).unwrap();
        trim_clip_start(&mut p, b, 300).unwrap(); // b now starts at 300: overlap 200
        let (ca, cb) = (p.clip(a).unwrap(), p.clip(b).unwrap());
        assert_eq!(cb.fade_in_frames(), 200);
        assert!(!ca.fade_out.unwrap().auto, "manual fade untouched");
        assert_eq!(ca.fade_out_frames(), 50);
    }

    #[test]
    fn contained_clips_and_disabled_setting_get_no_crossfade() {
        let (mut p, t, a, b) = two_clips();
        trim_clip_end(&mut p, b, 600).unwrap();
        move_clip(&mut p, b, t, 100).unwrap(); // 100..200, fully inside a
        assert_eq!(p.clip(b).unwrap().fade_in, None);
        set_auto_crossfade(&mut p, false).unwrap();
        move_clip(&mut p, b, t, 450).unwrap(); // partial overlap, but disabled
        assert_eq!(p.clip(a).unwrap().fade_out, None);
        set_auto_crossfade(&mut p, true).unwrap();
        assert_eq!(
            p.clip(a).unwrap().fade_out_frames(),
            50,
            "re-enabling applies it"
        );
    }

    #[test]
    fn fade_spans_clip_boundaries() {
        let (mut p, t1, _) = project();
        split_at(&mut p, &[t1], 500).unwrap();
        let mut q = p.clone();
        fade_out(&mut p, &[t1], 300, 700, FadeShape::Linear).unwrap();
        // Same fade on an unsplit track renders the same.
        let id = q.track(t1).unwrap().clips[0].id;
        let next = q.track(t1).unwrap().clips[1].id;
        remove_clips(&mut q, &[next]).unwrap();
        trim_clip_end(&mut q, id, 1000).unwrap();
        fade_out(&mut q, &[t1], 300, 700, FadeShape::Linear).unwrap();
        let o = RenderOptions::pre_fader(vec![t1]);
        assert_eq!(render(&p, 0, 1000, &o), render(&q, 0, 1000, &o));
        // The fade reaches silence at the selection end; audio after it is untouched.
        let out = render(&p, 0, 1000, &o);
        assert!(out.channel(0)[699].abs() < 0.01);
        assert!(out.channel(0)[700..].iter().any(|&s| s.abs() > 0.1));
    }
}
