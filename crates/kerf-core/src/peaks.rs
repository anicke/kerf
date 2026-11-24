//! Multi-resolution min/max peak cache for fast waveform drawing at any zoom.
//! Built incrementally while audio streams in, and saved next to cache files
//! so reopening a project does not need to rescan the audio.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::buffer::AudioBuffer;
use crate::error::Result;
use crate::store::Samples;

/// Frames per entry in the finest level. (64 keeps the cache at 1/32 of the
/// audio; drawing finer than that reads the visible samples directly.)
const BASE_BLOCK: usize = 64;
/// Each coarser level summarises this many entries of the previous one.
const FACTOR: usize = 4;
const MAGIC: &[u8; 8] = b"KERFPK02";

const EMPTY: [f32; 2] = [f32::INFINITY, f32::NEG_INFINITY];

struct Level {
    block: usize,
    /// Per channel, `[min, max]` per block.
    channels: Vec<Vec<[f32; 2]>>,
}

/// Precomputed min/max pyramid (about 1/24 the size of the audio, all levels).
pub struct PeakCache {
    levels: Vec<Level>,
}

/// Builds the finest level incrementally; coarser levels are derived on finish.
pub struct PeakBuilder {
    base: Vec<Vec<[f32; 2]>>,
    current: Vec<[f32; 2]>,
    filled: usize,
}

impl PeakBuilder {
    pub fn new(channels: usize) -> Self {
        Self {
            base: vec![Vec::new(); channels],
            current: vec![EMPTY; channels],
            filled: 0,
        }
    }

    pub fn push<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) {
        let mut i = 0;
        while i < frames {
            let n = (BASE_BLOCK - self.filled).min(frames - i);
            for (ch, cur) in self.current.iter_mut().enumerate() {
                for &s in &chunk[ch].as_ref()[i..i + n] {
                    cur[0] = cur[0].min(s);
                    cur[1] = cur[1].max(s);
                }
            }
            self.filled += n;
            i += n;
            if self.filled == BASE_BLOCK {
                self.flush_block();
            }
        }
    }

    fn flush_block(&mut self) {
        for (b, cur) in self.base.iter_mut().zip(&mut self.current) {
            b.push(*cur);
            *cur = EMPTY;
        }
        self.filled = 0;
    }

    pub fn finish(mut self) -> PeakCache {
        if self.filled > 0 {
            self.flush_block();
        }
        let mut levels = vec![Level {
            block: BASE_BLOCK,
            channels: self.base,
        }];
        loop {
            let prev = levels.last().unwrap();
            if prev.channels.first().is_none_or(|c| c.len() <= 1) {
                break;
            }
            let next = Level {
                block: prev.block * FACTOR,
                channels: prev
                    .channels
                    .iter()
                    .map(|ch| ch.chunks(FACTOR).map(merge_all).collect())
                    .collect(),
            };
            levels.push(next);
        }
        PeakCache { levels }
    }
}

impl PeakCache {
    pub fn build(buf: &AudioBuffer) -> Self {
        Self::build_from(buf)
    }

    pub fn build_from(audio: &(impl Samples + ?Sized)) -> Self {
        let mut b = PeakBuilder::new(audio.num_channels());
        b.push(&audio.channel_slices(), audio.frames());
        b.finish()
    }

    /// Min and max sample of `channel` over frames `start..end`, for drawing
    /// and overviews. A range of at least two base blocks is widened to whole
    /// base blocks, so only the precomputed peaks are read, never the audio
    /// (except a partial block at the very end); the result may include up to
    /// one block of neighbouring samples on each side, which never hides a
    /// peak. Shorter ranges are exact. Returns `(0, 0)` for empty ranges.
    ///
    /// (Reading a few raw samples per column across a whole file would make
    /// the OS map the whole file into memory again on every repaint.)
    pub fn min_max(
        &self,
        audio: &(impl Samples + ?Sized),
        channel: usize,
        start: usize,
        end: usize,
    ) -> (f32, f32) {
        let block = self.levels[0].block;
        let end = end.min(audio.frames());
        if end.saturating_sub(start) < 2 * block {
            return self.min_max_exact(audio, channel, start, end);
        }
        let start = start / block * block;
        let end = (end.div_ceil(block) * block).min(audio.frames());
        self.min_max_exact(audio, channel, start, end)
    }

