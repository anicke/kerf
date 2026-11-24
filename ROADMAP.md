# Kerf Roadmap

Kerf aims to be an open-source, cross-platform audio editor (with first-class Linux support) that
is precise, non-destructive and quick to use, built for two things other editors leave on the table:
**turning live recordings into finished albums**, and **being an editing partner an AI assistant
can actually operate**.

This plan collects the findings from comparing Kerf with established editors (WaveLab, Sound
Forge, Audition, REAPER, Audacity) and from designing LLM integration. Sizes are rough estimates
of focused work.

## Where Kerf stands

Shipped: 32-bit float non-destructive engine with unlimited undo; sample-exact split, cut, paste,
trim, fades and crossfades; peak, RMS and BS.1770 loudness normalization; memory-mapped disk cache
(flat RAM use); song-start detection for live recordings with setlist naming; CD-safe split and CUE
export with an Audio CD preset; keyboard navigation, markers panel, crash recovery, preferences and
device selection; scrubbing and J/K/L shuttle; an undo history panel; a statistics panel with true
peak, loudness range and a live loudness meter; customisable keyboard shortcuts; typed times and formats, zero-crossing snap, vertical zoom and dB view; tagged
FLAC export, album loudness and the Album window; a JSON operations layer and the `kerf-cli`
command-line tool; the `kerf-mcp` MCP server with waveform images, also working live inside the app; FFmpeg as an
optional decoder.

## Positioning: what makes Kerf stand out

| Gap in established editors | Kerf's answer |
|---|---|
| No professional editor on Linux (WaveLab, Sound Forge, Audition are Windows/Mac) | Open source, cross-platform, Linux as a first-class target |
| Finishing a live recording is a chain of manual chores | One workflow: detect, name, level, tag, export |
| GUI-only; no batch use from outside the app | A CLI built on the same engine |
| An AI assistant can't operate an audio editor | An MCP server, including one that edits the open project while you watch |
| Comparing alternative edits means undo, listen, redo, remember | Instant A/B switching during playback, made cheap by Kerf's snapshots |
| "Did this edit change anything else?" goes unanswered | A sample-exact null test that proves it |

## Phase 1: Live recording to finished album

**Status:** done except MP3 (see below): track and album metadata, streaming tagged FLAC export
(16/24-bit, cover art, gapless), album loudness with peak ceiling, the Album window, and richer CUE
sheets.

**MP3 needs a decision first:** encoding means the LAME library (C, LGPL), which affects licensing
and packaging, and gapless MP3 needs the encoder's delay/padding information written into the file.
Alternatives worth weighing: Opus (BSD, modern, gapless by design) instead of or before MP3.

The strongest differentiator, and Kerf's core use case. Builds directly on song detection and CD export.

1. **Track metadata model.** Album fields (artist, album, year, genre, cover art) on the project,
   and per-track fields (title, track number, optional ISRC) on markers. The setlist fills titles.
2. **FLAC and MP3 export with tags.** WAV can't reliably carry titles, so today setlist names are
   lost at export. FLAC (lossless, archival) first, then MP3. Tags, track numbers and cover art.
   Must stay gapless (FLAC is naturally; MP3 needs encoder delay/padding info written).
3. **Album-level loudness.** Measure the whole show and apply one gain to reach a target (e.g. -14
   or -16 LUFS), so quiet songs stay quiet relative to loud ones. Optional true-peak ceiling.
   Per-song normalization stays available but is not the default for albums.
4. **Album view.** One screen listing tracks with title, start, length, loudness and peak, flags
   for clipping or very short/long tracks, and a one-click export with the chosen preset.
5. **Tests:** tag round-trip, gapless joins for FLAC/MP3, album gain preserves relative levels.

Size: about 3–4 days.

## Phase 2: Operations layer and CLI

**Status:** done. `kerf_core::ops` has every capability as a typed, JSON-convertible request and
response (with a `Session` holding one project and its undo history), and `kerf-cli` offers `info`,
`analyze`, `detect`, `split`, `export` and `run` (any operation as JSON). The CLI binary is named
`kerf-cli` because `kerf` is the GUI; on Windows the GUI binary has no console, so the two stay
separate. Not yet done: "find silences" (planned for the MCP inspect tools) and moving the GUI's
own edits onto `ops`.

The foundation for both the CLI and MCP.

1. **Operations layer in `kerf-core` (`ops`).** Every capability as a typed request and response
   that converts to and from JSON: open/import/save, inspect, analyse range, detect songs, edit
   operations, markers, export (with dry run), undo/redo. One implementation, used by the CLI, the
   MCP server and eventually the GUI.
