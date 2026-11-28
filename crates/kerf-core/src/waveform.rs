//! Waveform pictures: a PNG of a range of the mix, as heard.
//!
//! Made for reports and for AI assistants, which cannot listen: one lane per
//! channel showing peaks and RMS per pixel column, clipping in red, markers
//! as yellow lines and a faint time grid. There is no text in the image; the
//! caller reports the time scale alongside it.

use crate::error::{Error, Result};
use crate::project::{Project, TrackId};
use crate::render::{self, Progress, RenderOptions};
use crate::Frame;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub width: u32,
    pub height: u32,
    /// 60 dB logarithmic amplitude scale.
    pub db_scale: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 300,
            db_scale: false,
        }
    }
}

pub struct Image {
    pub png: Vec<u8>,
    /// Spacing of the vertical grid lines, in seconds.
    pub grid_secs: f64,
}

pub(crate) type Rgb = [u8; 3];
const BACKGROUND: Rgb = [22, 24, 28];
const GRID: Rgb = [44, 47, 54];
const CENTER: Rgb = [58, 60, 68];
const SEPARATOR: Rgb = [80, 82, 90];
const PEAK: Rgb = [74, 144, 226];
const RMS: Rgb = [150, 196, 245];
const CLIP: Rgb = [235, 72, 64];
pub(crate) const MARKER: Rgb = [242, 200, 60];

#[derive(Clone, Copy)]
struct Column {
    min: f32,
    max: f32,
    sum_sq: f64,
    n: u32,
    clipped: bool,
}

impl Default for Column {
    fn default() -> Self {
        Self {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sum_sq: 0.0,
            n: 0,
            clipped: false,
        }
    }
}

/// A grid step giving lines at least ~80 px apart.
pub(crate) fn grid_step(secs_per_px: f64) -> f64 {
    const STEPS: [f64; 22] = [
        0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0,
        60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0,
    ];
    STEPS
        .into_iter()
        .find(|s| s / secs_per_px >= 80.0)
        .unwrap_or(7200.0)
}

/// Display position (0..1) of an amplitude, linear or on a 60 dB scale.
fn amp(v: f32, db: bool) -> f32 {
    let a = v.abs();
    if db {
        if a <= 1e-6 {
            0.0
        } else {
            (1.0 + 20.0 * a.log10() / 60.0).clamp(0.0, 1.0)
        }
    } else {
        a.min(1.0)
    }
}

pub(crate) struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(w: usize, h: usize) -> Self {
        let mut px = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            px.extend_from_slice(&BACKGROUND);
        }
        Self { w, h, px }
    }

    pub(crate) fn set(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.w && y < self.h {
            let i = (y * self.w + x) * 3;
            self.px[i..i + 3].copy_from_slice(&c);
        }
    }

    pub(crate) fn vline(&mut self, x: usize, y0: usize, y1: usize, c: Rgb) {
        for y in y0..=y1.min(self.h.saturating_sub(1)) {
            self.set(x, y, c);
        }
    }

    fn hline(&mut self, y: usize, c: Rgb) {
        for x in 0..self.w {
            self.set(x, y, c);
        }
    }

    pub(crate) fn png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.w as u32, self.h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let encode = |e: png::EncodingError| Error::Encode(e.to_string());
        let mut writer = enc.write_header().map_err(encode)?;
        writer.write_image_data(&self.px).map_err(encode)?;
        writer.finish().map_err(encode)?;
        Ok(out)
    }
}

