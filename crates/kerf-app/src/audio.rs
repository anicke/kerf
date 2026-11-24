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
//!
//! Besides normal playback the renderer can play at any speed in either
//! direction (J/K/L shuttle) and *scrub*: the position then chases a target
//! the UI moves with the mouse, like tape under a hand, with only a few blocks
//! of read-ahead so the sound follows the hand closely.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use kerf_core::analysis::SegmentFilter;
use kerf_core::render::{mix_into, RenderOptions};
use kerf_core::{Frame, Project};
use rtrb::{Consumer, Producer, RingBuffer};

/// Device frames per block.
const BLOCK: usize = 512;
/// How far the feeder renders ahead of what is heard.
const READ_AHEAD_SECS: f64 = 0.25;
/// Read-ahead while scrubbing, in blocks (about 20 ms at 48 kHz): enough to
/// ride out a scheduling hiccup, short enough to feel attached to the mouse.
const SCRUB_AHEAD_BLOCKS: usize = 2;
/// Scrubbing covers the distance to the target in about this time…
const SCRUB_FOLLOW_SECS: f64 = 0.06;
/// …with speed changes smoothed over this time (no zipper noise).
const SCRUB_SMOOTH_SECS: f64 = 0.015;
/// Fastest scrub, relative to normal speed.
const SCRUB_MAX_SPEED: f64 = 4.0;
/// Below this speed scrub audio fades out, so a resting hand is silent
/// rather than holding one sample value.
const SCRUB_QUIET_SPEED: f64 = 0.05;

