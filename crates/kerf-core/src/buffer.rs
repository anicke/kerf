/// Planar 32-bit float audio.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
    sample_rate: u32,
    channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    /// Create a buffer from planar channel data. All channels must be the same length.
    pub fn new(sample_rate: u32, channels: Vec<Vec<f32>>) -> Self {
        assert!(
            !channels.is_empty(),
            "audio buffer needs at least one channel"
        );
        let len = channels[0].len();
        assert!(
            channels.iter().all(|c| c.len() == len),
            "all channels must have the same length"
        );
        Self {
            sample_rate,
            channels,
        }
    }

    pub fn silent(sample_rate: u32, num_channels: usize, frames: usize) -> Self {
        Self::new(sample_rate, vec![vec![0.0; frames]; num_channels.max(1)])
    }

    pub fn from_interleaved(sample_rate: u32, num_channels: usize, data: &[f32]) -> Self {
        let num_channels = num_channels.max(1);
        let frames = data.len() / num_channels;
        let mut channels = vec![Vec::with_capacity(frames); num_channels];
        for frame in data.chunks_exact(num_channels) {
            for (ch, &s) in channels.iter_mut().zip(frame) {
                ch.push(s);
            }
        }
        Self::new(sample_rate, channels)
    }

    pub fn to_interleaved(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.frames() * self.num_channels());
        for i in 0..self.frames() {
            for ch in &self.channels {
                out.push(ch[i]);
            }
        }
        out
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    pub fn channel(&self, index: usize) -> &[f32] {
        &self.channels[index]
    }

    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        &mut self.channels[index]
    }

    pub fn channels(&self) -> &[Vec<f32>] {
        &self.channels
    }

    pub fn into_channels(self) -> Vec<Vec<f32>> {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_roundtrip() {
        let data = [1.0, -1.0, 0.5, -0.5, 0.25, -0.25];
        let buf = AudioBuffer::from_interleaved(48_000, 2, &data);
        assert_eq!(buf.frames(), 3);
        assert_eq!(buf.channel(1), &[-1.0, -0.5, -0.25]);
        assert_eq!(buf.to_interleaved(), data);
    }
}