/// Render `start..end` of the given tracks' mix as a PNG.
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
    let (w, h) = (o.width.max(1) as usize, o.height.max(1) as usize);
    let ch = (p.channels as usize).max(1);
    let len = end - start;

    // --- Measure each pixel column ---------------------------------------
    let mut cols = vec![vec![Column::default(); w]; ch];
    let col_end = |c: usize| start + ((c as u128 + 1) * len as u128 / w as u128) as Frame;
    let mut col = 0usize;
    let mut next = col_end(0);
    let mut pos = start;
    render::for_each_chunk(
        p,
        start,
        end,
        &RenderOptions::tracks(tracks.to_vec()),
        |chunk, n| {
            for i in 0..n {
                while pos >= next && col + 1 < w {
                    col += 1;
                    next = col_end(col);
                }
                for (c, lane) in cols.iter_mut().enumerate() {
                    let v = chunk.get(c).map_or(0.0, |s| s[i]);
                    let k = &mut lane[col];
                    k.min = k.min.min(v);
                    k.max = k.max.max(v);
                    k.sum_sq += (v as f64) * (v as f64);
                    k.n += 1;
                    k.clipped |= v.abs() > 1.0;
                }
                pos += 1;
            }
            Ok(progress((pos - start) as f64 / len as f64))
        },
    )?;
    // Zoomed in past one frame per column: repeat the previous column.
    for lane in &mut cols {
        for x in 1..w {
            if lane[x].n == 0 {
                lane[x] = lane[x - 1];
            }
        }
    }

    // --- Draw ----------------------------------------------------------------
    let mut cv = Canvas::new(w, h);
    let secs_per_px = len as f64 / p.sample_rate.max(1) as f64 / w as f64;
    let grid_secs = grid_step(secs_per_px);
    let sr = p.sample_rate.max(1) as f64;
    let start_secs = start as f64 / sr;
    let mut t = (start_secs / grid_secs).ceil() * grid_secs;
    while t < end as f64 / sr {
        let x = ((t - start_secs) / secs_per_px) as usize;
        cv.vline(x, 0, h - 1, GRID);
        t += grid_secs;
    }

    let lane_h = h / ch;
    for (c, lane) in cols.iter().enumerate() {
        let top = c * lane_h;
        let half = (lane_h as f32 / 2.0 - 1.0).max(1.0);
        let mid = top as f32 + lane_h as f32 / 2.0;
        cv.hline(mid as usize, CENTER);
        if c > 0 {
            cv.hline(top, SEPARATOR);
        }
        let y = |v: f32| {
            let a = amp(v, o.db_scale).copysign(v);
            (mid - a * half)
                .round()
                .clamp(top as f32, (top + lane_h - 1) as f32) as usize
        };
        for (x, k) in lane.iter().enumerate() {
            if k.n == 0 {
                continue;
            }
            cv.vline(x, y(k.max), y(k.min), if k.clipped { CLIP } else { PEAK });
            let rms = (k.sum_sq / k.n as f64).sqrt() as f32;
            let (r0, r1) = (y(rms), y(-rms));
            if r1 > r0 && !k.clipped {
                cv.vline(x, r0, r1, RMS);
            }
        }
    }

    for m in p
        .markers
        .iter()
        .filter(|m| m.position >= start && m.position < end)
    {
        let x = ((m.position - start) as u128 * w as u128 / len as u128) as usize;
        cv.vline(x, 0, h - 1, MARKER);
        // A small flag at the top so markers stand out from the waveform.
        for dx in 1..4 {
            cv.vline(x + dx, 0, 5 - dx, MARKER);
        }
    }

    Ok(Image {
        png: cv.png()?,
        grid_secs,
    })
}

/// Pixel column of `frame` in an image of `start..end` that is `width` wide.
pub fn column_of(frame: Frame, start: Frame, end: Frame, width: u32) -> u32 {
    let len = (end - start).max(1);
    ((frame.saturating_sub(start)) as u128 * width as u128 / len as u128) as u32
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

    fn pixel(buf: &[u8], w: u32, x: u32, y: u32) -> Rgb {
        let i = ((y * w + x) * 3) as usize;
        [buf[i], buf[i + 1], buf[i + 2]]
    }

    #[test]
    fn draws_loud_quiet_clipped_and_markers() {
        // 1 s silence, 1 s at half scale, 1 s clipping (1.5), mono, 1 kHz rate.
        let mut data = vec![0.0f32; 1000];
        data.extend((0..1000).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }));
        data.extend((0..1000).map(|i| if i % 2 == 0 { 1.5 } else { -1.5 }));
        let mut p = Project::new(1000, 1);
        p.add_source_as_track("t", None, AudioBuffer::new(1000, vec![data]), 0);
        p.add_marker(1500, "M");
        let o = Options {
            width: 300,
            height: 100,
            db_scale: false,
        };
        let img = render_png(&p, &p.all_track_ids(), 0, 3000, &o, &mut |_| true).unwrap();
        let (w, h, buf) = decode(&img.png);
        assert_eq!((w, h), (300, 100));
        assert_eq!(img.grid_secs, 1.0);

        // Silence: only the centre line. Half scale: reaches about a quarter down.
        assert_eq!(pixel(&buf, w, 50, 20), BACKGROUND);
        assert_ne!(pixel(&buf, w, 150, 30), BACKGROUND);
        assert_eq!(pixel(&buf, w, 160, 20), BACKGROUND);
        // Clipping is red, full height.
        assert_eq!(pixel(&buf, w, 250, 2), CLIP);
        // The marker at 1.5 s is column 150.
        assert_eq!(pixel(&buf, w, 150, 90), MARKER);
        assert_eq!(column_of(1500, 0, 3000, 300), 150);

        // The dB scale shows the half-scale part much taller (-6 dB of 60).
        let o = Options {
            db_scale: true,
            ..o
        };
        let img = render_png(&p, &p.all_track_ids(), 0, 3000, &o, &mut |_| true).unwrap();
        let (_, _, buf) = decode(&img.png);
        assert_ne!(pixel(&buf, w, 160, 10), BACKGROUND);
    }

    #[test]
    fn zoomed_in_past_single_samples() {
        let mut p = Project::new(1000, 2);
        p.add_source_as_track(
            "t",
            None,
            AudioBuffer::new(1000, vec![vec![0.5; 10], vec![-0.5; 10]]),
            0,
        );
        let o = Options {
            width: 400,
            height: 100,
            db_scale: false,
        };
        let img = render_png(&p, &p.all_track_ids(), 0, 10, &o, &mut |_| true).unwrap();
        let (_, _, buf) = decode(&img.png);
        // Every column is filled; two lanes (stereo).
        assert_ne!(pixel(&buf, 400, 399, 20), BACKGROUND);
        assert_ne!(pixel(&buf, 400, 399, 80), BACKGROUND);
    }
}
