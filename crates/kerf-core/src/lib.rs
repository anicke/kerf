//! Kerf core: a UI-independent, non-destructive audio editing engine.
//!
//! Design overview
//! ---------------
//! * All audio is held in memory as **32-bit float**, planar, at the project
//!   sample rate. Nothing is ever quantised until export, so edits, gain changes
//!   and normalization never clip or accumulate rounding error, and float exports
//!   preserve content above 0 dBFS.
//! * Imported audio becomes an immutable [`Source`](project::Source) shared via
//!   `Arc`. With an [`AudioCache`] its samples live in memory-mapped cache
//!   files, so RAM use stays small however long the material is. The timeline is made of [`Clip`](project::Clip)s that *reference*
//!   ranges of sources, so splitting, cutting, pasting, trimming, gain and fades
//!   are all non-destructive metadata changes.
//! * Because sources are shared, cloning a [`Project`](project::Project) is
//!   cheap. Undo/redo ([`Document`](document::Document)) therefore stores full
//!   project snapshots, which makes every edit trivially and exactly undoable.
//! * Rendering ([`render`]) mixes the timeline on the fly in fixed-size chunks,
//!   used identically for playback, analysis and export.

pub mod album;
pub mod analysis;
pub mod buffer;
pub mod cache;
pub mod compare;
pub mod decode;
pub mod detect;
pub mod dither;
pub mod document;
pub mod edit;
pub mod encode;
pub mod error;
pub mod io;
pub mod naming;
pub mod ops;
pub mod peaks;
pub mod project;
pub mod render;
pub mod resample;
pub mod session;
pub mod spectrogram;
pub mod store;
pub mod util;
pub mod waveform;

pub use buffer::AudioBuffer;
pub use cache::AudioCache;
pub use document::Document;
pub use error::{Error, Result};
pub use project::{
    AlbumInfo, Clip, ClipId, Fade, FadeShape, Marker, MarkerId, Project, Source, SourceId, Track,
    TrackId,
};
pub use store::{Samples, SourceAudio};

/// A position or length on the timeline, in sample frames at the project rate.
pub type Frame = u64;
