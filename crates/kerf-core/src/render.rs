//! Mixing the timeline to audio, and exporting it to files.

use std::path::{Path, PathBuf};

use crate::analysis::Meter;
use crate::buffer::AudioBuffer;
use crate::encode::{AudioWriter, Tags};
use crate::error::{Error, Result};
use crate::io::ExportSettings;
use crate::project::{Project, TrackId};
use crate::resample::{exact_len, Sink, StreamResampler};
use crate::store::Samples;
use crate::util::sanitize_file_name;
use crate::Frame;

/// Frames processed per chunk for offline rendering.
pub const CHUNK: usize = 65_536;

#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    /// Restrict to these tracks. When set, solo state is ignored (mute is not,
    /// unless `pre_fader`).
    pub tracks: Option<Vec<TrackId>>,
    /// Ignore track gain, pan, mute and solo: hear clips (with their gain and
    /// fades) exactly as recorded on the track. Used for measuring.
    pub pre_fader: bool,
}

impl RenderOptions {
    pub fn tracks(ids: impl Into<Vec<TrackId>>) -> Self {
        Self {
            tracks: Some(ids.into()),
            pre_fader: false,
        }
    }

    pub fn pre_fader(ids: impl Into<Vec<TrackId>>) -> Self {
        Self {
            tracks: Some(ids.into()),
            pre_fader: true,
        }
    }
}

/// Mix `frames` frames starting at timeline `start` into `out` (planar, at
/// least `project.channels` channels each at least `frames` long). Does not
/// allocate, so it is safe to call from the real-time audio thread.
pub fn mix_into(
    project: &Project,
    start: Frame,
    frames: usize,
    out: &mut [Vec<f32>],
    opts: &RenderOptions,
) {
    let oc = project.channels as usize;
    for ch in out.iter_mut().take(oc) {
        ch[..frames].fill(0.0);
    }
    if frames == 0 {
        return;
    }
    let end = start + frames as Frame;
    let any_solo = opts.tracks.is_none() && project.any_solo();

    for track in &project.tracks {
        if let Some(filter) = &opts.tracks {
            if !filter.contains(&track.id) {
                continue;
            }
        }
        // Per output channel gains for stereo sources (balance: centre leaves
        // both sides untouched) and mono sources (constant power: centre is
        // -3 dB per side, so loudness stays steady while panning).
        let (balance, mono_pan, g) = if opts.pre_fader {
            ([1.0, 1.0], [1.0, 1.0], 1.0)
        } else {
            if track.muted || (any_solo && !track.solo) {
                continue;
            }
            let pan = track.pan.clamp(-1.0, 1.0);
            let theta = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            (
                [
                    track.gain * (1.0 - pan).min(1.0),
                    track.gain * (1.0 + pan).min(1.0),
                ],
                [track.gain * theta.cos(), track.gain * theta.sin()],
                track.gain,
            )
        };
        let channel_gain = |c: usize, source_channels: usize| match (oc, source_channels) {
            (2, 1) => mono_pan[c],
            (2, _) => balance[c],
            _ => g,
        };

        for clip in &track.clips {
            if clip.position >= end {
                break; // clips are sorted by position
            }
            if clip.end() <= start {
                continue;
            }
            let Some(src) = project.sources.get(&clip.source) else {
                continue;
            };
            let buf = &src.audio;
            let s = clip.position.max(start);
            let e = clip.end().min(end);
            let out_off = (s - start) as usize;
            let src_off = (s - clip.position + clip.source_start) as usize;
            let n = ((e - s) as usize).min(buf.frames().saturating_sub(src_off));
            if n == 0 {
                continue;
            }
            let fades = clip.has_active_fades();
            let sc = buf.num_channels();

            for (oc_i, out_ch) in out.iter_mut().enumerate().take(oc) {
                let cg = channel_gain(oc_i, sc);
                let dst = &mut out_ch[out_off..out_off + n];
                if sc == 1 || (oc != 1 && oc_i < sc) {
                    let c = if sc == 1 { 0 } else { oc_i };
                    let srcs = &buf.channel(c)[src_off..src_off + n];
                    if fades {
                        for (i, (d, s)) in dst.iter_mut().zip(srcs).enumerate() {
                            *d += s * clip.gain_at_source((src_off + i) as i64) * cg;
                        }
                    } else {
                        let gain = clip.gain * cg;
                        for (d, s) in dst.iter_mut().zip(srcs) {
                            *d += s * gain;
                        }
                    }
                } else if oc == 1 {
                    // Downmix to mono: average source channels.
                    let norm = 1.0 / sc as f32;
                    for (i, d) in dst.iter_mut().enumerate() {
                        let si = src_off + i;
                        let sum: f32 = (0..sc).map(|c| buf.channel(c)[si]).sum();
                        let gain = if fades {
                            clip.gain_at_source(si as i64)
                        } else {
                            clip.gain
                        };
                        *d += sum * norm * gain * cg;
                    }
                }
                // else: output channel beyond the source's channels stays silent.
            }
        }
    }
    if !opts.pre_fader && project.master_gain != 1.0 {
        for ch in out.iter_mut().take(oc) {
            for v in &mut ch[..frames] {
                *v *= project.master_gain;
            }
        }
    }
}

