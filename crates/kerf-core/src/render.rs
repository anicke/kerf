//! Mixing the timeline to audio, and exporting it to files.

use std::path::{Path, PathBuf};

use crate::analysis::Meter;
use crate::buffer::AudioBuffer;
use crate::error::{Error, Result};
use crate::io::{ExportSettings, WavWriter};
use crate::project::{Project, TrackId};
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
    let mut chunk = vec![vec![0.0f32; CHUNK]; project.channels as usize];
    let mut pos = start;
    while pos < end {
        let n = CHUNK.min((end - pos) as usize);
        mix_into(project, pos, n, &mut chunk, opts);
        if !f(&chunk, n)? {
            return Err(Error::Cancelled);
        }
        pos += n as Frame;
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

/// Progress callback: receives 0..=1, returns `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f64) -> bool;

/// Render a range to a WAV file.
pub fn export_range(
    project: &Project,
    start: Frame,
    end: Frame,
    opts: &RenderOptions,
    path: &Path,
    settings: ExportSettings,
    progress: Progress,
) -> Result<()> {
    if end <= start {
        return Err(Error::InvalidRange);
    }
    let mut writer = WavWriter::create(path, project.sample_rate, project.channels, settings)?;
    let total = (end - start) as f64;
    let mut done = 0usize;
    let result = for_each_chunk(project, start, end, opts, |chunk, n| {
        writer.write_planar(chunk, n)?;
        done += n;
        Ok(progress(done as f64 / total))
    });
    match result {
        Ok(()) => writer.finalize(),
        Err(e) => {
            writer.abort();
            Err(e)
        }
    }
}

/// A named span of the timeline, e.g. one song of an album recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub start: Frame,
    pub end: Frame,
    pub name: String,
}

/// Split `0..end` at every marker. Each region takes the name of the marker it
/// starts at; audio before the first marker becomes a region named "Start".
pub fn regions_from_markers(project: &Project, end: Frame) -> Vec<Region> {
    let mut bounds: Vec<(Frame, String)> = vec![(0, "Start".into())];
    for m in &project.markers {
        if m.position >= end {
            break;
        }
        if m.position == 0 {
            bounds[0].1 = m.name.clone();
        } else if bounds.last().is_some_and(|b| b.0 == m.position) {
            // Duplicate position: keep the first name.
        } else {
            bounds.push((m.position, m.name.clone()));
        }
    }
    let mut regions = Vec::new();
    for (i, (start, name)) in bounds.iter().enumerate() {
        let region_end = bounds.get(i + 1).map_or(end, |b| b.0);
        if region_end > *start {
            regions.push(Region {
                start: *start,
                end: region_end,
                name: name.clone(),
            });
        }
    }
    regions
}

/// Export each region to `dir` as `NN - <base> - <region>.wav`.
pub fn export_regions(
    project: &Project,
    regions: &[Region],
    opts: &RenderOptions,
    dir: &Path,
    base_name: &str,
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let total: Frame = regions.iter().map(|r| r.end - r.start).sum();
    let mut done: Frame = 0;
    let mut paths = Vec::new();
    for (i, r) in regions.iter().enumerate() {
        let file = sanitize_file_name(&format!("{:02} - {} - {}", i + 1, base_name, r.name));
        let path = dir.join(format!("{file}.wav"));
        let len = r.end - r.start;
        export_range(project, r.start, r.end, opts, &path, settings, &mut |p| {
            progress((done as f64 + p * len as f64) / total.max(1) as f64)
        })?;
        done += len;
        paths.push(path);
    }
    Ok(paths)
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
/// e.g. a live concert exported as a single gapless WAV.
pub fn write_cue(
    path: &Path,
    audio_file: &str,
    title: &str,
    regions: &[Region],
    sample_rate: u32,
) -> Result<()> {
    let mut cue = String::new();
    cue.push_str("REM COMMENT \"Kerf\"\n");
    cue.push_str(&format!("TITLE \"{}\"\n", cue_quote(title)));
    cue.push_str(&format!("FILE \"{}\" WAVE\n", cue_quote(audio_file)));
    for (i, r) in regions.iter().enumerate() {
        cue.push_str(&format!("  TRACK {:02} AUDIO\n", i + 1));
        cue.push_str(&format!("    TITLE \"{}\"\n", cue_quote(&r.name)));
        cue.push_str(&format!(
            "    INDEX 01 {}\n",
            cue_time(r.start, sample_rate)
        ));
    }
    std::fs::write(path, cue)?;
    Ok(())
}

/// Export the whole mix as one WAV plus a CUE sheet with a track at every
/// marker. Returns both paths.
pub fn export_with_cue(
    project: &Project,
    wav_path: &Path,
    title: &str,
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let end = project.length();
    export_range(
        project,
        0,
        end,
        &RenderOptions::default(),
        wav_path,
        settings,
        progress,
    )?;
    let cue_path = wav_path.with_extension("cue");
    let file_name = wav_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    write_cue(
        &cue_path,
        &file_name,
        title,
        &regions_from_markers(project, end),
        project.sample_rate,
    )?;
    Ok(vec![wav_path.to_path_buf(), cue_path])
}

/// Export every audible track (post-fader, solo ignored) as its own file.
pub fn export_stems(
    project: &Project,
    start: Frame,
    end: Frame,
    dir: &Path,
    settings: ExportSettings,
    progress: Progress,
) -> Result<Vec<PathBuf>> {
    let tracks: Vec<_> = project.tracks.iter().filter(|t| !t.muted).collect();
    let mut paths = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let path = dir.join(format!(
            "{}.wav",
            sanitize_file_name(&format!("{:02} - {}", i + 1, t.name))
        ));
        let n = tracks.len() as f64;
        export_range(
            project,
            start,
            end,
            &RenderOptions::tracks(vec![t.id]),
            &path,
            settings,
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
                    name: "Intro".into()
                },
                Region {
                    start: 40,
                    end: 110,
                    name: "Song".into()
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
            },
            Region {
                start: 48_000 * 90,
                end: 48_000 * 200,
                name: "Say \"Hi\"".into(),
            },
        ];
        write_cue(&path, "show.wav", "Live", &regions, 48_000).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("FILE \"show.wav\" WAVE"));
        assert!(text.contains("  TRACK 02 AUDIO\n    TITLE \"Say 'Hi'\"\n    INDEX 01 01:30:00"));
    }
}
