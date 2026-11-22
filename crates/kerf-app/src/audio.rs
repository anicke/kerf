//! Real-time playback with read-ahead.
//!
//! Two threads share the work:
//!
//! * A **feeder** thread (ordinary priority) mixes the timeline with
//!   [`kerf_core::render::mix_into`], converts to the device rate and channel
//!   layout, and pushes fixed-size blocks into a lock-free ring buffer about a
//!   quarter second ahead of the playhead. Reading memory-mapped audio may hit
//!   the disk; that happens here, where a stall is harmless.
//! * The device **callback** only copies blocks out of the ring. It never
//!   mixes, allocates, locks, or touches project data, so disk or CPU hiccups
//!   cannot cause dropouts unless they outlast the whole read-ahead buffer.
//!
//! Every block is tagged with a *generation*. Play, stop, seek and edits bump
//! the generation from the UI thread, and the callback discards blocks from
//! older generations immediately, so transport changes are heard at once even
//! though audio is buffered ahead.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use kerf_core::render::{mix_into, RenderOptions};
use kerf_core::{Frame, Project};
use rtrb::{Consumer, Producer, RingBuffer};

/// Device frames per block.
const BLOCK: usize = 512;
/// How far the feeder renders ahead of what is heard.
const READ_AHEAD_SECS: f64 = 0.25;

enum Cmd {
    Play {
        gen: u32,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
    },
    /// Edited project: continue from the audible position with the new content.
    Update {
        gen: u32,
        project: Arc<Project>,
    },
    Stop,
}

/// Describes one block in the sample ring (pushed after its samples).
#[derive(Clone, Copy, Debug)]
struct Mark {
    gen: u32,
    /// Project frame at the start of the block.
    pos: Frame,
    /// Frames of real audio in the block (the rest is silence padding).
    frames: u32,
    /// Playback ends after this block.
    last: bool,
    /// Project sample rate, to turn played device frames into a position.
    project_rate: u32,
    /// First block after an edit: continues from the audible position, so the
    /// output skips whatever it already played of the old audio meanwhile.
    resume: bool,
}

#[derive(Default)]
pub struct Shared {
    /// Project frame currently being heard.
    position: AtomicU64,
    playing: AtomicBool,
    generation: AtomicU32,
    /// Peak per output channel (f32 bits) since the UI last read it.
    meters: [AtomicU32; 2],
    underruns: AtomicU64,
    /// Blocks the output has finished or discarded (lets the feeder track
    /// how much of the ring holds current audio).
    consumed_blocks: AtomicU64,
}

pub struct Player {
    stream: Option<cpal::Stream>,
    tx: Option<Sender<Cmd>>,
    shared: Arc<Shared>,
    stream_project_rate: u32,
    pub device_rate: u32,
    pub device_name: String,
    /// Chosen output device id (`None`: system default).
    device_id: Option<String>,
    /// Chosen buffer size in frames (`None`: device default).
    buffer_frames: Option<u32>,
    /// Set when the chosen device or buffer could not be used.
    pub fallback_note: Option<String>,
}

/// Output devices as `(id, description)`.
pub fn output_devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d
                .description()
                .map(|x| x.to_string())
                .unwrap_or_else(|_| id.clone());
            Some((id, name))
        })
        .collect()
}

impl Default for Player {
    fn default() -> Self {
        Self {
            stream: None,
            tx: None,
            shared: Arc::new(Shared::default()),
            stream_project_rate: 0,
            device_rate: 0,
            device_name: String::new(),
            device_id: None,
            buffer_frames: None,
            fallback_note: None,
        }
    }
}