enum Cmd {
    /// Play from `start` to `end` at `speed` (1 = normal). With a negative
    /// speed playback runs backwards and `end` (≤ `start`) is where it stops.
    Play {
        gen: u32,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
        speed: f64,
    },
    /// Follow [`Shared::scrub_target`] from `at` until stopped.
    Scrub {
        gen: u32,
        project: Arc<Project>,
        at: Frame,
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
    /// Project position at the start of the block.
    pos: f64,
    /// Project frames per device frame in the block (negative in reverse).
    step: f64,
    /// Frames of real audio in the block (the rest is silence padding).
    frames: u32,
    /// Playback ends after this block.
    last: bool,
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
    /// Where scrubbing should go (project frame, f64 bits).
    scrub_target: AtomicU64,
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
    /// Speed of the current playback (1 = normal, negative = reverse).
    speed: f64,
    scrubbing: bool,
    /// 100 ms K-weighted energies of what is heard, for the live loudness meter.
    loudness: Option<Consumer<f64>>,
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
            speed: 1.0,
            scrubbing: false,
            loudness: None,
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
    ) -> Result<(cpal::Stream, Sender<Cmd>, Consumer<f64>), String> {
        let (tx, rx) = channel();
        let (feeder, output, loudness) = pipeline(
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
        Ok((stream, tx, loudness))
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Speed of the current playback: 1 is normal, negative is reverse.
    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn is_scrubbing(&self) -> bool {
        self.scrubbing && self.is_playing()
    }

    pub fn position(&self) -> Frame {
        self.shared.position.load(Ordering::Relaxed)
    }

    /// Times the read-ahead ran dry since the stream started (should stay 0).
    #[allow(dead_code)]
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Hand `f` each 100 ms K-weighted energy of the output heard since the
    /// last call (see `kerf_core::analysis::Loudness`).
    pub fn take_loudness(&mut self, mut f: impl FnMut(f64)) {
        if let Some(rx) = &mut self.loudness {
            while let Ok(z) = rx.pop() {
                f(z);
            }
        }
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
        self.start(project, start, end, loop_start, 1.0)
    }

    /// Play from `start` at `speed` (negative: backwards) to the end, or
    /// back to the beginning.
    pub fn shuttle(
        &mut self,
        project: Arc<Project>,
        start: Frame,
        speed: f64,
    ) -> Result<(), String> {
        let end = if speed > 0.0 { project.length() } else { 0 };
        self.start(project, start, end, None, speed)
    }

    fn start(
        &mut self,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
        speed: f64,
    ) -> Result<(), String> {
        self.ensure_stream(project.sample_rate)?;
        let gen = self.bump_generation();
        self.shared.position.store(start, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.speed = speed;
        self.scrubbing = false;
        self.send(Cmd::Play {
            gen,
            project,
            start,
            end,
            loop_start,
            speed,
        });
        Ok(())
    }

    /// Start scrubbing at `at`; move it with [`Player::scrub_to`] and end it
    /// with [`Player::stop`].
    pub fn scrub(&mut self, project: Arc<Project>, at: Frame) -> Result<(), String> {
        self.ensure_stream(project.sample_rate)?;
        let gen = self.bump_generation();
        self.shared
            .scrub_target
            .store((at as f64).to_bits(), Ordering::Relaxed);
        self.shared.position.store(at, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.speed = 0.0;
        self.scrubbing = true;
        self.send(Cmd::Scrub { gen, project, at });
        Ok(())
    }

    /// Where scrubbing should head (the mouse position).
    pub fn scrub_to(&self, frame: Frame) {
        self.shared
            .scrub_target
            .store((frame as f64).to_bits(), Ordering::Relaxed);
    }

    /// Swap in an edited project while playing.
    pub fn update(&mut self, project: Arc<Project>) {
        if self.is_playing() {
            let gen = self.bump_generation();
            self.send(Cmd::Update { gen, project });
        }
    }

    pub fn stop(&mut self) {
        self.scrubbing = false;
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
        let (stream, tx, loudness) = match self.open_stream(&device, &config, format) {
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
        self.loudness = Some(loudness);
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
) -> (Feeder, Output, Consumer<f64>) {
    let channels = channels.max(1);
    let target = ((device_rate as f64 * READ_AHEAD_SECS) as usize / BLOCK).max(4);
    // Twice the read-ahead: after a seek or edit, fresh audio can queue up
    // behind a full read-ahead of stale audio instead of waiting for it to drain.
    let capacity = 2 * target + 2;
    let (sample_tx, sample_rx) = RingBuffer::new(capacity * BLOCK * channels);
    let (mark_tx, mark_rx) = RingBuffer::new(capacity);
    // A minute of segments: plenty even if the UI stalls.
    let (loud_tx, loud_rx) = RingBuffer::new(600);
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
        current: None,
        played: 0,
        skip: 0,
        loudness: SegmentFilter::new(device_rate, channels),
        loudness_tx: loud_tx,
    };
    (feeder, output, loud_rx)
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
                speed,
            } => {
                self.switch_generation(gen);
                self.renderer.start(project, start, end, loop_start, speed);
                self.active = true;
                self.resume_next = false;
            }
            Cmd::Scrub { gen, project, at } => {
                self.switch_generation(gen);
                self.renderer.start_scrub(project, at);
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
        let target = if self.renderer.scrub.is_some() {
            SCRUB_AHEAD_BLOCKS.min(self.target)
        } else {
            self.target
        };
        while self.active
            && self.ahead() < target as u64
            && self.samples.slots() >= self.block.len()
            && self.marks.slots() >= 1
        {
            if let Some(scrub) = &mut self.renderer.scrub {
                scrub.target = f64::from_bits(self.shared.scrub_target.load(Ordering::Relaxed));
            }
            let pos = self.renderer.pos;
            let frames = self.renderer.render(&mut self.block);
            let last = frames < BLOCK;
            if self.samples.push_entire_slice(&self.block).is_err() {
                return; // cannot happen: space was checked above
            }
            let resume = std::mem::take(&mut self.resume_next);
            let _ = self.marks.push(Mark {
                gen: self.gen,
                pos,
                step: self.renderer.last_step,
                frames: frames as u32,
                last,
                resume,
            });
            self.pushed += 1;
            if last {
                self.active = false;
            }
        }
    }
}

/// Scrubbing: the position chases `target` with a smoothed speed.
struct Scrub {
    target: f64,
    /// Speed (relative to normal) and gain reached at the end of the last block.
    speed: f64,
    gain: f32,
}

/// Mixes the timeline at the device rate and channel layout.
struct Renderer {
    device_rate: u32,
    device_channels: usize,
    project: Option<Arc<Project>>,
    /// Fractional project frame.
    pos: f64,
    /// Where playback stops: the end going forwards, the start in reverse.
    end: Frame,
    loop_start: Option<Frame>,
    /// 1 = normal; negative plays backwards.
    speed: f64,
    scrub: Option<Scrub>,
    /// Project frames per device frame in the last block rendered.
    last_step: f64,
    scratch: Vec<Vec<f32>>,
    /// Source position and gain of each output frame (reverse and scrub).
    path: Vec<f64>,
    gains: Vec<f32>,
    opts: RenderOptions,
}

/// Project channel heard on device channel `d` (mono feeds every channel).
fn source_channel(project_channels: usize, d: usize) -> Option<usize> {
    if project_channels == 1 {
        Some(0)
    } else {
        (d < project_channels).then_some(d)
    }
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
            speed: 1.0,
            scrub: None,
            last_step: 0.0,
            scratch: vec![vec![0.0; 16_384]; 8],
            path: Vec::new(),
            gains: Vec::new(),
            opts: RenderOptions::default(),
        }
    }

    #[cfg(test)]
    fn pos(&self) -> Frame {
        self.pos as Frame
    }

    fn start(
        &mut self,
        project: Arc<Project>,
        start: Frame,
        end: Frame,
        loop_start: Option<Frame>,
        speed: f64,
    ) {
        self.replace(project, start);
        self.end = end;
        self.loop_start = loop_start;
        self.speed = speed;
        self.scrub = None;
    }

    fn start_scrub(&mut self, project: Arc<Project>, at: Frame) {
        self.replace(project, at);
        self.loop_start = None;
        self.scrub = Some(Scrub {
            target: at as f64,
            speed: 0.0,
            gain: 0.0,
        });
    }

    fn replace(&mut self, project: Arc<Project>, at: Frame) {
        if self.scratch.len() < project.channels as usize {
            self.scratch
                .resize(project.channels as usize, vec![0.0; 16_384]);
        }
        self.project = Some(project);
        self.pos = at as f64;
    }

    fn ensure_scratch(&mut self, frames: usize) {
        if self.scratch[0].len() < frames {
            for ch in &mut self.scratch {
                ch.resize(frames, 0.0);
            }
        }
    }

    /// Project frames per device frame at normal speed.
    fn ratio(&self, project: &Project) -> f64 {
        project.sample_rate as f64 / self.device_rate as f64
    }

    /// Fill `out` (interleaved device frames). Returns the number of frames of
    /// audio produced; fewer than requested means playback reached its end
    /// (the remainder is silence). Scrubbing never ends.
    fn render(&mut self, out: &mut [f32]) -> usize {
        let Some(project) = self.project.clone() else {
            out.fill(0.0);
            self.last_step = 0.0;
            return 0;
        };
        if self.scrub.is_some() {
            self.render_scrub(&project, out);
            out.len() / self.device_channels
        } else if self.speed < 0.0 {
            self.render_reverse(&project, out)
        } else {
            self.render_forward(&project, out)
        }
    }

    fn render_forward(&mut self, project: &Project, out: &mut [f32]) -> usize {
        let dc = self.device_channels;
        let frames = out.len() / dc;
        let pc = project.channels as usize;
        let ratio = self.ratio(project) * self.speed;
        self.last_step = ratio;
        self.ensure_scratch((frames as f64 * ratio).ceil() as usize + 2);

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
            mix_into(project, base, src_frames, &mut self.scratch, &self.opts);

            for i in 0..n {
                let p = frac0 + i as f64 * ratio;
                let i0 = p.floor() as usize;
                let f = (p - i0 as f64) as f32;
                let frame_out = &mut out[(written + i) * dc..(written + i + 1) * dc];
                for (d, slot) in frame_out.iter_mut().enumerate() {
                    *slot = match source_channel(pc, d) {
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

    /// Play backwards, stopping at `end`.
    fn render_reverse(&mut self, project: &Project, out: &mut [f32]) -> usize {
        let dc = self.device_channels;
        let frames = out.len() / dc;
        let step = self.ratio(project) * -self.speed;
        self.last_step = -step;
        let stop = self.end as f64;
        let n = if self.pos < stop {
            0
        } else {
            (((self.pos - stop) / step).floor() as usize + 1).min(frames)
        };
        let pos = self.pos;
        self.path.clear();
        self.path.extend((0..n).map(|i| pos - i as f64 * step));
        if n > 0 {
            self.render_path(project, out, n, false);
        }
        out[n * dc..].fill(0.0);
        self.pos -= n as f64 * step;
        n
    }

    /// Move towards the scrub target, the speed following how far away it is.
    fn render_scrub(&mut self, project: &Project, out: &mut [f32]) {
        let frames = out.len() / self.device_channels;
        let rate = project.sample_rate as f64;
        let ratio = self.ratio(project);
        let len = project.length() as f64;
        let secs = frames as f64 / self.device_rate as f64;
        let Some(s) = self.scrub.as_mut() else {
            return;
        };
        let dist = s.target.clamp(0.0, len) - self.pos;
        let want = (dist / (SCRUB_FOLLOW_SECS * rate)).clamp(-SCRUB_MAX_SPEED, SCRUB_MAX_SPEED);
        let alpha = 1.0 - (-secs / SCRUB_SMOOTH_SECS).exp();
        let (s0, g0) = (s.speed, s.gain);
        let mut s1 = s0 + (want - s0) * alpha;
        if dist.abs() < 1.0 && s1.abs() < SCRUB_QUIET_SPEED {
            s1 = 0.0;
        }
        let g1 = (s1.abs() / SCRUB_QUIET_SPEED).min(1.0) as f32;
        s.speed = s1;
        s.gain = g1;
        if s0 == 0.0 && s1 == 0.0 {
            out.fill(0.0);
            self.last_step = 0.0;
            return;
        }
        // Ramp speed and gain across the block.
        self.path.clear();
        self.gains.clear();
        let mut p = self.pos;
        for i in 0..frames {
            let t = (i as f64 + 0.5) / frames as f64;
            self.path.push(p);
            self.gains.push(g0 + (g1 - g0) * t as f32);
            p = (p + (s0 + (s1 - s0) * t) * ratio).clamp(0.0, len);
        }
        self.render_path(project, out, frames, true);
        self.last_step = (p - self.pos) / frames as f64;
        self.pos = p;
    }

    /// Write the first `n` output frames from the source positions in
    /// `self.path` (≥ 0), scaled by `self.gains` when `gained`.
    fn render_path(&mut self, project: &Project, out: &mut [f32], n: usize, gained: bool) {
        let dc = self.device_channels;
        let pc = project.channels as usize;
        let (lo, hi) = self.path[..n]
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        let base = lo.max(0.0).floor() as Frame;
        let span = (hi.ceil() as Frame).saturating_sub(base) as usize + 2;
        self.ensure_scratch(span);
        mix_into(project, base, span, &mut self.scratch, &self.opts);
        for i in 0..n {
            let x = (self.path[i] - base as f64).max(0.0);
            let i0 = x.floor() as usize;
            let f = (x - i0 as f64) as f32;
            let g = if gained { self.gains[i] } else { 1.0 };
            for d in 0..dc {
                out[i * dc + d] = match source_channel(pc, d) {
                    Some(c) => {
                        let s = &self.scratch[c];
                        g * (s[i0] + (s[i0 + 1] - s[i0]) * f)
                    }
                    None => 0.0,
                };
            }
        }
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
    /// Block being played and how many of its frames have been played.
    current: Option<Mark>,
    played: usize,
    /// Device frames to skip silently (catching up after an edit).
    skip: usize,
    loudness: SegmentFilter,
    loudness_tx: Producer<f64>,
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
                        if m.resume && m.gen == gen && m.step != 0.0 {
                            let heard = self.shared.position.load(Ordering::Relaxed);
                            let behind = (heard as f64 - m.pos) / m.step;
                            self.skip = behind.max(0.0) as usize;
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
                    self.loudness.sample(c, v);
                    if c == ch - 1 {
                        if let Some(z) = self.loudness.end_frame() {
                            let _ = self.loudness_tx.push(z);
                        }
                    }
                    out[i * ch + k] = T::from_sample(v);
                }
                chunk.commit_all();
                self.played += n;
                i += n;
                // What is heard, even old audio still playing out after an edit (the
                // edited audio then resumes from exactly here).
                let within = self.played.min(m.frames as usize) as f64;
                let pos = (m.pos + within * m.step).max(0.0) as Frame;
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
        r.start(ramp_project(), 10, 20, None, 1.0);
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
        r.start(ramp_project(), 0, 4, Some(2), 1.0);
        let mut out = vec![0.0f32; 8];
        assert_eq!(r.render(&mut out), 8);
        let want: Vec<f32> = [0, 1, 2, 3, 2, 3, 2, 3].iter().map(|&i| v(i)).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn resamples_to_device_rate() {
        // Project at 1 kHz, device at 2 kHz: every other output sample interpolates.
        let mut r = Renderer::new(2000, 1);
        r.start(ramp_project(), 0, 1000, None, 1.0);
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
        loudness: Consumer<f64>,
    }

    fn rig(device_rate: u32) -> Rig {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        let (feeder, output, loudness) = pipeline(rx, shared.clone(), device_rate, 1);
        Rig {
            tx,
            shared,
            feeder,
            output,
            loudness,
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
                    speed: 1.0,
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
    fn live_loudness_gets_a_segment_per_100_ms_played() {
        // 8 kHz output (the test project is 1 kHz; K-weighting needs a real rate).
        let mut r = rig(8000);
        r.play(0, 2000);
        for _ in 0..5 {
            r.pull(800);
            r.pump();
        }
        // (The test ramp is nearly DC, which K-weighting removes, so only
        // the segment timing is checked here; the levels are tested in core.)
        let mut segments = Vec::new();
        while let Ok(z) = r.loudness.pop() {
            segments.push(z);
        }
        assert_eq!(segments.len(), 5);
        assert!(segments.iter().all(|z| z.is_finite() && *z >= 0.0));
        // Stopped: silence is output but not measured.
        r.shared.generation.fetch_add(1, Ordering::AcqRel);
        r.shared.playing.store(false, Ordering::Relaxed);
        r.tx.send(Cmd::Stop).unwrap();
        r.pull(4000);
        assert!(r.loudness.pop().is_err());
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
    fn plays_backwards_and_stops_at_the_start() {
        let mut r = Renderer::new(1000, 1);
        r.start(ramp_project(), 5, 0, None, -1.0);
        let mut out = vec![9.0f32; 10];
        assert_eq!(r.render(&mut out), 6);
        let want: Vec<f32> = [5, 4, 3, 2, 1, 0].iter().map(|&i| v(i)).collect();
        assert_eq!(&out[..6], &want[..]);
        assert!(out[6..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn shuttles_at_double_speed() {
        let mut r = Renderer::new(1000, 1);
        r.start(ramp_project(), 100, 2000, None, 2.0);
        let mut out = vec![0.0f32; 5];
        r.render(&mut out);
        assert_eq!(out, [100, 102, 104, 106, 108].map(v));
        let mut r = Renderer::new(1000, 1);
        r.start(ramp_project(), 100, 0, None, -4.0);
        r.render(&mut out);
        assert_eq!(out, [100, 96, 92, 88, 84].map(v));
    }

    #[test]
    fn scrub_follows_the_target_then_falls_silent() {
        let mut r = Renderer::new(1000, 1);
        r.start_scrub(ramp_project(), 100);
        let mut out = vec![0.0f32; 16];
        // Resting: silence.
        r.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(r.pos(), 100);
        // Move the target ahead: audio plays forwards towards it…
        r.scrub.as_mut().unwrap().target = 300.0;
        let mut heard = Vec::new();
        for _ in 0..200 {
            r.render(&mut out);
            heard.extend_from_slice(&out);
        }
        assert!(heard.iter().any(|&s| s != 0.0));
        // …arrives without passing it by more than a little, and goes quiet.
        assert!((r.pos - 300.0).abs() < 1.0, "{}", r.pos);
        assert!(out.iter().all(|&s| s == 0.0));
        // Backwards too.
        r.scrub.as_mut().unwrap().target = 50.0;
        for _ in 0..200 {
            r.render(&mut out);
        }
        assert!((r.pos - 50.0).abs() < 1.0, "{}", r.pos);
        // Never beyond the project.
        r.scrub.as_mut().unwrap().target = 1e9;
        for _ in 0..2000 {
            r.render(&mut out);
        }
        assert!(r.pos <= 2000.0);
    }

    #[test]
    fn reverse_position_is_reported_through_the_ring() {
        let mut r = rig(1000);
        let gen = r.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        r.shared.playing.store(true, Ordering::Relaxed);
        r.tx.send(Cmd::Play {
            gen,
            project: ramp_project(),
            start: 1500,
            end: 0,
            loop_start: None,
            speed: -1.0,
        })
        .unwrap();
        r.pump();
        assert_eq!(r.pull(3), [1500, 1499, 1498].map(v));
        r.pull(97);
        assert_eq!(r.shared.position.load(Ordering::Relaxed), 1400);
    }

    #[test]
    fn scrubbing_keeps_little_audio_queued() {
        let mut r = rig(48_000);
        let gen = r.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        r.shared.playing.store(true, Ordering::Relaxed);
        r.tx.send(Cmd::Scrub {
            gen,
            project: ramp_project(),
            at: 0,
        })
        .unwrap();
        r.pump();
        assert_eq!(r.feeder.ahead(), SCRUB_AHEAD_BLOCKS as u64);
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