/// Render a range into a new buffer.
pub fn render(project: &Project, start: Frame, end: Frame, opts: &RenderOptions) -> AudioBuffer {
    let frames = end.saturating_sub(start) as usize;
    let mut out = vec![vec![0.0f32; frames]; project.channels as usize];
    let mut pos = 0;
    let mut chunk = vec![vec![0.0f32; CHUNK.min(frames.max(1))]; project.channels as usize];
    while pos < frames {
        let n = CHUNK.min(frames - pos);
        mix_into(project, start + pos as Frame, n, &mut chunk, opts);
        for (o, c) in out.iter_mut().zip(&chunk) {
            o[pos..pos + n].copy_from_slice(&c[..n]);
        }
        pos += n;
    }
    AudioBuffer::new(project.sample_rate, out)
}

/// Stream a range through `f` in chunks. `f` returns `false` to cancel.
pub fn for_each_chunk(
    project: &Project,
    start: Frame,
    end: Frame,
    opts: &RenderOptions,
    mut f: impl FnMut(&[Vec<f32>], usize) -> Result<bool>,
) -> Result<()> {
    // Long reads (exports, analysis) release mapped audio as they go, so a
    // pass over an hour of audio doesn't leave it all counted in memory.
    const RELEASE_EVERY_CHUNKS: usize = 64;
    let mut chunk = vec![vec![0.0f32; CHUNK]; project.channels as usize];
    let mut pos = start;
    let mut chunks = 0usize;
    while pos < end {
        let n = CHUNK.min((end - pos) as usize);
        mix_into(project, pos, n, &mut chunk, opts);
        if !f(&chunk, n)? {
            return Err(Error::Cancelled);
        }
        pos += n as Frame;
        chunks += 1;
        if chunks.is_multiple_of(RELEASE_EVERY_CHUNKS) {
            crate::store::release_mapped_memory();
        }
    }
    Ok(())
}

/// Measure levels of a rendered range.
pub fn measure(project: &Project, start: Frame, end: Frame, opts: &RenderOptions) -> Meter {
    let mut meter = Meter::new(project.sample_rate, project.channels as usize);
    // Infallible: the closure never errors or cancels.
    let _ = for_each_chunk(project, start, end, opts, |chunk, n| {
        meter.push(chunk, n);
        Ok(true)
    });
    meter
}

/// The zero crossing of the given tracks' mix (channels summed) nearest to
/// `frame`, searching up to `max_distance` frames either way. Cutting or
/// splitting there avoids clicks. Returns `frame` if there is none nearby.
pub fn nearest_zero_crossing(
    project: &Project,
    tracks: &[TrackId],
    frame: Frame,
    max_distance: Frame,
) -> Frame {
    let start = frame.saturating_sub(max_distance);
    let end = frame + max_distance + 1;
    let buf = render(
        project,
        start,
        end,
        &RenderOptions::pre_fader(tracks.to_vec()),
    );
    let mono: Vec<f32> = (0..buf.frames())
        .map(|i| (0..buf.num_channels()).map(|c| buf.channel(c)[i]).sum())
        .collect();
    let mut best: Option<(Frame, Frame)> = None; // (distance, position)
    for i in 1..mono.len() {
        let (a, b) = (mono[i - 1], mono[i]);
        let crosses = (a <= 0.0 && b > 0.0) || (a >= 0.0 && b < 0.0) || b == 0.0;
        if !crosses {
            continue;
        }
        // Of the two samples around the crossing, the one nearer zero.
        let pos = start
            + if a.abs() < b.abs() {
                i as Frame - 1
            } else {
                i as Frame
            };
        let d = pos.abs_diff(frame);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, pos));
        }
    }
    best.map_or(frame, |(_, p)| p)
}

/// Progress callback: receives 0..=1, returns `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f64) -> bool;

/// Render `start..end`, convert to the channel count and sample rate in
/// `settings`, and stream the result to `sink` in chunks.
fn render_converted(
    project: &Project,
    start: Frame,
    end: Frame,
    opts: &RenderOptions,
    settings: ExportSettings,
    progress: Progress,
    sink: &mut Sink,
) -> Result<()> {
    let pc = project.channels as usize;
    let oc = settings.channels_for(project.channels) as usize;
    let mut resampler = StreamResampler::new(
        project.sample_rate,
        settings.rate_for(project.sample_rate),
        oc,
    )?;
    let mut mapped: Vec<Vec<f32>> = vec![Vec::new(); oc];
    let total = (end - start) as f64;
    let mut done = 0usize;
    for_each_chunk(project, start, end, opts, |chunk, n| {
        map_channels(chunk, n, pc, &mut mapped);
        resampler.push(&mapped, n, sink)?;
        done += n;
        Ok(progress(done as f64 / total))
    })?;
    resampler.finish(sink)
}

/// Project channels → output channels: same count copies, mono is duplicated,
/// mono output averages, otherwise the first channels are kept.
fn map_channels(chunk: &[Vec<f32>], n: usize, pc: usize, out: &mut [Vec<f32>]) {
    let oc = out.len();
    for (o, dst) in out.iter_mut().enumerate() {
        dst.clear();
        if pc == 1 {
            dst.extend_from_slice(&chunk[0][..n]);
        } else if oc == 1 {
            dst.extend((0..n).map(|i| (0..pc).map(|c| chunk[c][i]).sum::<f32>() / pc as f32));
        } else if o < pc {
            dst.extend_from_slice(&chunk[o][..n]);
        } else {
            dst.resize(n, 0.0);
        }
    }
}

