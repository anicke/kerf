//! Spectrogram pictures: frequency over time for a range of the mix.
//!
//! Made for AI assistants, which cannot listen and can't tell speech from
//! music in a waveform: in a spectrogram, speech shows as stacks of
//! harmonics that come and go with the syllables over a dark background,
//! crowd noise as an even haze, and music as dense, sustained bands. One
//! picture of the mono mix (time left to right, frequency bottom to top, on a
//! log scale by default), brighter for louder, scaled to the loudest point in
//! view; markers as yellow lines, a dotted time grid and dotted lines at 100
//! Hz, 1 kHz and 10 kHz.

use realfft::RealFftPlanner;

use crate::error::{Error, Result};
use crate::project::{Project, TrackId};
use crate::render::{self, Progress, RenderOptions};
use crate::waveform::{grid_step, Canvas, Rgb, MARKER};
use crate::Frame;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub width: u32,
    pub height: u32,
    /// Highest frequency shown (capped at half the sample rate).
    pub max_hz: f64,
    /// Frequency on a log scale (default); linear otherwise.
    pub log_freq: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 300,
            max_hz: 16_000.0,
            log_freq: true,
        }
    }
}

/// Lowest frequency on a log scale.
pub const MIN_LOG_HZ: f64 = 40.0;
/// Levels shown, below the loudest point in view.
pub const RANGE_DB: f64 = 80.0;

pub struct Image {
    pub png: Vec<u8>,
    /// Spacing of the time grid, in seconds.
    pub grid_secs: f64,
    /// Highest frequency shown.
    pub max_hz: f64,
    /// Lowest frequency shown.
    pub min_hz: f64,
    /// Level (dBFS of the FFT bin power) of the brightest colour; black is
    /// [`RANGE_DB`] below.
    pub top_db: f64,
    /// Row of each frequency line drawn (100 Hz, 1 kHz, 10 kHz in view).
    pub freq_lines: Vec<(f64, u32)>,
}

const GRID: Rgb = [110, 110, 120];

/// Dark → purple → red → orange → pale yellow.
fn colour(v: f32) -> Rgb {
    const STOPS: [(f32, Rgb); 5] = [
        (0.0, [0, 0, 4]),
        (0.3, [60, 15, 110]),
        (0.55, [190, 40, 70]),
        (0.8, [250, 150, 30]),
        (1.0, [255, 250, 200]),
    ];
    let v = v.clamp(0.0, 1.0);
    let i = STOPS.iter().position(|(s, _)| *s >= v).unwrap_or(4).max(1);
    let ((a, ca), (b, cb)) = (STOPS[i - 1], STOPS[i]);
    let t = if b > a { (v - a) / (b - a) } else { 0.0 };
    std::array::from_fn(|k| (ca[k] as f32 + (cb[k] as f32 - ca[k] as f32) * t).round() as u8)
}

/// Frequency at height `y` (0 at the top edge, `h` at the bottom).
fn edge_hz(y: f64, h: usize, min_hz: f64, max_hz: f64, log: bool) -> f64 {
    let t = 1.0 - y / h as f64;
    if log {
        min_hz * (max_hz / min_hz).powf(t)
    } else {
        min_hz + (max_hz - min_hz) * t
    }
}

/// Row showing `hz`.
fn hz_row(hz: f64, h: usize, min_hz: f64, max_hz: f64, log: bool) -> usize {
    let t = if log {
        (hz / min_hz).ln() / (max_hz / min_hz).ln()
    } else {
        (hz - min_hz) / (max_hz - min_hz)
    };
    (((1.0 - t) * h as f64) as isize).clamp(0, h as isize - 1) as usize
}

/// FFT length giving about 23 Hz per bin (2048 at 44.1 and 48 kHz).
fn fft_len(sample_rate: u32) -> usize {
    (sample_rate as usize / 23)
        .next_power_of_two()
        .clamp(256, 16_384)
}

