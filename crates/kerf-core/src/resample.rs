//! High-quality offline sample-rate conversion (FFT-based, via `rubato`).

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Indexing, Resampler};

use crate::buffer::AudioBuffer;

/// Receives converted audio: planar channel slices of equal length.
pub type Sink<'a> = dyn FnMut(&[&[f32]]) -> Result<()> + 'a;
use crate::error::{Error, Result};

/// Convert `buf` to `target_rate`. Returns a clone if the rate already matches.
pub fn resample(buf: &AudioBuffer, target_rate: u32) -> Result<AudioBuffer> {
    if buf.sample_rate() == target_rate {
        return Ok(buf.clone());
    }
    let channels = buf.num_channels();
    let frames = buf.frames();
    if frames == 0 {
        return Ok(AudioBuffer::silent(target_rate, channels, 0));
    }
    let err = |e: &dyn std::fmt::Display| Error::Resample(e.to_string());
    let mut resampler = Fft::<f32>::new(
        buf.sample_rate() as usize,
        target_rate as usize,
        1024,
        channels,
        FixedSync::Input,
    )
    .map_err(|e| err(&e))?;
    let input =
        SequentialSliceOfVecs::new(buf.channels(), channels, frames).map_err(|e| err(&e))?;
    let output = resampler
        .process_all(&input, frames, None)
        .map_err(|e| err(&e))?;
    // rubato rounds the length up via a float ratio; use the exact rational length
    // so clip positions stay consistent across save/load.
    let (sr_in, sr_out) = (buf.sample_rate() as u128, target_rate as u128);
    let exact = ((frames as u128 * sr_out + sr_in / 2) / sr_in) as usize;
    let mut data = output.take_data();
    data.resize(exact * channels, 0.0);
    Ok(AudioBuffer::from_interleaved(target_rate, channels, &data))
}

/// Exact output length for `frames` input frames.
fn exact_len(frames: u64, from: u32, to: u32) -> u64 {
    ((frames as u128 * to as u128 + from as u128 / 2) / from as u128) as u64
}

/// Chunk-by-chunk sample-rate conversion with the same result length as
/// [`resample`]: output is held back until the exact final length is known.
pub struct StreamResampler {
    inner: Option<Fft<f32>>,
    channels: usize,
    from: u32,
    to: u32,
    pending: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    held: Vec<Vec<f32>>,
    delay_left: usize,
    in_total: u64,
    out_emitted: u64,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Result<Self> {
        let inner = if from == to {
            None
        } else {
            Some(
                Fft::<f32>::new(from as usize, to as usize, 1024, channels, FixedSync::Input)
                    .map_err(|e| Error::Resample(e.to_string()))?,
            )
        };
        let out_max = inner.as_ref().map_or(0, |r| r.output_frames_max());
        let delay_left = inner.as_ref().map_or(0, |r| r.output_delay());
        Ok(Self {
            inner,
            channels,
            from,
            to,
            pending: vec![Vec::new(); channels],
            out: vec![vec![0.0; out_max]; channels],
            held: vec![Vec::new(); channels],
            delay_left,
            in_total: 0,
            out_emitted: 0,
        })
    }

    pub fn push<C: AsRef<[f32]>>(
        &mut self,
        chunk: &[C],
        frames: usize,
        sink: &mut Sink,
    ) -> Result<()> {
        self.in_total += frames as u64;
        if self.inner.is_none() {
            let slices: Vec<&[f32]> = chunk
                .iter()
                .take(self.channels)
                .map(|c| &c.as_ref()[..frames])
                .collect();
            self.out_emitted += frames as u64;
            return sink(&slices);
        }
        for (p, c) in self.pending.iter_mut().zip(chunk) {
            p.extend_from_slice(&c.as_ref()[..frames]);
        }
        loop {
            let need = self.inner.as_ref().unwrap().input_frames_next();
            if self.pending[0].len() < need {
                break;
            }
            self.process(need, None)?;
            for p in &mut self.pending {
                p.drain(..need);
            }
        }
        // Never emit past what the input so far can justify.
        let limit = (self.in_total as u128 * self.to as u128 / self.from as u128) as u64;
        self.emit(limit, sink)
    }