2. **`kerf` CLI** (a separate binary in the workspace). Subcommands mapping onto operations, with
   human-readable output by default and `--json` for scripts and agents. Example:
   `kerf split show.wav --songs 14 --setlist list.txt --preset cd --cue --out album/`.
   Batch over many files; non-zero exit codes on failure; never overwrites without `--force`.
3. **Tests:** each operation through the JSON layer; CLI end-to-end on generated audio.

Size: about 2–3 days.

## Phase 3: MCP server (headless)

**Status:** done. `kerf-mcp` (stdio, `rmcp` 3.11) serves one tool per `ops` request, generated
from its JSON Schema; `waveform` (PNG) and `find_silences` were added to `ops` (and the CLI).
Resources, the `prepare_live_album` prompt, progress and cancellation, and an `--allow` folder
sandbox are in. Deferred to Phase 5: checkpoint/compare and the null test. Audio content is not
sent to the model.

Lets any MCP client (Claude Desktop, Claude Code, IDEs) drive Kerf with no shell access.

The core insight: an LLM **cannot hear**, works **step by step**, and needs **typed, discoverable
operations**. So the server must report audio richly, keep a live session, and describe its tools
precisely.

- **Transport:** stdio first (local, simplest), using the official Rust MCP SDK (`rmcp`). Verify
  its current API and version before starting.
- **Session:** one live project per server, with undo history and the shared disk cache.
- **Tools** (all undoable, each returning a short summary of what changed):
  - Session: open, new, import audio, save, undo, redo, history.
  - Inspect: project summary (tracks, clips, markers, length, format); analyse a range (peak, RMS,
    LUFS, clipping); detect song starts (with per-candidate confidence); find silences;
    **render a waveform image of a range** (PNG returned as an image), which is the model's "eyes".
  - Edit: split, delete/ripple delete, cut/paste, fades, gain, normalize, crossfade settings;
    markers (add, move, rename, remove, apply detected).
  - Album (after Phase 1): set metadata, album loudness, album summary.
  - Export: every kind and preset, with a **dry run** reporting file names, timings and sector
    boundary errors before writing.
  - Compare: checkpoint and compare versions (loudness difference, null test).
- **Resources:** project summary, marker list, edit history.
- **Prompts:** guided workflows, starting with "prepare a live album" (detect, check, name, level,
  tag, export).
- **Safety:** everything non-destructive and undoable; exports never overwrite without an explicit
  flag; dry runs encouraged by tool descriptions; file access limited to paths the user provides.
- **Audio output to the model:** MCP can carry audio content, but whether a given model can listen
  to it varies. Text analysis and waveform images are the dependable path; audio is optional.
- **Tests:** tool schemas; scripted MCP sessions on generated audio, including undo and dry runs.

Size: about 3 days, given Phase 2.

## Phase 4: MCP inside the running app

**Status:** done. The app hosts the MCP server (opt-in in Preferences) on `127.0.0.1` behind a
token in a user-only connection file; `kerf-mcp --app` bridges a client's stdio to it. Operations
run on project snapshots off the UI thread and commit as one "AI: …" undo step only if the user
hasn't edited meanwhile (else they are redone), and never during a drag. Status-bar indicator with
Disconnect. `open`/`new` are not offered in the app. Remote connections (open decision 4) are not
supported.

The standout version: the assistant edits the project you have open, and **you watch the changes
happen on the timeline**, with every step in normal undo. You can stop it, adjust, or take over.

- The app hosts the same MCP tools on a local socket. Requests are queued into the UI loop and
  applied there, so the GUI and the assistant never edit concurrently.
- Each AI edit is labelled in the undo history (e.g. "AI: split at 3:12").
- A visible indicator while an assistant is connected, and a one-click disconnect.
- Opt-in in Preferences; off by default.

Size: about 2–3 days, given Phase 3.

## Phase 5: A/B alternatives and null test

- **A/B alternatives:** mark the current state as A, try a different crossfade, cut or level as B,
  and flip between them **during playback** with one key, staying in sync. Built on project
  snapshots (already cheap). Keep or discard either side.
- **Null test / compare:** compare two files, renders or project versions sample by sample: where
  they differ, by how much (peak and RMS of the difference), with optional automatic alignment.
  Proves edits are lossless and catches accidental changes. Exposed in the app, CLI and MCP.

Size: about 2–3 days.

## Phase 6: Remaining everyday features

**Status:** done.

From the comparison with established editors:

- ~~Scrubbing and J/K/L shuttle~~ (done: drag the playhead or Ctrl+drag the ruler; J/K/L up to
  8× either way, K+J/L for half speed; loop moved to Shift+L).
- ~~Undo history panel~~ (done: *View → History panel* or Ctrl+Shift+H; click any step to go
  back or forward to it, undone steps stay listed until the next edit, the saved state is marked).