/// Render `start..end` of the given tracks' mix as a spectrogram PNG.
pub fn render_png(
    p: &Project,
    tracks: &[TrackId],
    start: Frame,
    end: Frame,
    o: &Options,
    progress: Progress,
) -> Result<Image> {
    if end <= start {
        return Err(Error::InvalidRange);
    }
    let (w, h) = (o.width.max(1) as usize, o.height.max(2) as usize);
    let sr = p.sample_rate.max(1);
    let nyquist = sr as f64 / 2.0;
    let max_hz = o.max_hz.clamp(1000.0, nyquist);
    let min_hz = if o.log_freq { MIN_LOG_HZ } else { 0.0 };
    let n = fft_len(sr);
    let len = end - start;
    let col_len = len as f64 / w as f64;
    // Several FFTs per column on long ranges (their powers are averaged), at
    // least one per column when zoomed in.
    let hop = (col_len as usize).clamp(32, n / 2);
    let bin_hz = sr as f64 / n as f64;
    let bins = n / 2 + 1;

    // Read half a window either side so the edges are centred like the rest.
    let read_start = start.saturating_sub(n as Frame / 2);
    let read_end = end + n as Frame / 2;
    let window: Vec<f32> = (0..n)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / n as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();

    let mut power = vec![vec![0f64; bins]; w];
    let mut count = vec![0u32; w];
    let mut buf: Vec<f32> = Vec::with_capacity(n * 2);
    let mut buf_start = read_start; // timeline frame of buf[0]
    let channels = p.channels.max(1) as usize;
    let mut pos = read_start;
    let total = (read_end - read_start) as f64;
    render::for_each_chunk(
        p,
        read_start,
        read_end,
        &RenderOptions::tracks(tracks.to_vec()),
        |chunk, frames| {
            for i in 0..frames {
                let s: f32 = chunk.iter().take(channels).map(|c| c[i]).sum();
                buf.push(s / channels as f32);
            }
            pos += frames as Frame;
            while buf.len() >= n {
                let centre = buf_start + n as Frame / 2;
                if centre >= start && centre < end {
                    for (k, x) in input.iter_mut().enumerate() {
                        *x = buf[k] * window[k];
                    }
                    if fft
                        .process_with_scratch(&mut input, &mut spectrum, &mut scratch)
                        .is_ok()
                    {
                        let col = (((centre - start) as f64 / col_len) as usize).min(w - 1);
                        for (acc, c) in power[col].iter_mut().zip(&spectrum) {
                            *acc += c.norm_sqr() as f64;
                        }
                        count[col] += 1;
                    }
                }
                buf.drain(..hop);
                buf_start += hop as Frame;
            }
            Ok(progress((pos - read_start) as f64 / total))
        },
    )?;

    // Average, then fill columns no FFT landed in from their neighbours.
    for (col, c) in power.iter_mut().zip(&count) {
        if *c > 0 {
            col.iter_mut().for_each(|v| *v /= *c as f64);
        }
    }
    let first = count.iter().position(|&c| c > 0);
    if let Some(first) = first {
        for x in 0..w {
            if count[x] == 0 {
                let src = if x < first { first } else { x - 1 };
                power[x] = power[src].clone();
                count[x] = 1;
            }
        }
    }

    // Each row shows the strongest bin in its frequency band.
    // A full-scale sine peaks at n/4 through a Hann window: 0 dB.
    let norm = (n as f64 / 4.0).powi(2);
    let bands: Vec<(usize, usize)> = (0..h)
        .map(|y| {
            let edge = |y: f64| edge_hz(y, h, min_hz, max_hz, o.log_freq);
            let (hi, lo) = (edge(y as f64), edge(y as f64 + 1.0));
            let k0 = ((lo / bin_hz).round() as usize).min(bins - 1);
            let k1 = ((hi / bin_hz).round() as usize).clamp(k0, bins - 1);
            (k0, k1)
        })
        .collect();
    let mut db = vec![f64::NEG_INFINITY; w * h];
    for (x, col) in power.iter().enumerate() {
        for (y, &(k0, k1)) in bands.iter().enumerate() {
            let p = col[k0..=k1].iter().fold(0f64, |a, &b| a.max(b)) / norm;
            if p > 0.0 {
                db[y * w + x] = 10.0 * p.log10();
            }
        }
    }
    let top_db = db
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    let top_db = if top_db.is_finite() { top_db } else { 0.0 };

    let mut cv = Canvas::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = (db[y * w + x] - (top_db - RANGE_DB)) / RANGE_DB;
            cv.set(x, y, colour(v as f32));
        }
    }

    // Dotted time grid and frequency lines, so they don't hide what's under them.
    let secs_per_px = len as f64 / sr as f64 / w as f64;
    let grid_secs = grid_step(secs_per_px);
    let start_secs = start as f64 / sr as f64;
    let mut t = (start_secs / grid_secs).ceil() * grid_secs;
    while t < end as f64 / sr as f64 {
        let x = ((t - start_secs) / secs_per_px) as usize;
        for y in (0..h).step_by(4) {
            cv.set(x, y, GRID);
        }
        t += grid_secs;
    }
    let mut freq_lines = Vec::new();
    for hz in [100.0, 1000.0, 10_000.0] {
        if hz > min_hz && hz < max_hz {
            let y = hz_row(hz, h, min_hz, max_hz, o.log_freq);
            for x in (0..w).step_by(4) {
                cv.set(x, y, GRID);
            }
            freq_lines.push((hz, y as u32));
        }
    }
    for m in p
        .markers
        .iter()
        .filter(|m| m.position >= start && m.position < end)
    {
        let x = ((m.position - start) as u128 * w as u128 / len as u128) as usize;
        cv.vline(x, 0, h - 1, MARKER);
        for dx in 1..4 {
            cv.vline(x + dx, 0, 5 - dx, MARKER);
        }
    }

    Ok(Image {
        png: cv.png()?,
        grid_secs,
        max_hz,
        min_hz,
        top_db,
        freq_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioBuffer;

    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    fn brightness(buf: &[u8], w: u32, x: u32, y: u32) -> u32 {
        let i = ((y * w + x) * 3) as usize;
        buf[i] as u32 + buf[i + 1] as u32 + buf[i + 2] as u32
    }

    /// The brightest row of column `x`.
    fn peak_row(buf: &[u8], w: u32, h: u32, x: u32) -> u32 {
        (0..h).max_by_key(|&y| brightness(buf, w, x, y)).unwrap()
    }

    #[test]
    fn tones_show_at_their_frequency() {
        // 2 s of 1 kHz, then 2 s of 4 kHz, stereo at 48 kHz.
        let sr = 48_000;
        let tone =
            |hz: f32, i: usize| (i as f32 * hz * std::f32::consts::TAU / sr as f32).sin() * 0.5;
        let data: Vec<f32> = (0..4 * sr as usize)
            .map(|i| tone(if i < 2 * sr as usize { 1000.0 } else { 4000.0 }, i))
            .collect();
        let mut p = Project::new(sr, 2);
        p.add_source_as_track("t", None, AudioBuffer::new(sr, vec![data.clone(), data]), 0);
        p.add_marker(2 * sr as Frame, "Change");
        let o = Options {
            width: 400,
            height: 200,
            ..Default::default()
        };
        let img = render_png(&p, &p.all_track_ids(), 0, 4 * sr as Frame, &o, &mut |_| {
            true
        })
        .unwrap();
        let (w, h, buf) = decode(&img.png);
        assert_eq!((w, h), (400, 200));
        assert_eq!(img.max_hz, 16_000.0);
        assert!(
            (img.top_db - -6.0).abs() < 1.5,
            "a half-scale sine: {}",
            img.top_db
        );

        let row = |hz| hz_row(hz, 200, MIN_LOG_HZ, 16_000.0, true) as i64;
        let near = |got: u32, hz| (got as i64 - row(hz)).abs() <= 2;
        assert!(
            near(peak_row(&buf, w, h, 100), 1000.0),
            "1 kHz in the first half"
        );
        assert!(
            near(peak_row(&buf, w, h, 300), 4000.0),
            "4 kHz in the second half"
        );
        assert!(img
            .freq_lines
            .iter()
            .any(|&(hz, y)| hz == 1000.0 && y as i64 == row(1000.0)));
        // Far from the tone it's dark.
        assert!(brightness(&buf, w, 100, row(200.0) as u32) < 60);
        // The marker at 2 s is column 200.
        let i = ((10 * w + 200) * 3) as usize;
        assert_eq!(&buf[i..i + 3], &MARKER);

        // Linear scale up to 8 kHz: 4 kHz is half-way up.
        let o = Options {
            log_freq: false,
            max_hz: 8000.0,
            ..o
        };
        let img = render_png(&p, &p.all_track_ids(), 0, 4 * sr as Frame, &o, &mut |_| {
            true
        })
        .unwrap();
        let (_, _, buf) = decode(&img.png);
        assert!((peak_row(&buf, w, h, 300) as i64 - 100).abs() <= 2);
    }

    #[test]
    fn zoomed_in_and_silent_ranges() {
        let sr = 44_100;
        let data: Vec<f32> = (0..sr as usize)
            .map(|i| (i as f32 * 0.3).sin() * 0.1)
            .collect();
        let mut p = Project::new(sr, 1);
        p.add_source_as_track("t", None, AudioBuffer::new(sr, vec![data]), 0);
        let o = Options::default();
        // 10 ms over 1200 columns: every column is filled.
        let img = render_png(&p, &p.all_track_ids(), 20_000, 20_441, &o, &mut |_| true).unwrap();
        let (w, h, buf) = decode(&img.png);
        assert!((0..w).all(|x| (0..h).any(|y| brightness(&buf, w, x, y) > 200)));
        // Well past the end of the audio: silence renders black, without errors.
        let (a, b) = (2 * sr as Frame, 3 * sr as Frame);
        let img = render_png(&p, &p.all_track_ids(), a, b, &o, &mut |_| true).unwrap();
        assert_eq!(img.top_db, 0.0);
    }

    #[test]
    fn rows_and_frequencies_agree() {
        for log in [true, false] {
            let (lo, hi) = if log {
                (MIN_LOG_HZ, 16_000.0)
            } else {
                (0.0, 8000.0)
            };
            for y in [0, 50, 199] {
                let hz = edge_hz(y as f64 + 0.5, 200, lo, hi, log);
                assert_eq!(hz_row(hz, 200, lo, hi, log), y);
            }
        }
    }
}
