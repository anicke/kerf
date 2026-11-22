//! Long-running work (decoding, loading, exporting) on worker threads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use kerf_core::peaks::PeakCache;
use kerf_core::{Project, SourceAudio};

/// Audio decoded, resampled and analysed off the UI thread, ready to insert.
pub struct Prepared {
    pub path: PathBuf,
    pub name: String,
    pub audio: SourceAudio,
    pub peaks: PeakCache,
}

pub enum JobDone {
    Imported(Vec<Result<Prepared, String>>),
    Loaded(PathBuf, Result<Project, String>),
    Exported(Result<Vec<PathBuf>, String>),
    /// Song-start analysis, with the tracks and document revision it was made from.
    Analyzed(
        Result<kerf_core::detect::Analysis, String>,
        Vec<kerf_core::TrackId>,
        u64,
    ),
}

#[derive(Clone)]
pub struct JobCtl {
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    ctx: eframe::egui::Context,
}

impl JobCtl {
    /// Report progress (0..=1); returns `false` if the user cancelled.
    pub fn progress(&self, p: f64) -> bool {
        self.progress.store((p as f32).to_bits(), Ordering::Relaxed);
        self.ctx.request_repaint();
        !self.cancel.load(Ordering::Relaxed)
    }
}

pub struct Job {
    pub title: String,
    ctl: JobCtl,
    rx: Receiver<JobDone>,
}

impl Job {
    pub fn spawn(
        ctx: &eframe::egui::Context,
        title: impl Into<String>,
        work: impl FnOnce(JobCtl) -> JobDone + Send + 'static,
    ) -> Self {
        let ctl = JobCtl {
            progress: Arc::new(AtomicU32::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            ctx: ctx.clone(),
        };
        let (tx, rx) = channel();
        let worker_ctl = ctl.clone();
        std::thread::spawn(move || {
            let ctx = worker_ctl.ctx.clone();
            let _ = tx.send(work(worker_ctl));
            ctx.request_repaint();
        });
        Self {
            title: title.into(),
            ctl,
            rx,
        }
    }

    pub fn progress(&self) -> f32 {
        f32::from_bits(self.ctl.progress.load(Ordering::Relaxed))
    }

    pub fn cancel(&self) {
        self.ctl.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelling(&self) -> bool {
        self.ctl.cancel.load(Ordering::Relaxed)
    }

    pub fn poll(&self) -> Option<JobDone> {
        self.rx.try_recv().ok()
    }
}
