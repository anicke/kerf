//! Album-level analysis and loudness for live recordings and other albums.
//!
//! Albums should be levelled as a whole: one gain for the entire album, so a
//! quiet ballad stays quieter than the loud song after it, exactly as on
//! stage. Normalizing each track separately (a common mistake) flattens that.

use crate::analysis::Meter;
use crate::error::{Error, Result};
use crate::project::Project;
use crate::render::{self, Progress, Region, RenderOptions};
use crate::util::{db_to_gain, gain_to_db};

/// Loudness and peak of one track (as heard, including the master gain).
#[derive(Clone, Debug, PartialEq)]
pub struct TrackLevel {
    /// Integrated loudness (LUFS); `None` if too short or silent.
    pub lufs: Option<f64>,
    /// Sample peak in dBFS.
    pub peak_db: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlbumAnalysis {
    pub tracks: Vec<TrackLevel>,
    /// Integrated loudness of the whole album.
    pub lufs: Option<f64>,
    pub peak_db: f64,
}

/// Measure every region and the album as a whole, in a single pass.
/// Regions must be contiguous (as from [`render::regions_from_markers`]).
pub fn analyze(p: &Project, regions: &[Region], progress: Progress) -> Result<AlbumAnalysis> {
    let (Some(first), Some(last)) = (regions.first(), regions.last()) else {
        return Err(Error::NothingSelected);
    };
    let ch = p.channels as usize;
    let mut album = Meter::new(p.sample_rate, ch);
    let mut meters: Vec<Meter> = regions
        .iter()
        .map(|_| Meter::new(p.sample_rate, ch))
        .collect();
    let total = (last.end - first.start) as f64;
    let mut pos = first.start;
    let mut current = 0;
    render::for_each_chunk(
        p,
        first.start,
        last.end,
        &RenderOptions::default(),
        |chunk, n| {
            album.push(chunk, n);
            // Split the chunk across region boundaries.
            let mut off = 0;
            while off < n && current < regions.len() {
                let take = ((regions[current].end - pos) as usize).min(n - off);
                let part: Vec<&[f32]> =
                    chunk.iter().take(ch).map(|c| &c[off..off + take]).collect();
                meters[current].push(&part, take);
                off += take;
                pos += take as u64;
                if pos >= regions[current].end {
                    current += 1;
                }
            }
            Ok(progress((pos - first.start) as f64 / total))
        },
    )?;
    Ok(AlbumAnalysis {
        tracks: meters
            .iter()
            .map(|m| TrackLevel {
                lufs: m.integrated_lufs(),
                peak_db: m.peak_db(),
            })
            .collect(),
        lufs: album.integrated_lufs(),
        peak_db: album.peak_db(),
    })
}

/// The gain change (dB) that brings the album to `target_lufs`, limited so
/// the peak stays at or below `ceiling_db`. `None` if the album is silent.
pub fn album_gain_db(a: &AlbumAnalysis, target_lufs: f64, ceiling_db: Option<f64>) -> Option<f64> {
    let mut gain = target_lufs - a.lufs?;
    if let Some(c) = ceiling_db.filter(|_| a.peak_db.is_finite()) {
        gain = gain.min(c - a.peak_db);
    }
    Some(gain)
}

/// Change the master gain by `db` (undoable via `Document::apply`).
pub fn adjust_master_gain(p: &mut Project, db: f64) -> Result<()> {
    p.master_gain = (p.master_gain as f64 * db_to_gain(db)) as f32;
    Ok(())
}

/// Current master gain in dB.
pub fn master_gain_db(p: &Project) -> f64 {
    gain_to_db(p.master_gain as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::regions_from_markers;
    use crate::AudioBuffer;

    const SR: u32 = 48_000;

    /// A loud song (-12 dBFS sine) then a quiet one (-24 dBFS), 5 s each.
    fn album() -> Project {
        let mut p = Project::new(SR, 2);
        let tone = |amp: f32, freq: f32| -> Vec<f32> {
            (0..SR as usize * 5)
                .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin() * amp)
                .collect()
        };
        let mut l = tone(0.25, 440.0);
        l.extend(tone(0.063, 660.0));
        p.add_source_as_track("show", None, AudioBuffer::new(SR, vec![l.clone(), l]), 0);
        p.add_marker(0, "Loud");
        p.add_marker(SR as u64 * 5, "Quiet");
        p
    }

    #[test]
    fn per_track_and_album_levels() {
        let p = album();
        let regions = regions_from_markers(&p, p.length());
        let a = analyze(&p, &regions, &mut |_| true).unwrap();
        assert_eq!(a.tracks.len(), 2);
        let (loud, quiet) = (a.tracks[0].lufs.unwrap(), a.tracks[1].lufs.unwrap());
        assert!((loud - quiet - 12.0).abs() < 0.3, "{loud} vs {quiet}");
        assert!((a.tracks[0].peak_db - gain_to_db(0.25)).abs() < 0.01);
        assert!(a.lufs.unwrap() < loud && a.lufs.unwrap() > quiet);
    }

    #[test]
    fn album_gain_hits_target_and_keeps_balance() {
        let mut p = album();
        let regions = regions_from_markers(&p, p.length());
        let before = analyze(&p, &regions, &mut |_| true).unwrap();
        let gain = album_gain_db(&before, -16.0, None).unwrap();
        adjust_master_gain(&mut p, gain).unwrap();
        let after = analyze(&p, &regions, &mut |_| true).unwrap();
        assert!(
            (after.lufs.unwrap() - -16.0).abs() < 0.05,
            "{:?}",
            after.lufs
        );
        let diff = |a: &AlbumAnalysis| a.tracks[0].lufs.unwrap() - a.tracks[1].lufs.unwrap();
        assert!(
            (diff(&after) - diff(&before)).abs() < 0.01,
            "relative levels kept"
        );

        // A peak ceiling wins over a loud target.
        let g = album_gain_db(&after, 0.0, Some(-1.0)).unwrap();
        assert!((g - (-1.0 - after.peak_db)).abs() < 1e-9);
        assert!((master_gain_db(&p) - gain).abs() < 1e-4);
    }

    #[test]
    fn master_gain_affects_export_but_not_measurement_input() {
        let mut p = album();
        adjust_master_gain(&mut p, -6.0).unwrap();
        let heard = render::render(&p, 0, 10, &RenderOptions::default());
        let pre = render::render(&p, 0, 10, &RenderOptions::pre_fader(p.all_track_ids()));
        assert!((heard.channel(0)[5] - pre.channel(0)[5] * db_to_gain(-6.0) as f32).abs() < 1e-6);
    }
}