/// Render a range to a file (converted per `settings`), tagged with the
/// album's details.
pub fn export_range(
    project: &Project,
    start: Frame,
    end: Frame,
    opts: &RenderOptions,
    path: &Path,
    settings: ExportSettings,
    progress: Progress,
) -> Result<()> {
    let tags = Tags {
        title: project.album.title.clone(),
        ..album_tags(project)
    };
    export_range_tagged(project, start, end, opts, path, settings, &tags, progress)
}

#[allow(clippy::too_many_arguments)]
fn export_range_tagged(
    project: &Project,
    start: Frame,
    end: Frame,
    opts: &RenderOptions,
    path: &Path,
    settings: ExportSettings,
    tags: &Tags,
    progress: Progress,
) -> Result<()> {
    if end <= start {
        return Err(Error::InvalidRange);
    }
    let mut writer = AudioWriter::create(
        path,
        settings.rate_for(project.sample_rate),
        settings.channels_for(project.channels),
        settings,
    )?;
    let result = render_converted(
        project,
        start,
        end,
        opts,
        settings,
        progress,
        &mut |c: &[&[f32]]| writer.write_planar(c, c[0].len()),
    );
    match result {
        Ok(()) => writer.finalize(Some(tags)),
        Err(e) => {
            writer.abort();
            Err(e)
        }
    }
}

/// Tags common to every file exported from the project.
pub fn album_tags(p: &Project) -> Tags {
    let a = &p.album;
    Tags {
        artist: a.artist.clone(),
        album: a.title.clone(),
        album_artist: a.artist.clone(),
        date: a.year.clone(),
        genre: a.genre.clone(),
        comment: a.comment.clone(),
        cover: a.cover.clone(),
        ..Default::default()
    }
}

/// Tags for track `index` (0-based) of `total` made from `region`.
pub fn track_tags(p: &Project, region: &Region, index: usize, total: usize) -> Tags {
    let base = album_tags(p);
    Tags {
        title: region.name.clone(),
        artist: region.artist.clone().unwrap_or(base.artist.clone()),
        isrc: region.isrc.clone().unwrap_or_default(),
        track: Some((index as u32 + 1, total as u32)),
        ..base
    }
}

/// A named span of the timeline, e.g. one song of an album recording.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Region {
    pub start: Frame,
    pub end: Frame,
    pub name: String,
    /// Track artist, if it differs from the album artist.
    pub artist: Option<String>,
    pub isrc: Option<String>,
}

/// Split `0..end` at every marker. Each region takes the name of the marker it
/// starts at; audio before the first marker becomes a region named "Start".
pub fn regions_from_markers(project: &Project, end: Frame) -> Vec<Region> {
    let start_region = Region {
        start: 0,
        name: "Start".into(),
        ..Default::default()
    };
    let mut bounds: Vec<Region> = vec![start_region];
    for m in &project.markers {
        if m.position >= end {
            break;
        }
        let from_marker = Region {
            start: m.position,
            name: m.name.clone(),
            artist: m.artist.clone(),
            isrc: m.isrc.clone(),
            end: 0,
        };
        if m.position == 0 {
            bounds[0] = from_marker;
        } else if bounds.last().is_some_and(|b| b.start == m.position) {
            // Duplicate position: keep the first.
        } else {
            bounds.push(from_marker);
        }
    }
    let mut regions = Vec::new();
    for i in 0..bounds.len() {
        let region_end = bounds.get(i + 1).map_or(end, |b| b.start);
        if region_end > bounds[i].start {
            regions.push(Region {
                end: region_end,
                ..bounds[i].clone()
            });
        }
    }
    regions
}

