# Kerf

A fast, precise, cross-platform audio editor written in Rust (Windows, macOS, Linux).
Focused purely on editing: no plugins or effects, just careful, non-destructive editing.

## Highlights

- **32-bit float end to end.** All audio is held as 32-bit float at the project rate. Nothing is
  quantized until you export, so gain changes and normalization never clip internally, and float
  export keeps peaks above 0 dBFS. 32-bit float WAV round-trips bit-exactly.
- **Non-destructive and sample-exact.** Clips reference immutable source audio. Splitting anywhere,
  or cutting and pasting back, renders bit-identical audio. Fades are anchored to the source, so
  splitting inside a fade is inaudible.
- **Unlimited undo/redo.** Every edit is one undoable step, including fader drags (merged into one
  step). Snapshots share audio, so history costs almost no memory.
- **Normalization.** Peak (dBFS), RMS, or integrated loudness (ITU-R BS.1770 / EBU R128 LUFS,
  checked against EBU Tech 3341). Optional peak ceiling. Per track or linked (keeps the balance).
  Works on a selection or whole tracks.
- **Rendering and track splits.** Export the mix or a selection; split an album recording into one
  file per region between markers; export each track as a stem; or render a track in place to
  consolidate edits. WAV output in 16/24/32-bit PCM (with TPDF dither) or 32-bit float.
- **Song markers for live recordings.** *Markers → Detect Song Starts…* finds where each song
  begins in a concert recording. Live recordings have no silence between songs (applause, crowd,
  talking), so Kerf looks for where the *music* stops and starts, using loudness relative to the
  show's own song level and how noise-like the sound is. A quiet passage inside a song is not
  mistaken for a gap. If you know how many songs there were, say so and the strongest gaps win.
  Paste a setlist to name the tracks, preview the markers live, then export with *Export Split at
  Markers* or *Export Mix with CUE Sheet* (one gapless WAV plus a `.cue` file). Spoken
  introductions stay with the song they introduce. Re-running replaces only automatic markers.
  Songs that flow into each other without a gap need a marker by hand.
- **CD-safe track boundaries.** Split and CUE exports can snap each track boundary to a CD sector
  (1/75 s, moving it at most 6.7 ms; your markers stay put) and pad only the end of the album, so
  there are no sector boundary errors: tracks burn gaplessly as separate files or from the CUE
  sheet. The export dialog shows how many boundaries would otherwise be affected. Works at any rate
  that is a multiple of 75 Hz (44.1, 48, 88.2, 96 kHz): those sectors convert exactly to CD sectors.
  *Markers → Snap Markers to CD Sectors* moves the markers themselves.
- **Export presets and conversion.** *Audio CD* sets 16-bit, 44.1 kHz stereo, TPDF dither and
  CD-safe boundaries in one click, from a project at any rate; *Archive* keeps 32-bit float at the
  project rate. Any export can convert sample rate (high-quality resampling) and channels (mono ↔
  stereo). Split exports render and convert the whole album in one pass and cut it at the
  converted boundaries, so resampled tracks still join seamlessly.
- **Editing.** Cut/copy/paste (insert, overwrite, or mix), delete (close gap), silence (leave gap),
  trim to selection, insert silence, split, move and trim clips with snapping, clip gain,
  track gain/pan/mute/solo, and markers. Panning uses a constant-power law (−3 dB at centre) for
  mono material and a balance control for stereo material.
- **Fades and crossfades.** Drag the handles at the top corners of any clip to set its fade-in
  and fade-out, or fade over a selection with I / O (works across clip boundaries). Four curve
  shapes. Where clips on a track overlap after a move or trim, an equal-power crossfade is added
  automatically and follows the overlap as you edit; fades you made yourself are never replaced.
  Toggle under *Process → Automatic crossfades*.
- **Import.** WAV, FLAC, MP3, OGG/Vorbis, AIFF, CAF, M4A/AAC, ALAC (via Symphonia). Files are
  resampled with a high-quality FFT resampler when their rate differs from the project's.
- **Small memory footprint.** Imported audio is decoded once into a disk cache and memory-mapped,
  so RAM use doesn't grow with the length of your material: a 20-minute stereo 32-bit float file
  adds about 50 MB (waveform overview data), against about 490 MB when held in memory. Reopening
  a project maps the cache directly, with no decoding. Playback renders about 0.25 s ahead on its
  own thread, so disk reads never reach the real-time audio thread, and stop, seek and edits still
  take effect at once.
- **Projects** (`.kerf`) are JSON plus references to your audio files. Rendered material is
  saved beside the project as 32-bit float WAV.

## Building

Requires Rust 1.95+ (pinned to 1.96 by `rust-toolchain.toml`).

```sh
cargo run --release -p kerf              # launch
cargo run --release -p kerf -- song.wav  # open files / a .kerf project
cargo test --workspace
```

### Audio cache

Decoded audio lives in `~/.cache/kerf/audio` (Linux), `~/Library/Caches/kerf/audio` (macOS) or
`%LOCALAPPDATA%\kerf\audio` (Windows). Set `KERF_CACHE_DIR` to put it elsewhere, such as a fast
drive. Entries are re-created automatically if a source file changes, and are deleted after 30 days
unused. *File → Audio Cache…* shows its size and frees everything the open project (including its
undo history) isn't using. The cache can always be deleted safely. It uses about the same space as the audio at
32-bit float (about 1.4 GB per hour of stereo at 48 kHz). If no cache folder can be created, Kerf
falls back to keeping audio in memory.

On Linux, building needs the ALSA development headers (`libasound2-dev` / `alsa-lib-devel`).

## Layout

| Crate | Purpose |
|---|---|
| `crates/kerf-core` | UI-free engine: model, edits, undo, mixing, analysis, I/O, projects. Fully unit-tested. |
| `crates/kerf-app`  | Desktop app (egui/eframe), real-time playback (cpal). |

`cargo run -p kerf-core --example demo_project -- <dir>` generates a demo project that exercises
most features.
`cargo run --release -p kerf-core --example live_show -- show.wav` generates a synthetic concert
for trying song-start detection.

## Keyboard

| Key | Action | Key | Action |
|---|---|---|---|
| Space | Play / stop | S | Split at cursor (or selection edges) |
| Del / ⌫ | Delete, close gap | Ctrl+L | Silence, keep gap |
| Ctrl+X / C / V | Cut / copy / paste | Ctrl+T | Trim to selection |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo | Ctrl+M | Normalize |
| Ctrl+G | Gain | I / O | Fade in / out over selection |
| Ctrl+R | Render track in place | M | Add marker |
| , / . | Previous / next marker | + / − / Z | Zoom in / out / to selection |
| Ctrl+F | Zoom to fit | L / N | Toggle loop / snap |
| Ctrl+E | Export mix | Ctrl+Shift+E | Export split at markers |

Mouse: drag in a track to select time (across tracks). Drag a clip's title bar to move it, even to
another track. Drag clip edges to trim. Drag the small squares at a clip's top corners to set its fades
(right-click the clip to change the curve or remove them). Hold Alt to disable snapping. Ctrl+wheel zooms, Shift+wheel
scrolls. Drag the ruler to select across all tracks. Double-click a marker to rename it.
Drag and drop files onto the window.