    /// Exact min and max over `start..end`, in O(levels × FACTOR + BASE_BLOCK)
    /// time; reads raw samples at the range edges.
    pub fn min_max_exact(
        &self,
        audio: &(impl Samples + ?Sized),
        channel: usize,
        start: usize,
        end: usize,
    ) -> (f32, f32) {
        let end = end.min(audio.frames());
        if start >= end {
            return (0.0, 0.0);
        }
        let span = end - start;
        let level = self.levels.iter().rposition(|l| l.block <= span);
        let [lo, hi] = self.query(audio, channel, level, start, end);
        (lo, hi)
    }

    /// Whole blocks of `level` cover the middle; edges recurse into finer levels.
    fn query(
        &self,
        audio: &(impl Samples + ?Sized),
        channel: usize,
        level: Option<usize>,
        start: usize,
        end: usize,
    ) -> [f32; 2] {
        if start >= end {
            return EMPTY;
        }
        let Some(li) = level else {
            return audio.channel(channel)[start..end]
                .iter()
                .fold(EMPTY, |[lo, hi], &v| [lo.min(v), hi.max(v)]);
        };
        let finer = li.checked_sub(1);
        let l = &self.levels[li];
        let b0 = start.div_ceil(l.block);
        let b1 = (end / l.block).min(l.channels[channel].len());
        if b0 >= b1 {
            return self.query(audio, channel, finer, start, end);
        }
        let mid = merge_all(&l.channels[channel][b0..b1]);
        let left = self.query(audio, channel, finer, start, b0 * l.block);
        let right = self.query(audio, channel, finer, (b1 * l.block).min(end), end);
        merge_all(&[mid, left, right])
    }

    pub fn num_channels(&self) -> usize {
        self.levels[0].channels.len()
    }

    /// Size of the finest level in frames covered (for validating loads).
    fn base_len(&self) -> usize {
        self.levels[0].channels.first().map_or(0, Vec::len)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut w = BufWriter::new(fs::File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&(self.levels.len() as u64).to_le_bytes())?;
        w.write_all(&(self.num_channels() as u64).to_le_bytes())?;
        for l in &self.levels {
            w.write_all(&(l.block as u64).to_le_bytes())?;
            w.write_all(&(l.channels[0].len() as u64).to_le_bytes())?;
            for ch in &l.channels {
                w.write_all(bytemuck::cast_slice(ch))?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Load a saved cache; `None` if missing, corrupt or not matching the audio.
    pub fn load(path: &Path, channels: usize, frames: usize) -> Option<Self> {
        let data = fs::read(path).ok()?;
        let mut pos = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = data.get(pos..pos + n)?;
            pos += n;
            Some(s)
        };
        let u64_at = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap()) as usize;
        if take(8)? != MAGIC {
            return None;
        }
        let n_levels = u64_at(take(8)?);
        if u64_at(take(8)?) != channels || n_levels == 0 || n_levels > 64 {
            return None;
        }
        let mut levels = Vec::with_capacity(n_levels);
        for _ in 0..n_levels {
            let block = u64_at(take(8)?);
            let len = u64_at(take(8)?);
            let mut chans = Vec::with_capacity(channels);
            for _ in 0..channels {
                let bytes = take(len.checked_mul(8)?)?;
                let f = |b: &[u8]| f32::from_le_bytes(b.try_into().unwrap());
                chans.push(
                    bytes
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|b| [f(&b[..4]), f(&b[4..])])
                        .collect(),
                );
            }
            levels.push(Level {
                block,
                channels: chans,
            });
        }
        let cache = PeakCache { levels };
        (cache.levels[0].block == BASE_BLOCK && cache.base_len() == frames.div_ceil(BASE_BLOCK))
            .then_some(cache)
    }
}