- ~~Statistics and loudness panel~~ (done: *View → Statistics panel* or Ctrl+Shift+I; peak, true
  peak, RMS, integrated/max momentary/max short-term loudness, loudness range and clipping of the
  selection or project, measured in the background; live momentary, short-term and integrated
  loudness while playing. True peak and loudness range are also in `analyze` for the CLI and MCP).
- ~~Recent files~~ (done: *File → Open Recent* and Ctrl+Shift+O).
- ~~Customisable shortcuts~~ (done: *Edit → Keyboard Shortcuts…*; press keys to add one, a key
  already in use moves to the new command, reset one command or all; menus and tooltips show the
  keys as bound).

Size: about 3–4 days in total.

## Export improvements

From reviewing export against established editors.

- ~~File-name templates~~ (done: `{nn} - {title}` by default; fields {nn} {num} {title} {artist}
  {album} {year} {genre} {project}, `/` for folders; previewed in the dialog, clashes refused; also
  `--names` in the CLI and `names` for MCP). The app also asks before replacing files and remembers
  the export format and names.
- Leave stretches out of a split (pre-show, chatter between songs): a tick-box per track in the
  export dialog first; start/end regions later.
- Lossy formats (MP3/Opus/AAC), pending open decision 3. AIFF.
- ~~Dither options~~ (done: none / TPDF / TPDF with light or strong (F-weighted) noise shaping for
  16-bit, light by default for Audio CD; silence and audio already exact at the target depth are
  left undithered, so unedited 16-bit audio exports bit-identical; `--dither` in the CLI).
- Smaller: stems that follow the selection; CUE pregaps (`INDEX 00`).

## Phase 7: Platform, packaging and open-source readiness

- ~~**CI on Windows, macOS and Linux**~~ (done: fmt, clippy and cargo-deny on Linux; tests on Linux
  for every change, and on Windows and macOS for `main`, manual runs and PRs labelled `full-ci`.
  Actions pinned to SHAs, cache written only by `main`, job timeouts, Dependabot with a 7-day
  cooldown; the repository allows only GitHub's actions and `Swatinem/rust-cache`, SHA-pinned).
- **Packages:** Linux (AppImage and/or Flatpak), macOS (signed .dmg), Windows (installer).
- ~~**Licence**~~ (done: MIT OR Apache-2.0, with `LICENSE-MIT` and `LICENSE-APACHE`).
- ~~**Dependency licence checks**~~ (done: `cargo-deny` in CI, `deny.toml`; revisit the allowed
  licences once Kerf's own licence is chosen).
- **AAC decoding as a build option,** since AAC may involve patent licensing in some countries
  (MP3's patents have expired).
- ~~Contributor docs~~ (done: `CONTRIBUTING.md` with how the code is organised, `SECURITY.md`).
- **Going public:** run Windows and macOS on every pull request again (minutes are free for public
  repositories); turn on private vulnerability reporting, Dependabot alerts, secret scanning with
  push protection, approval for first-time contributors' workflows, and rules blocking force-pushes
  to `main`.

## Later and ambitious

- **Offline speech transcription** with a local model (nothing sent to the cloud): name songs from
  the band's own introductions ("this one's called…"), and edit speech by editing its transcript
  for podcasts and interviews. Large, but the most striking differentiator.
- **Read uncompressed WAV directly** (no cache copy) when it already matches the project format:
  halves disk use and makes import instant for large WAV collections.
- **Regions** (start/end, not just point markers) for precise gaps between CD tracks.
- **Recording**, if Kerf should ever capture audio rather than only edit it.
- **Spectral view and spectral selection.**

## Known limitations

- Song detection is tuned on synthetic audio; defaults need real concert recordings to refine.
  Songs that flow into each other (segues) need markers by hand.
- Exports at sample rates that aren't a multiple of 75 Hz (e.g. 32 kHz) can't be made CD-safe.
- Playback has only been verified by tests, not on a range of real audio devices.
- Big-endian processors are not supported (no current desktop platform uses one).

## Open decisions

1. ~~Licence choice~~ (decided: MIT OR Apache-2.0).
2. Album loudness default target (-14 LUFS streaming vs. -16 or lower for dynamic live music).
3. Lossy export: MP3 via LAME (LGPL) and/or Opus; also AAC, DDP for CD replication.
4. Whether the in-app MCP server should also accept connections from other machines (default:
   local only).
5. Transcription model and its size/licence, if pursued.

## Suggested order

Phase 1 → Phase 2 → Phase 3 → Phase 4, with Phase 7's CI added early (it protects everything
after it). Phases 5 and 6 can be interleaved as smaller tasks.