    pub fn finish(mut self, sink: &mut Sink) -> Result<()> {
        let exact = exact_len(self.in_total, self.from, self.to);
        if self.inner.is_some() {
            let need = self.inner.as_ref().unwrap().input_frames_next();
            let partial = self.pending[0].len();
            for p in &mut self.pending {
                p.resize(need, 0.0);
            }
            self.process(need, Some(partial))?;
            for p in &mut self.pending {
                p.fill(0.0);
            }
            // Pump silence until the tail (and the startup delay) is flushed.
            let mut guard = 0;
            while self.out_emitted + (self.held[0].len() as u64) < exact && guard < 64 {
                let need = self.inner.as_ref().unwrap().input_frames_next();
                for p in &mut self.pending {
                    p.resize(need, 0.0);
                }
                self.process(need, Some(0))?;
                guard += 1;
            }
        }
        self.emit(exact, sink)?;
        if self.out_emitted < exact {
            let pad = vec![0.0f32; (exact - self.out_emitted) as usize];
            let slices: Vec<&[f32]> = (0..self.channels).map(|_| &pad[..]).collect();
            sink(&slices)?;
        }
        Ok(())
    }

    fn process(&mut self, need: usize, partial: Option<usize>) -> Result<()> {
        let r = self.inner.as_mut().unwrap();
        let err = |e: &dyn std::fmt::Display| Error::Resample(e.to_string());
        let input =
            SequentialSliceOfVecs::new(&self.pending, self.channels, need).map_err(|e| err(&e))?;
        let cap = self.out[0].len();
        let mut output = SequentialSliceOfVecs::new_mut(&mut self.out, self.channels, cap)
            .map_err(|e| err(&e))?;
        let mut indexing = Indexing::new();
        if let Some(n) = partial {
            indexing = indexing.partial_len(n);
        }
        let (_, produced) = r
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| err(&e))?;
        let skip = self.delay_left.min(produced);
        self.delay_left -= skip;
        for (h, o) in self.held.iter_mut().zip(&self.out) {
            h.extend_from_slice(&o[skip..produced]);
        }
        Ok(())
    }

    /// Emit held output up to a total of `limit` frames.
    fn emit(&mut self, limit: u64, sink: &mut Sink) -> Result<()> {
        let n = (self.held[0].len() as u64).min(limit.saturating_sub(self.out_emitted)) as usize;
        if n == 0 {
            return Ok(());
        }
        let slices: Vec<&[f32]> = self.held.iter().map(|h| &h[..n]).collect();
        sink(&slices)?;
        self.out_emitted += n as u64;
        for h in &mut self.held {
            h.drain(..n);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_content_preserved() {
        let sr_in = 44_100;
        let n = sr_in as usize; // 1 s
        let sine: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr_in as f32).sin() * 0.5)
            .collect();
        let buf = AudioBuffer::new(sr_in, vec![sine.clone(), sine]);
        let out = resample(&buf, 48_000).unwrap();
        assert_eq!(out.sample_rate(), 48_000);
        assert_eq!(out.frames(), 48_000);
        // Compare mid-file against an ideal 1 kHz sine at 48 kHz (no delay after trimming).
        for i in 10_000..10_100 {
            let ideal = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin() * 0.5;
            assert!(
                (out.channel(0)[i] - ideal).abs() < 0.01,
                "frame {i}: {} vs {ideal}",
                out.channel(0)[i]
            );
        }
    }

    #[test]
    fn streaming_matches_whole_buffer() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (48_000, 48_000),
        ] {
            let n = 30_011usize;
            let sig: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.03).sin() * 0.7).collect();
            let buf = AudioBuffer::new(
                from,
                vec![sig.clone(), sig.iter().map(|v| v * -0.5).collect()],
            );
            let whole = resample(&buf, to).unwrap();

            let mut out: Vec<Vec<f32>> = vec![Vec::new(); 2];
            let mut sink = |c: &[&[f32]]| {
                for (o, s) in out.iter_mut().zip(c) {
                    o.extend_from_slice(s);
                }
                Ok(())
            };
            let mut rs = StreamResampler::new(from, to, 2).unwrap();
            let mut pos = 0;
            while pos < n {
                let k = 1777.min(n - pos);
                let chunk: Vec<&[f32]> = buf.channels().iter().map(|c| &c[pos..pos + k]).collect();
                rs.push(&chunk, k, &mut sink).unwrap();
                pos += k;
            }
            rs.finish(&mut sink).unwrap();
            assert_eq!(out[0].len(), whole.frames(), "{from}->{to}");
            let max_err = out[0]
                .iter()
                .zip(whole.channel(0))
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            assert!(max_err < 1e-4, "{from}->{to}: {max_err}");
        }
    }
}