fn merge_all(entries: &[[f32; 2]]) -> [f32; 2] {
    entries
        .iter()
        .fold(EMPTY, |[lo, hi], [a, b]| [lo.min(*a), hi.max(*b)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f32) * 0.013).sin() * (i as f32 / n as f32))
            .collect()
    }

    #[test]
    fn matches_brute_force() {
        let n = 100_000;
        let data = signal(n);
        let buf = AudioBuffer::new(48_000, vec![data.clone()]);
        let cache = PeakCache::build(&buf);
        for (s, e) in [
            (0, 10),
            (5, 70),
            (1000, 5000),
            (0, n),
            (12_345, 67_890),
            (31, 33),
            (99_999, 100_000),
            (4095, 4097),
        ] {
            let (lo, hi) = cache.min_max_exact(&buf, 0, s, e);
            let blo = data[s..e].iter().cloned().fold(f32::INFINITY, f32::min);
            let bhi = data[s..e].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            assert_eq!((lo, hi), (blo, bhi), "{s}..{e}");
        }
    }

    #[test]
    fn streaming_build_equals_whole_and_survives_save_load() {
        let n = 54_321;
        let buf = AudioBuffer::new(
            48_000,
            vec![signal(n), signal(n).iter().map(|v| -v).collect()],
        );
        let whole = PeakCache::build(&buf);
        let mut b = PeakBuilder::new(2);
        let mut pos = 0;
        while pos < n {
            let k = 777.min(n - pos);
            let chunk: Vec<&[f32]> = buf.channels().iter().map(|c| &c[pos..pos + k]).collect();
            b.push(&chunk, k);
            pos += k;
        }
        let streamed = b.finish();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.peaks");
        streamed.save(&path).unwrap();
        let loaded = PeakCache::load(&path, 2, n).unwrap();
        assert!(
            PeakCache::load(&path, 2, n + 100).is_none(),
            "length mismatch rejected"
        );
        for (s, e) in [(0, n), (100, 20_000), (40_000, 54_321)] {
            for ch in 0..2 {
                let want = whole.min_max_exact(&buf, ch, s, e);
                assert_eq!(streamed.min_max_exact(&buf, ch, s, e), want);
                assert_eq!(loaded.min_max_exact(&buf, ch, s, e), want);
            }
        }
    }

    /// Audio that counts how often its samples are read.
    struct Counting {
        buf: AudioBuffer,
        reads: std::cell::Cell<usize>,
    }

    impl Samples for Counting {
        fn sample_rate(&self) -> u32 {
            self.buf.sample_rate()
        }
        fn num_channels(&self) -> usize {
            self.buf.num_channels()
        }
        fn frames(&self) -> usize {
            self.buf.frames()
        }
        fn channel(&self, i: usize) -> &[f32] {
            self.reads.set(self.reads.get() + 1);
            self.buf.channel(i)
        }
    }

    #[test]
    fn wide_queries_never_read_the_audio() {
        let audio = Counting {
            // Not a whole number of blocks.
            buf: AudioBuffer::new(48_000, vec![signal(1_000_003)]),
            reads: Default::default(),
        };
        let peaks = PeakCache::build_from(&audio.buf);
        let narrowest = 2 * BASE_BLOCK; // the smallest range that stays coarse
        for (a, b) in [
            (17, 50_017),
            (123_457, 654_321),
            (999, 2_000),
            (5, 5 + narrowest),
        ] {
            audio.reads.set(0);
            let (lo, hi) = peaks.min_max(&audio, 0, a, b);
            assert_eq!(audio.reads.get(), 0, "{a}..{b} read the audio");
            // Never narrower than the exact answer.
            let exact = peaks.min_max_exact(&audio.buf, 0, a, b);
            assert!(lo <= exact.0 && hi >= exact.1);
        }
        // Zoomed in (under two blocks): exact, from the audio.
        let (a, b) = (100, 100 + narrowest - 1);
        let r = peaks.min_max(&audio, 0, a, b);
        assert_eq!(r, peaks.min_max_exact(&audio.buf, 0, a, b));
    }
}