/// The files [`export_regions`] writes for `regions`, in order: `dir` joined
/// with `template` (see [`crate::naming`]) filled in for each track. Fails
/// for a bad template or when two tracks would get the same file.
pub fn region_paths(
    project: &Project,
    regions: &[Region],
    dir: &Path,
    template: &str,
    project_name: &str,
    container: crate::encode::Container,
) -> Result<Vec<PathBuf>> {
    let a = &project.album;
    let total = regions.len();
    let mut paths: Vec<PathBuf> = Vec::with_capacity(total);
    for (i, r) in regions.iter().enumerate() {
        let fields = crate::naming::Fields {
            number: i + 1,
            total,
            title: &r.name,
            artist: r
                .artist
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&a.artist),
            album: &a.title,
            year: &a.year,
            genre: &a.genre,
            project: project_name,
        };
        let rel = crate::naming::expand(template, &fields).map_err(Error::Naming)?;
        let mut path = dir.join(rel);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        path.set_file_name(format!("{name}.{}", container.extension()));
        if let Some(j) = paths.iter().position(|p| *p == path) {
            return Err(Error::Naming(format!(
                "tracks {} and {} would both be {}; add {{nn}} to the name",
                j + 1,
                i + 1,
                path.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// The tracks [`export_stems`] exports (every unmuted track), with their files.
pub fn stem_paths<'a>(
    project: &'a Project,
    dir: &Path,
    container: crate::encode::Container,
) -> Vec<(&'a crate::project::Track, PathBuf)> {
    let tracks: Vec<_> = project.tracks.iter().filter(|t| !t.muted).collect();
    tracks
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let name = sanitize_file_name(&format!("{:02} - {}", i + 1, t.name));
            (t, dir.join(format!("{name}.{}", container.extension())))
        })
        .collect()
}

/// Export each region to its file in `paths` (from [`region_paths`]; folders
/// are created), tagged with its title, track number and the album's details.
///
/// Regions must be contiguous (as from [`regions_from_markers`]). The whole
/// span is rendered and converted in **one pass** and cut into files at the
/// converted boundaries, so the files join back seamlessly even when the
/// sample rate is converted (resampling each file separately would leave
/// tiny discontinuities at every boundary).
pub fn export_regions(
    project: &Project,
    regions: &[Region],
    opts: &RenderOptions,
    paths: &[PathBuf],
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let (Some(first), Some(last)) = (regions.first(), regions.last()) else {
        return Ok(Vec::new());
    };
    if regions.windows(2).any(|w| w[0].end != w[1].start) || paths.len() != regions.len() {
        return Err(Error::InvalidRange);
    }
    for dir in paths.iter().filter_map(|p| p.parent()) {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let (in_rate, out_rate) = (project.sample_rate, settings.rate_for(project.sample_rate));
    let channels = settings.channels_for(project.channels);
    // Where each file ends, in output frames from the start.
    let ends: Vec<u64> = regions
        .iter()
        .map(|r| exact_len(r.end - first.start, in_rate, out_rate))
        .collect();

    let mut current = 0usize;
    let mut written = 0u64;
    let mut writer: Option<AudioWriter> = None;
    let total = regions.len();
    let mut sink = |c: &[&[f32]]| -> Result<()> {
        let len = c[0].len() as u64;
        let mut off = 0u64;
        while off < len && current < paths.len() {
            let w = match &mut writer {
                Some(w) => w,
                None => writer.insert(AudioWriter::create(
                    &paths[current],
                    out_rate,
                    channels,
                    settings,
                )?),
            };
            let n = (ends[current] - written).min(len - off);
            let part: Vec<&[f32]> = c
                .iter()
                .map(|ch| &ch[off as usize..(off + n) as usize])
                .collect();
            w.write_planar(&part, n as usize)?;
            written += n;
            off += n;
            if written == ends[current] {
                writer.take().unwrap().finalize(Some(&track_tags(
                    project,
                    &regions[current],
                    current,
                    total,
                )))?;
                current += 1;
            }
        }
        Ok(())
    };
    render_converted(
        project,
        first.start,
        last.end,
        opts,
        settings,
        progress,
        &mut sink,
    )?;
    if let Some(w) = writer.take() {
        let last = current.min(total - 1);
        w.finalize(Some(&track_tags(project, &regions[last], last, total)))?;
    }
    Ok(paths.to_vec())
}

/// CUE sheet index time: `mm:ss:ff` with 75 frames per second (CD sectors).
/// CUE resolution is 1/75 s, so positions are rounded down to a sector.
fn cue_time(frames: Frame, sample_rate: u32) -> String {
    let sectors = frames * 75 / sample_rate.max(1) as Frame;
    format!(
        "{:02}:{:02}:{:02}",
        sectors / (75 * 60),
        (sectors / 75) % 60,
        sectors % 75
    )
}

fn cue_quote(s: &str) -> String {
    s.replace('"', "'")
}

/// Write a CUE sheet describing `regions` as the tracks of one audio file,
/// e.g. a live concert exported as a single gapless file. Album details
/// (performer, title, date, genre) and per-track performer and ISRC are
/// included when known; `fallback_title` is used if the album has no title.
pub fn write_cue(
    path: &Path,
    audio_file: &str,
    album: &crate::project::AlbumInfo,
    fallback_title: &str,
    regions: &[Region],
    sample_rate: u32,
) -> Result<()> {
    let mut cue = String::new();
    cue.push_str("REM COMMENT \"Kerf\"\n");
    if !album.year.is_empty() {
        cue.push_str(&format!("REM DATE {}\n", album.year));
    }
    if !album.genre.is_empty() {
        cue.push_str(&format!("REM GENRE \"{}\"\n", cue_quote(&album.genre)));
    }
    if !album.artist.is_empty() {
        cue.push_str(&format!("PERFORMER \"{}\"\n", cue_quote(&album.artist)));
    }
    let title = if album.title.is_empty() {
        fallback_title
    } else {
        &album.title
    };
    cue.push_str(&format!("TITLE \"{}\"\n", cue_quote(title)));
    cue.push_str(&format!("FILE \"{}\" WAVE\n", cue_quote(audio_file)));
    for (i, r) in regions.iter().enumerate() {
        cue.push_str(&format!("  TRACK {:02} AUDIO\n", i + 1));
        cue.push_str(&format!("    TITLE \"{}\"\n", cue_quote(&r.name)));
        if let Some(a) = r.artist.as_deref().filter(|a| !a.is_empty()) {
            cue.push_str(&format!("    PERFORMER \"{}\"\n", cue_quote(a)));
        }
        if let Some(isrc) = r.isrc.as_deref().filter(|c| !c.is_empty()) {
            cue.push_str(&format!("    ISRC {isrc}\n"));
        }
        cue.push_str(&format!(
            "    INDEX 01 {}\n",
            cue_time(r.start, sample_rate)
        ));
    }
    std::fs::write(path, cue)?;
    Ok(())
}

/// Export the whole mix as one file plus a CUE sheet with a track per region
/// (typically [`regions_from_markers`], optionally [`snap_to_sectors`]).
/// The file covers `0..` the end of the last region. Returns both paths.
pub fn export_with_cue(
    project: &Project,
    regions: &[Region],
    audio_path: &Path,
    title: &str,
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let end = regions.last().map_or(project.length(), |r| r.end);
    export_range(
        project,
        0,
        end,
        &RenderOptions::default(),
        audio_path,
        settings,
        progress,
    )?;
    let cue_path = audio_path.with_extension("cue");
    let file_name = audio_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    write_cue(
        &cue_path,
        &file_name,
        &project.album,
        title,
        regions,
        project.sample_rate,
    )?;
    Ok(vec![audio_path.to_path_buf(), cue_path])
}

// ---------------------------------------------------------------------------
// CD sectors
// ---------------------------------------------------------------------------

/// Frames per CD sector (1/75 s) at `sample_rate`: 588 at 44.1 kHz.
///
/// `None` if the rate is not a multiple of 75. For rates that are (48, 88.2,
/// 96 kHz…), a sector boundary converts to a CD sector boundary exactly, so
/// snapping at the project rate stays correct after conversion to 44.1 kHz.
pub fn sector_len(sample_rate: u32) -> Option<Frame> {
    (sample_rate > 0 && sample_rate.is_multiple_of(75)).then_some(sample_rate as Frame / 75)
}

/// Tracks (all but the last) whose length is not a whole number of sectors:
/// burned as separate files, each would get silence padded at its end, a
/// "sector boundary error" (SBE) heard as a gap or click on gapless albums.
pub fn sector_errors(regions: &[Region], sector: Frame) -> usize {
    let n = regions.len().saturating_sub(1);
    regions[..n]
        .iter()
        .filter(|r| (r.end - r.start) % sector != 0)
        .count()
}

/// Move every boundary between regions to the nearest sector boundary (at
/// most half a sector, 6.7 ms) and pad the end of the last region to a whole
/// sector, so no track has a sector boundary error. Regions that snapping
/// would make empty are merged into their neighbour. Markers are untouched.
pub fn snap_to_sectors(regions: &[Region], sector: Frame) -> Vec<Region> {
    let snap = |f: Frame| (f + sector / 2) / sector * sector;
    let mut out: Vec<Region> = Vec::new();
    for (i, r) in regions.iter().enumerate() {
        let start = if i == 0 { r.start } else { snap(r.start) };
        match out.last_mut() {
            Some(prev) if start <= prev.start => {} // collapsed into the previous region
            Some(prev) => {
                prev.end = start;
                out.push(Region { start, ..r.clone() });
            }
            None => out.push(Region { start, ..r.clone() }),
        }
    }
    if let Some(last) = out.last_mut() {
        let len = last.end - last.start;
        last.end = last.start + len.div_ceil(sector) * sector;
    }
    out
}

/// Export every audible track (post-fader, solo ignored) as its own file,
/// titled with the track's name.
pub fn export_stems(
    project: &Project,
    start: Frame,
    end: Frame,
    dir: &Path,
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let stems = stem_paths(project, dir, settings.container);
    let n = stems.len() as f64;
    let mut paths = Vec::new();
    for (i, (t, path)) in stems.into_iter().enumerate() {
        let tags = Tags {
            title: t.name.clone(),
            ..album_tags(project)
        };
        export_range_tagged(
            project,
            start,
            end,
            &RenderOptions::tracks(vec![t.id]),
            &path,
            settings,
            &tags,
            &mut |p| progress((i as f64 + p) / n),
        )?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Fade, FadeShape};

    fn ramp_project() -> (Project, TrackId) {
        let mut p = Project::new(1000, 1);
        let data: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let t = p.add_source_as_track("ramp", None, AudioBuffer::new(1000, vec![data]), 10);
        (p, t)
    }

    #[test]
    fn mixes_clip_at_position() {
        let (p, _) = ramp_project();
        let out = render(&p, 0, 120, &RenderOptions::default());
        assert_eq!(out.channel(0)[9], 0.0);
        assert_eq!(out.channel(0)[10], 0.0);
        assert_eq!(out.channel(0)[11], 1.0);
        assert_eq!(out.channel(0)[109], 99.0);
        assert_eq!(out.channel(0)[110], 0.0);
    }

    #[test]
    fn mute_solo_and_gain() {
        let (mut p, t) = ramp_project();
        let t2 = p.add_source_as_track(
            "ones",
            None,
            AudioBuffer::new(1000, vec![vec![1.0; 200]]),
            0,
        );
        p.track_mut(t).unwrap().gain = 2.0;
        let out = render(&p, 0, 200, &RenderOptions::default());
        assert_eq!(out.channel(0)[20], 2.0 * 10.0 + 1.0);
        p.track_mut(t2).unwrap().solo = true;
        let out = render(&p, 0, 200, &RenderOptions::default());
        assert_eq!(out.channel(0)[20], 1.0);
        // Pre-fader ignores gain and solo.
        let out = render(&p, 0, 200, &RenderOptions::pre_fader(vec![t]));
        assert_eq!(out.channel(0)[20], 10.0);
    }

    #[test]
    fn mono_source_pans_with_constant_power() {
        let mut p = Project::new(1000, 2);
        let t = p.add_source_as_track("x", None, AudioBuffer::new(1000, vec![vec![0.5; 10]]), 0);
        let lr = |p: &Project| {
            let out = render(p, 0, 10, &RenderOptions::default());
            (out.channel(0)[0], out.channel(1)[0])
        };
        let (l, r) = lr(&p);
        assert!(
            (l - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6 && l == r,
            "centre is -3 dB per side"
        );
        for pan in [-1.0, -0.6, -0.2, 0.3, 0.7, 1.0] {
            p.track_mut(t).unwrap().pan = pan;
            let (l, r) = lr(&p);
            assert!(
                (l * l + r * r - 0.25).abs() < 1e-6,
                "power constant at pan {pan}"
            );
        }
        p.track_mut(t).unwrap().pan = 1.0;
        let (l, r) = lr(&p);
        assert!(
            l.abs() < 1e-6 && (r - 0.5).abs() < 1e-6,
            "hard right: unity on the right only"
        );
        // Pre-fader (measurement) ignores pan entirely.
        let out = render(&p, 0, 10, &RenderOptions::pre_fader(vec![t]));
        assert_eq!((out.channel(0)[0], out.channel(1)[0]), (0.5, 0.5));
    }

    #[test]
    fn stereo_source_pans_with_balance() {
        let mut p = Project::new(1000, 2);
        let t = p.add_source_as_track(
            "x",
            None,
            AudioBuffer::new(1000, vec![vec![0.5; 10], vec![0.25; 10]]),
            0,
        );
        let out = render(&p, 0, 10, &RenderOptions::default());
        assert_eq!(
            (out.channel(0)[0], out.channel(1)[0]),
            (0.5, 0.25),
            "centre leaves stereo untouched"
        );
        p.track_mut(t).unwrap().pan = -0.5;
        let out = render(&p, 0, 10, &RenderOptions::default());
        assert_eq!(
            (out.channel(0)[0], out.channel(1)[0]),
            (0.5, 0.125),
            "left balance turns the right side down"
        );
    }

    #[test]
    fn fades_are_applied() {
        let mut p = Project::new(1000, 1);
        let t = p.add_source_as_track("x", None, AudioBuffer::new(1000, vec![vec![1.0; 100]]), 0);
        let clip = &mut p.track_mut(t).unwrap().clips[0];
        clip.fade_in = Some(Fade {
            anchor: 0,
            length: 10,
            shape: FadeShape::Linear,
            auto: false,
        });
        let out = render(&p, 0, 100, &RenderOptions::default());
        assert_eq!(out.channel(0)[0], 0.0);
        assert!((out.channel(0)[5] - 0.5).abs() < 1e-6);
        assert_eq!(out.channel(0)[50], 1.0);
    }

    #[test]
    fn regions_split_at_markers() {
        let (mut p, _) = ramp_project();
        p.add_marker(0, "Intro");
        p.add_marker(40, "Song");
        p.add_marker(40, "Dup");
        p.add_marker(200, "Past end");
        let r = regions_from_markers(&p, 110);
        assert_eq!(
            r,
            vec![
                Region {
                    start: 0,
                    end: 40,
                    name: "Intro".into(),
                    ..Default::default()
                },
                Region {
                    start: 40,
                    end: 110,
                    name: "Song".into(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn cue_sheet() {
        assert_eq!(cue_time(0, 48_000), "00:00:00");
        assert_eq!(cue_time(48_000 * 61 + 640, 48_000), "01:01:01"); // 640 samples = 1 sector at 48 kHz
        assert_eq!(cue_time(44_100 * 3600, 44_100), "60:00:00");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("show.cue");
        let regions = vec![
            Region {
                start: 0,
                end: 48_000,
                name: "Intro".into(),
                ..Default::default()
            },
            Region {
                start: 48_000 * 90,
                end: 48_000 * 200,
                name: "Say \"Hi\"".into(),
                ..Default::default()
            },
        ];
        write_cue(
            &path,
            "show.wav",
            &crate::project::AlbumInfo::default(),
            "Live",
            &regions,
            48_000,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("FILE \"show.wav\" WAVE"));
        assert!(text.contains("  TRACK 02 AUDIO\n    TITLE \"Say 'Hi'\"\n    INDEX 01 01:30:00"));
    }

    fn cd_project(markers: &[Frame]) -> Project {
        let mut p = Project::new(44_100, 2);
        let n = 44_100 * 20 + 123;
        let l: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.003).sin() * 0.5).collect();
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        p.add_source_as_track("show", None, AudioBuffer::new(44_100, vec![l, r]), 0);
        for (i, &m) in markers.iter().enumerate() {
            p.add_marker(m, format!("Song {}", i + 1));
        }
        p
    }

    #[test]
    fn sector_lengths() {
        assert_eq!(sector_len(44_100), Some(588));
        assert_eq!(sector_len(48_000), Some(640));
        assert_eq!(sector_len(32_000), None);
        // A 48 kHz sector converts to exactly one CD sector.
        assert_eq!(640 * 44_100 / 48_000, 588);
    }

    #[test]
    fn snapping_removes_sector_boundary_errors() {
        let p = cd_project(&[44_100 * 3 + 17, 44_100 * 9 + 300, 44_100 * 14 + 587]);
        let regions = regions_from_markers(&p, p.length());
        assert_eq!(
            sector_errors(&regions, 588),
            3,
            "unsnapped: every track but the last"
        );
        let snapped = snap_to_sectors(&regions, 588);
        assert_eq!(snapped.len(), regions.len());
        assert_eq!(sector_errors(&snapped, 588), 0);
        for (a, b) in regions.iter().zip(&snapped) {
            assert_eq!(b.start % 588, 0);
            assert!(
                a.start.abs_diff(b.start) <= 294,
                "moves at most half a sector"
            );
            assert_eq!(a.name, b.name);
        }
        for w in snapped.windows(2) {
            assert_eq!(w[0].end, w[1].start, "contiguous: nothing lost or repeated");
        }
        let last = snapped.last().unwrap();
        assert_eq!(last.end % 588, 0, "last track padded to a whole sector");
        assert!(last.end >= p.length() && last.end - p.length() < 588);
    }

    #[test]
    fn snapping_merges_markers_within_one_sector() {
        let p = cd_project(&[44_100 * 5, 44_100 * 5 + 100]);
        let snapped = snap_to_sectors(&regions_from_markers(&p, p.length()), 588);
        assert_eq!(snapped.len(), 2);
        assert_eq!(sector_errors(&snapped, 588), 0);
    }

    #[test]
    fn cd_safe_split_files_rejoin_to_the_original() {
        let p = cd_project(&[44_100 * 4 + 1, 44_100 * 11 + 299]);
        let regions = snap_to_sectors(&regions_from_markers(&p, p.length()), 588);
        let dir = tempfile::tempdir().unwrap();
        let settings = ExportSettings {
            format: crate::io::SampleFormat::Float32,
            dither: crate::dither::Dither::None,
            ..Default::default()
        };
        let paths = export_regions(
            &p,
            &regions,
            &RenderOptions::default(),
            &region_paths(
                &p,
                &regions,
                dir.path(),
                crate::naming::DEFAULT_TRACK_NAMES,
                "Live",
                settings.container,
            )
            .unwrap(),
            settings,
            &mut |_| true,
        )
        .unwrap();
        let mut joined = vec![Vec::new(), Vec::new()];
        for (i, path) in paths.iter().enumerate() {
            let buf = crate::io::decode_file(path).unwrap();
            if i + 1 < paths.len() {
                assert_eq!(buf.frames() % 588, 0, "{} has no SBE", path.display());
            }
            for (j, ch) in joined.iter_mut().enumerate() {
                ch.extend_from_slice(buf.channel(j));
            }
        }
        let original = render(
            &p,
            0,
            regions.last().unwrap().end,
            &RenderOptions::default(),
        );
        assert_eq!(
            joined,
            original.channels(),
            "gapless: split files rejoin bit-exactly"
        );

        // The CUE sheet points at exactly the same places.
        let wav = dir.path().join("Live.wav");
        let out = export_with_cue(&p, &regions, &wav, "Live", settings, &mut |_| true).unwrap();
        let cue = std::fs::read_to_string(&out[1]).unwrap();
        for r in &regions {
            assert!(cue.contains(&format!("INDEX 01 {}", cue_time(r.start, 44_100))));
            assert_eq!(r.start * 75 % 44_100, 0, "exact: no rounding in the CUE");
        }
    }

    #[test]
    fn export_converts_rate_and_channels() {
        let mut p = Project::new(48_000, 1);
        let sine: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        p.add_source_as_track("mono", None, AudioBuffer::new(48_000, vec![sine]), 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cd.wav");
        let settings = ExportSettings {
            dither: crate::dither::Dither::None,
            ..ExportSettings::AUDIO_CD
        };
        export_range(
            &p,
            0,
            48_000,
            &RenderOptions::default(),
            &path,
            settings,
            &mut |_| true,
        )
        .unwrap();
        let out = crate::io::decode_file(&path).unwrap();
        assert_eq!(
            (out.sample_rate(), out.num_channels(), out.frames()),
            (44_100, 2, 44_100)
        );
        assert_eq!(
            out.channel(0),
            out.channel(1),
            "mono duplicated to both sides"
        );
        for i in 10_000..10_050 {
            let ideal = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44_100.0).sin() * 0.5;
            assert!((out.channel(0)[i] - ideal).abs() < 0.01, "frame {i}");
        }
    }

    #[test]
    fn audio_cd_split_from_48k_is_gapless_and_sector_exact() {
        // A 48 kHz show, CD-safe at its own rate (640-frame sectors), exported as Audio CD.
        let mut p = Project::new(48_000, 2);
        let n = 48_000 * 12 + 77;
        let l: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.002).sin() * 0.6).collect();
        p.add_source_as_track(
            "show",
            None,
            AudioBuffer::new(48_000, vec![l.clone(), l]),
            0,
        );
        p.add_marker(48_000 * 4 + 311, "Two");
        p.add_marker(48_000 * 8 + 5, "Three");
        let regions = snap_to_sectors(
            &regions_from_markers(&p, p.length()),
            sector_len(48_000).unwrap(),
        );
        let settings = ExportSettings {
            dither: crate::dither::Dither::None,
            ..ExportSettings::AUDIO_CD
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = export_regions(
            &p,
            &regions,
            &RenderOptions::default(),
            &region_paths(
                &p,
                &regions,
                dir.path(),
                crate::naming::DEFAULT_TRACK_NAMES,
                "Live",
                settings.container,
            )
            .unwrap(),
            settings,
            &mut |_| true,
        )
        .unwrap();

        let whole_path = dir.path().join("whole.wav");
        export_range(
            &p,
            regions[0].start,
            regions.last().unwrap().end,
            &RenderOptions::default(),
            &whole_path,
            settings,
            &mut |_| true,
        )
        .unwrap();
        let whole = crate::io::decode_file(&whole_path).unwrap();

        let mut joined: Vec<Vec<f32>> = vec![Vec::new(); 2];
        for (i, path) in paths.iter().enumerate() {
            let b = crate::io::decode_file(path).unwrap();
            assert_eq!((b.sample_rate(), b.num_channels()), (44_100, 2));
            assert_eq!(b.frames() % 588, 0, "file {i} is whole CD sectors (no SBE)");
            for (j, ch) in joined.iter_mut().enumerate() {
                ch.extend_from_slice(b.channel(j));
            }
        }
        assert_eq!(
            joined,
            whole.channels(),
            "split files rejoin to exactly the one-piece export"
        );
    }

    #[test]
    fn non_contiguous_regions_are_rejected() {
        let (p, _) = ramp_project();
        let regions = vec![
            Region {
                start: 0,
                end: 10,
                name: "a".into(),
                ..Default::default()
            },
            Region {
                start: 20,
                end: 30,
                name: "b".into(),
                ..Default::default()
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let r = export_regions(
            &p,
            &regions,
            &RenderOptions::default(),
            &region_paths(
                &p,
                &regions,
                dir.path(),
                crate::naming::DEFAULT_TRACK_NAMES,
                "x",
                ExportSettings::default().container,
            )
            .unwrap(),
            ExportSettings::default(),
            &mut |_| true,
        );
        assert!(matches!(r, Err(Error::InvalidRange)));
    }

    #[test]
    fn zero_crossings() {
        // 100 Hz sine at 48 kHz: zero crossings every 240 frames, starting at 0.
        let mut p = Project::new(48_000, 1);
        let sine: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * (i as f32 + 0.5) / 48_000.0).sin())
            .collect();
        let t = p.add_source_as_track("s", None, AudioBuffer::new(48_000, vec![sine]), 0);
        for (from, want) in [(1000, 960), (1100, 1200), (1199, 1200), (5000, 5040)] {
            let z = nearest_zero_crossing(&p, &[t], from, 480);
            assert!(z.abs_diff(want) <= 1, "{from}: got {z}, want ~{want}");
        }
        // Nothing within reach: unchanged.
        let dc = p.add_source_as_track(
            "dc",
            None,
            AudioBuffer::new(48_000, vec![vec![0.5; 1000]]),
            0,
        );
        assert_eq!(nearest_zero_crossing(&p, &[dc], 500, 100), 500);
    }

    #[test]
    fn tagged_flac_album_export() {
        let mut p = Project::new(44_100, 2);
        let n = 44_100 * 9;
        let l: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.003).sin() * 0.5).collect();
        p.add_source_as_track(
            "show",
            None,
            AudioBuffer::new(44_100, vec![l.clone(), l]),
            0,
        );
        p.add_marker(0, "Opener");
        p.add_marker(44_100 * 3, "Ballad");
        let guest = p.add_marker(44_100 * 6, "Encore");
        p.marker_mut(guest).unwrap().artist = Some("Guest Star".into());
        p.album = crate::project::AlbumInfo {
            artist: "The Band".into(),
            title: "Live at the Hall".into(),
            year: "2026".into(),
            ..Default::default()
        };
        let regions = regions_from_markers(&p, p.length());
        let settings = ExportSettings {
            dither: crate::dither::Dither::None,
            ..ExportSettings::FLAC_ALBUM
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = export_regions(
            &p,
            &regions,
            &RenderOptions::default(),
            &region_paths(
                &p,
                &regions,
                dir.path(),
                crate::naming::DEFAULT_TRACK_NAMES,
                "Live",
                settings.container,
            )
            .unwrap(),
            settings,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[1].to_string_lossy().ends_with("02 - Ballad.flac"));

        let tag = |i: usize, k: &str| {
            let t = metaflac::Tag::read_from_path(&paths[i]).unwrap();
            t.get_vorbis(k)
                .and_then(|mut v| v.next())
                .map(str::to_string)
                .unwrap_or_default()
        };
        assert_eq!(tag(0, "TITLE"), "Opener");
        assert_eq!(tag(1, "TRACKNUMBER"), "2");
        assert_eq!(tag(1, "TRACKTOTAL"), "3");
        assert_eq!(tag(1, "ALBUM"), "Live at the Hall");
        assert_eq!(tag(1, "ARTIST"), "The Band");
        assert_eq!(tag(2, "ARTIST"), "Guest Star");
        assert_eq!(tag(2, "ALBUMARTIST"), "The Band");

        // Gapless: the tracks rejoin to exactly the one-piece export.
        let whole = dir.path().join("whole.flac");
        export_range(
            &p,
            0,
            p.length(),
            &RenderOptions::default(),
            &whole,
            settings,
            &mut |_| true,
        )
        .unwrap();
        let mut joined = vec![Vec::new(), Vec::new()];
        for path in &paths {
            let b = crate::io::decode_file(path).unwrap();
            for (c, ch) in joined.iter_mut().enumerate() {
                ch.extend_from_slice(b.channel(c));
            }
        }
        assert_eq!(joined, crate::io::decode_file(&whole).unwrap().channels());

        // The CUE sheet carries the same details.
        let cue_path = dir.path().join("show.flac");
        let out =
            export_with_cue(&p, &regions, &cue_path, "Live", settings, &mut |_| true).unwrap();
        let cue = std::fs::read_to_string(&out[1]).unwrap();
        assert!(cue.contains("PERFORMER \"The Band\""));
        assert!(cue.contains("TITLE \"Live at the Hall\""));
        assert!(cue.contains("FILE \"show.flac\" WAVE"));
        assert!(cue.contains("    PERFORMER \"Guest Star\""));
        assert!(cue.contains("REM DATE 2026"));
    }
}