impl Player {
    /// Start the feeder thread and the device stream.
    fn open_stream(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        format: cpal::SampleFormat,
    ) -> Result<(cpal::Stream, Sender<Cmd>), String> {
        let (tx, rx) = channel();
        let (feeder, output) = pipeline(
            rx,
            self.shared.clone(),
            config.sample_rate,
            config.channels as usize,
        );
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(device, config, output),
            cpal::SampleFormat::I16 => build::<i16>(device, config, output),
            cpal::SampleFormat::I32 => build::<i32>(device, config, output),
            cpal::SampleFormat::U16 => build::<u16>(device, config, output),
            cpal::SampleFormat::F64 => build::<f64>(device, config, output),
            other => return Err(format!("Unsupported device sample format {other}")),
        }?;
        std::thread::Builder::new()
            .name("kerf-playback-feeder".into())
            .spawn(move || feeder.run())
            .map_err(|e| e.to_string())?;
        Ok((stream, tx))
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> Frame {
        self.shared.position.load(Ordering::Relaxed)
    }

    /// Times the read-ahead ran dry since the stream started (should stay 0).
    #[allow(dead_code)]
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Take the output peak levels (linear) accumulated since the last call.
    pub fn take_meters(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.shared.meters[i].swap(0, Ordering::Relaxed)))
    }

    fn bump_generation(&self) -> u32 {
        self.shared
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// Start playing `start..end`. With `loop_start`, playback wraps to it at `end`.
    pub fn play(
        &mut self,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
    ) -> Result<(), String> {
        self.ensure_stream(project.sample_rate)?;
        let gen = self.bump_generation();
        self.shared.position.store(start, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.send(Cmd::Play {
            gen,
            project,
            start,
            end,
            loop_start,
        });
        Ok(())
    }

    /// Swap in an edited project while playing.
    pub fn update(&mut self, project: Arc<Project>) {
        if self.is_playing() {
            let gen = self.bump_generation();
            self.send(Cmd::Update { gen, project });
        }
    }

    pub fn stop(&mut self) {
        self.bump_generation();
        self.shared.playing.store(false, Ordering::Relaxed);
        self.send(Cmd::Stop);
    }

    fn send(&mut self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            if tx.send(cmd).is_err() {
                // Feeder gone (stream died): rebuild on next play.
                self.stream = None;
                self.tx = None;
            }
        }
    }

    /// Choose the output device and buffer size; takes effect on the next play.
    pub fn configure(&mut self, device_id: Option<String>, buffer_frames: Option<u32>) {
        if self.device_id != device_id || self.buffer_frames != buffer_frames {
            self.stop();
            self.device_id = device_id;
            self.buffer_frames = buffer_frames;
            self.stream = None;
            self.tx = None;
        }
    }

    fn ensure_stream(&mut self, project_rate: u32) -> Result<(), String> {
        if self.stream.is_some() && self.tx.is_some() && self.stream_project_rate == project_rate {
            return Ok(());
        }
        self.stream = None;
        self.tx = None; // ends the old feeder thread
        let host = cpal::default_host();
        self.fallback_note = None;
        let chosen = self
            .device_id
            .as_deref()
            .and_then(|id| id.parse::<cpal::DeviceId>().ok())
            .and_then(|id| host.device_by_id(&id));
        if self.device_id.is_some() && chosen.is_none() {
            self.fallback_note =
                Some("Chosen output device not found; using the system default".into());
        }
        let device = match chosen {
            Some(d) => d,
            None => host
                .default_output_device()
                .ok_or("No audio output device found")?,
        };
        self.device_name = device
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "Audio output".into());

        // Prefer a config running at the project rate (no resampling), f32, ≥2 channels.
        let mut best: Option<(i32, cpal::SupportedStreamConfig)> = None;
        if let Ok(configs) = device.supported_output_configs() {
            for c in configs {
                if c.min_sample_rate() <= project_rate && project_rate <= c.max_sample_rate() {
                    let score = (c.sample_format() == cpal::SampleFormat::F32) as i32 * 2
                        + (c.channels() >= 2) as i32;
                    if best.as_ref().is_none_or(|(s, _)| score > *s) {
                        best = Some((score, c.with_sample_rate(project_rate)));
                    }
                }
            }
        }
        let config = match best {
            Some((_, c)) => c,
            None => device.default_output_config().map_err(|e| e.to_string())?,
        };
        let format = config.sample_format();
        let mut config: cpal::StreamConfig = config.into();
        if let Some(n) = self.buffer_frames {
            config.buffer_size = cpal::BufferSize::Fixed(n);
        }
        let (stream, tx) = match self.open_stream(&device, &config, format) {
            Ok(ok) => ok,
            Err(e) if self.buffer_frames.is_some() => {
                // Not every device accepts every buffer size: fall back.
                config.buffer_size = cpal::BufferSize::Default;
                self.fallback_note = Some(format!(
                    "Buffer size not supported ({e}); using the device default"
                ));
                self.open_stream(&device, &config, format)?
            }
            Err(e) => return Err(e),
        };
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        self.tx = Some(tx);
        self.stream_project_rate = project_rate;
        self.device_rate = config.sample_rate;
        Ok(())
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut output: Output,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| output.fill(data),
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// Create the feeder/output pair connected by ring buffers.
fn pipeline(
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    device_rate: u32,
    channels: usize,
) -> (Feeder, Output) {
    let channels = channels.max(1);
    let target = ((device_rate as f64 * READ_AHEAD_SECS) as usize / BLOCK).max(4);
    // Twice the read-ahead: after a seek or edit, fresh audio can queue up
    // behind a full read-ahead of stale audio instead of waiting for it to drain.
    let capacity = 2 * target + 2;
    let (sample_tx, sample_rx) = RingBuffer::new(capacity * BLOCK * channels);
    let (mark_tx, mark_rx) = RingBuffer::new(capacity);
    let feeder = Feeder {
        rx,
        shared: shared.clone(),
        samples: sample_tx,
        marks: mark_tx,
        renderer: Renderer::new(device_rate, channels),
        gen: 0,
        active: false,
        resume_next: false,
        block: vec![0.0; BLOCK * channels],
        target,
        pushed: 0,
        stale_at_switch: 0,
        consumed_at_switch: 0,
    };
    let output = Output {
        samples: sample_rx,
        marks: mark_rx,
        shared,
        channels,
        device_rate,
        current: None,
        played: 0,
        skip: 0,
    };
    (feeder, output)
}

// ---------------------------------------------------------------------------
// Feeder (ordinary thread)
// ---------------------------------------------------------------------------

struct Feeder {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    samples: Producer<f32>,
    marks: Producer<Mark>,
    renderer: Renderer,
    gen: u32,
    active: bool,
    resume_next: bool,
    block: Vec<f32>,
    /// Read-ahead in blocks of current audio.
    target: usize,
    pushed: u64,
    /// Blocks in the ring when the generation last changed (all now stale).
    stale_at_switch: u64,
    consumed_at_switch: u64,
}

impl Feeder {
    fn run(mut self) {
        loop {
            let cmd = if self.active {
                match self.rx.recv_timeout(Duration::from_millis(3)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match self.rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                }
            };
            if let Some(c) = cmd {
                self.handle(c);
            }
            if !self.drain_commands() {
                return;
            }
            self.fill();
        }
    }

    /// Apply queued commands; `false` if the player is gone.
    fn drain_commands(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(c) => self.handle(c),
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn switch_generation(&mut self, gen: u32) {
        self.gen = gen;
        let consumed = self.shared.consumed_blocks.load(Ordering::Acquire);
        self.stale_at_switch = self.pushed.saturating_sub(consumed);
        self.consumed_at_switch = consumed;
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play {
                gen,
                project,
                start,
                end,
                loop_start,
            } => {
                self.switch_generation(gen);
                self.renderer.start(project, start, end, loop_start);
                self.active = true;
                self.resume_next = false;
            }
            Cmd::Update { gen, project } => {
                // Also when the final block is already rendered: the end may
                // still be a read-ahead away from being heard.
                if self.active
                    || (self.renderer.project.is_some()
                        && self.shared.playing.load(Ordering::Relaxed))
                {
                    self.switch_generation(gen);
                    // Resume from what is being heard, not from how far we rendered.
                    let heard = self.shared.position.load(Ordering::Relaxed);
                    self.renderer.replace(project, heard);
                    self.resume_next = true;
                    self.active = true;
                }
            }
            Cmd::Stop => self.active = false,
        }
    }

    /// Blocks of current-generation audio queued ahead of the output.
    fn ahead(&self) -> u64 {
        let consumed = self.shared.consumed_blocks.load(Ordering::Acquire);
        let queued = self.pushed.saturating_sub(consumed);
        let stale_left = self
            .stale_at_switch
            .saturating_sub(consumed - self.consumed_at_switch);
        queued.saturating_sub(stale_left)
    }

    /// Render blocks until the read-ahead is full or playback ends.
    fn fill(&mut self) {
        while self.active
            && self.ahead() < self.target as u64
            && self.samples.slots() >= self.block.len()
            && self.marks.slots() >= 1
        {
            let pos = self.renderer.pos();
            let frames = self.renderer.render(&mut self.block);
            let last = frames < BLOCK;
            if self.samples.push_entire_slice(&self.block).is_err() {
                return; // cannot happen: space was checked above
            }
            let project_rate = self.renderer.project.as_ref().map_or(0, |p| p.sample_rate);
            let resume = std::mem::take(&mut self.resume_next);
            let _ = self.marks.push(Mark {
                gen: self.gen,
                pos,
                frames: frames as u32,
                last,
                project_rate,
                resume,
            });
            self.pushed += 1;
            if last {
                self.active = false;
            }
        }
    }
}

/// Mixes the timeline at the device rate and channel layout.
struct Renderer {
    device_rate: u32,
    device_channels: usize,
    project: Option<Arc<Project>>,
    /// Fractional project frame.
    pos: f64,
    end: Frame,
    loop_start: Option<Frame>,
    scratch: Vec<Vec<f32>>,
    opts: RenderOptions,
}

impl Renderer {
    fn new(device_rate: u32, device_channels: usize) -> Self {
        Self {
            device_rate,
            device_channels: device_channels.max(1),
            project: None,
            pos: 0.0,
            end: 0,
            loop_start: None,
            scratch: vec![vec![0.0; 16_384]; 8],
            opts: RenderOptions::default(),
        }
    }

    fn pos(&self) -> Frame {
        self.pos as Frame
    }

    fn start(
        &mut self,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
    ) {
        self.replace(project, start);
        self.end = end;
        self.loop_start = loop_start;
    }

    fn replace(&mut self, project: Arc<Project>, at: Frame) {
        if self.scratch.len() < project.channels as usize {
            self.scratch
                .resize(project.channels as usize, vec![0.0; 16_384]);
        }
        self.project = Some(project);
        self.pos = at as f64;
    }

    /// Fill `out` (interleaved device frames). Returns the number of frames of
    /// audio produced; fewer than requested means playback reached its end
    /// (the remainder is silence).
    fn render(&mut self, out: &mut [f32]) -> usize {
        let dc = self.device_channels;
        let frames = out.len() / dc;
        let Some(project) = self.project.clone() else {
            out.fill(0.0);
            return 0;
        };
        let pc = project.channels as usize;
        let ratio = project.sample_rate as f64 / self.device_rate as f64;
        let needed = (frames as f64 * ratio).ceil() as usize + 2;
        if self.scratch[0].len() < needed {
            for ch in &mut self.scratch {
                ch.resize(needed, 0.0);
            }
        }

        let mut written = 0;
        while written < frames {
            if self.pos >= self.end as f64 {
                match self.loop_start {
                    Some(ls) if ls < self.end => self.pos = ls as f64,
                    _ => {
                        out[written * dc..].fill(0.0);
                        return written;
                    }
                }
            }
            let base = self.pos.floor() as Frame;
            let frac0 = self.pos - base as f64;
            let remaining_src = self.end as f64 - self.pos;
            let n = (frames - written).min((remaining_src / ratio).ceil().max(1.0) as usize);
            let src_frames =
                ((frac0 + n as f64 * ratio).ceil() as usize + 2).min(self.scratch[0].len());
            mix_into(&project, base, src_frames, &mut self.scratch, &self.opts);

            for i in 0..n {
                let p = frac0 + i as f64 * ratio;
                let i0 = p.floor() as usize;
                let f = (p - i0 as f64) as f32;
                let frame_out = &mut out[(written + i) * dc..(written + i + 1) * dc];
                for (d, slot) in frame_out.iter_mut().enumerate() {
                    let src_ch = if pc == 1 {
                        Some(0)
                    } else if d < pc {
                        Some(d)
                    } else {
                        None
                    };
                    *slot = match src_ch {
                        Some(c) => {
                            let s = &self.scratch[c];
                            if ratio == 1.0 {
                                s[i0]
                            } else {
                                s[i0] + (s[i0 + 1] - s[i0]) * f
                            }
                        }
                        None => 0.0,
                    };
                }
            }
            self.pos += n as f64 * ratio;
            written += n;
        }
        frames
    }
}

// ---------------------------------------------------------------------------
// Output (real-time callback)
// ---------------------------------------------------------------------------

struct Output {
    samples: Consumer<f32>,
    marks: Consumer<Mark>,
    shared: Arc<Shared>,
    channels: usize,
    device_rate: u32,
    /// Block being played and how many of its frames have been played.
    current: Option<Mark>,
    played: usize,
    /// Device frames to skip silently (catching up after an edit).
    skip: usize,
}

impl Output {
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        let gen = self.shared.generation.load(Ordering::Acquire);
        let playing = self.shared.playing.load(Ordering::Relaxed);
        let mut peaks = [0f32; 2];
        let mut i = 0;
        while i < frames {
            // 1. Generation changed: jump to the new audio as soon as it is queued.
            if self.current.map_or_else(
                || self.front_gen().is_some_and(|g| g != gen),
                |m| m.gen != gen,
            ) {
                match self.find_gen(gen) {
                    Some(k) => {
                        self.drop_current();
                        self.discard(k);
                    }
                    None if !playing => {
                        // Stopped: silence now.
                        self.drop_current();
                        self.discard(self.marks.slots());
                        break;
                    }
                    // Seek or edit not rendered yet: keep playing the old audio
                    // for these few milliseconds rather than dropping out.
                    None => {}
                }
            }

            // 2. Next block.
            if self.current.is_none() {
                match self.marks.pop() {
                    Ok(m) => {
                        self.current = Some(m);
                        self.played = 0;
                        if m.resume && m.gen == gen {
                            let heard = self.shared.position.load(Ordering::Relaxed);
                            let behind = heard.saturating_sub(m.pos);
                            self.skip = (behind * self.device_rate as u64
                                / m.project_rate.max(1) as u64)
                                as usize;
                        }
                    }
                    Err(_) => {
                        if playing {
                            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                        break; // nothing buffered: silence for the rest
                    }
                }
            }
            let m = self.current.unwrap();

            // 3. Skip audio already heard (after an edit), or play.
            if self.skip > 0 {
                let k = self.skip.min(BLOCK - self.played);
                if let Ok(c) = self.samples.read_chunk(k * ch) {
                    c.commit_all();
                }
                self.skip -= k;
                self.played += k;
            } else {
                let n = (BLOCK - self.played).min(frames - i);
                let Ok(chunk) = self.samples.read_chunk(n * ch) else {
                    break;
                };
                let (a, b) = chunk.as_slices();
                for (k, &v) in a.iter().chain(b).enumerate() {
                    let c = k % ch;
                    if c < 2 {
                        peaks[c] = peaks[c].max(v.abs());
                    }
                    out[i * ch + k] = T::from_sample(v);
                }
                chunk.commit_all();
                self.played += n;
                i += n;
                // What is heard, even old audio still playing out after an edit (the
                // edited audio then resumes from exactly here).
                let within = self.played.min(m.frames as usize) as u64;
                let pos = m.pos + within * m.project_rate as u64 / self.device_rate.max(1) as u64;
                self.shared.position.store(pos, Ordering::Relaxed);
                if m.gen == gen && m.last && self.played >= m.frames as usize {
                    self.shared.playing.store(false, Ordering::Relaxed);
                }
            }
            if self.played >= BLOCK {
                self.current = None;
                self.shared.consumed_blocks.fetch_add(1, Ordering::Release);
            }
        }
        for slot in &mut out[i * ch..] {
            *slot = T::from_sample(0.0f32);
        }
        for (meter, p) in self.shared.meters.iter().zip(peaks) {
            let prev = f32::from_bits(meter.load(Ordering::Relaxed));
            if p > prev {
                meter.store(p.to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// Generation of the next queued block.
    fn front_gen(&mut self) -> Option<u32> {
        let chunk = self.marks.read_chunk(1).ok()?;
        chunk.as_slices().0.first().map(|m| m.gen)
    }

    /// Index of the first queued block of `gen` (peeks without consuming).
    fn find_gen(&mut self, gen: u32) -> Option<usize> {
        let n = self.marks.slots();
        let chunk = self.marks.read_chunk(n).ok()?;
        let (a, b) = chunk.as_slices();
        a.iter().chain(b).position(|m| m.gen == gen)
    }

    /// Abandon the rest of the current block.
    fn drop_current(&mut self) {
        if self.current.take().is_some() {
            let left = (BLOCK - self.played.min(BLOCK)) * self.channels;
            if let Ok(c) = self.samples.read_chunk(left) {
                c.commit_all();
            }
            self.skip = 0;
            self.shared.consumed_blocks.fetch_add(1, Ordering::Release);
        }
    }

    /// Discard the next `k` queued blocks.
    fn discard(&mut self, k: usize) {
        for _ in 0..k {
            if self.marks.pop().is_ok() {
                if let Ok(c) = self.samples.read_chunk(BLOCK * self.channels) {
                    c.commit_all();
                }
                self.shared.consumed_blocks.fetch_add(1, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kerf_core::AudioBuffer;

    fn ramp_project() -> Arc<Project> {
        let mut p = Project::new(1000, 1);
        let data: Vec<f32> = (0..2000).map(|i| i as f32 / 10_000.0).collect();
        p.add_source_as_track("r", None, AudioBuffer::new(1000, vec![data]), 0);
        Arc::new(p)
    }

    fn v(i: usize) -> f32 {
        i as f32 / 10_000.0
    }

    // --- Renderer -----------------------------------------------------------

    #[test]
    fn renders_sample_exact_and_stops_at_end() {
        let mut r = Renderer::new(1000, 2);
        r.start(ramp_project(), 10, 20, None);
        let mut out = vec![9.0f32; 2 * 16];
        assert_eq!(r.render(&mut out), 10);
        for i in 0..10 {
            assert_eq!(
                (out[2 * i], out[2 * i + 1]),
                (v(10 + i), v(10 + i)),
                "mono duplicated to stereo"
            );
        }
        assert!(out[20..].iter().all(|&s| s == 0.0), "silence after end");
    }

    #[test]
    fn loops_back_to_loop_start() {
        let mut r = Renderer::new(1000, 1);
        r.start(ramp_project(), 0, 4, Some(2));
        let mut out = vec![0.0f32; 8];
        assert_eq!(r.render(&mut out), 8);
        let want: Vec<f32> = [0, 1, 2, 3, 2, 3, 2, 3].iter().map(|&i| v(i)).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn resamples_to_device_rate() {
        // Project at 1 kHz, device at 2 kHz: every other output sample interpolates.
        let mut r = Renderer::new(2000, 1);
        r.start(ramp_project(), 0, 1000, None);
        let mut out = vec![0.0f32; 10];
        r.render(&mut out);
        for (i, s) in out.iter().enumerate() {
            assert!(
                (s - v(0) - (i as f32 / 2.0) / 10_000.0).abs() < 1e-7,
                "{i}: {s}"
            );
        }
    }

    // --- Feeder + output pipeline -------------------------------------------

    struct Rig {
        tx: Sender<Cmd>,
        shared: Arc<Shared>,
        feeder: Feeder,
        output: Output,
    }

    fn rig(device_rate: u32) -> Rig {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        let (feeder, output) = pipeline(rx, shared.clone(), device_rate, 1);
        Rig {
            tx,
            shared,
            feeder,
            output,
        }
    }

    impl Rig {
        fn play(&mut self, start: Frame, end: Frame) {
            let gen = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
            self.shared.playing.store(true, Ordering::Relaxed);
            self.tx
                .send(Cmd::Play {
                    gen,
                    project: ramp_project(),
                    start,
                    end,
                    loop_start: None,
                })
                .unwrap();
            self.pump();
        }
        fn pump(&mut self) {
            self.feeder.drain_commands();
            self.feeder.fill();
        }
        fn pull(&mut self, n: usize) -> Vec<f32> {
            let mut out = vec![-1.0f32; n];
            self.output.fill(&mut out);
            out
        }
    }

    #[test]
    fn plays_through_the_ring_sample_exact() {
        let mut r = rig(1000);
        r.play(100, 1900);
        let mut got = Vec::new();
        for _ in 0..10 {
            got.extend(r.pull(300));
            r.pump();
        }
        let want: Vec<f32> = (100..1900).map(v).collect();
        assert_eq!(&got[..1800], &want[..]);
        assert!(got[1800..].iter().all(|&s| s == 0.0));
        assert!(
            !r.shared.playing.load(Ordering::Relaxed),
            "stopped at the end"
        );
        assert_eq!(r.shared.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reported_position_tracks_what_is_heard() {
        let mut r = rig(1000);
        r.play(0, 2000);
        r.pull(700);
        // The feeder has rendered far ahead, but the position is what was played.
        assert!(r.feeder.renderer.pos() > 700);
        assert_eq!(r.shared.position.load(Ordering::Relaxed), 700);
    }

    #[test]
    fn stop_and_seek_take_effect_immediately_despite_read_ahead() {
        let mut r = rig(1000);
        r.play(0, 2000);
        r.pull(100);
        // Stop: buffered audio is discarded, output is silent right away.
        r.shared.generation.fetch_add(1, Ordering::AcqRel);
        r.shared.playing.store(false, Ordering::Relaxed);
        r.tx.send(Cmd::Stop).unwrap();
        assert!(r.pull(200).iter().all(|&s| s == 0.0));
        // Seek (a new play) starts exactly at the new position.
        r.play(1500, 2000);
        let got = r.pull(50);
        assert_eq!(got, (1500..1550).map(v).collect::<Vec<_>>());
    }

    #[test]
    fn edits_during_playback_are_seamless() {
        let mut r = rig(1000);
        r.play(0, 2000);
        let before = r.pull(600);
        assert_eq!(before, (0..600).map(v).collect::<Vec<_>>());
        // Edit (track gain → -1). The UI bumps the generation and sends the update…
        let mut p = (*ramp_project()).clone();
        p.tracks[0].gain = -1.0;
        let gen = r.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        r.tx.send(Cmd::Update {
            gen,
            project: Arc::new(p),
        })
        .unwrap();
        // …but before the feeder reacts, the device asks for more: old audio continues.
        assert_eq!(
            r.pull(20),
            (600..620).map(v).collect::<Vec<_>>(),
            "no gap while the edit renders"
        );
        r.pump();
        // Then the edited audio picks up exactly where playback is: no repeat, no jump.
        assert_eq!(r.pull(20), (620..640).map(|i| -v(i)).collect::<Vec<_>>());
        assert_eq!(r.shared.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn seek_while_playing_switches_without_silence() {
        let mut r = rig(1000);
        r.play(0, 2000);
        r.pull(100);
        r.play(1500, 2000); // play() pumps the feeder: new audio is queued behind the old
        assert_eq!(r.pull(30), (1500..1530).map(v).collect::<Vec<_>>());
        assert_eq!(r.shared.position.load(Ordering::Relaxed), 1530);
    }

    #[test]
    fn underrun_outputs_silence_and_is_counted() {
        let mut r = rig(1000);
        r.play(0, 100_000);
        // Pull far more than the read-ahead holds without letting the feeder refill.
        let got = r.pull(20_000);
        assert!(got[got.len() - 100..].iter().all(|&s| s == 0.0));
        assert!(r.shared.underruns.load(Ordering::Relaxed) > 0);
    }
}
