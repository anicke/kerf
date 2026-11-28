---
name: kerf
description: Edit audio with the Kerf editor's command-line tool (kerf-cli) - split a live concert or radio recording into songs, place and name markers, level loudness, tag and export albums (FLAC, WAV, CD), analyze levels, draw waveforms, and null-test edits. Use when the user wants to cut, split, trim, fade, normalize, mark, export or inspect audio files or .kerf projects.
---

# Kerf from the command line

Kerf is a non-destructive audio editor. `kerf-cli` runs its engine without the GUI. Edits never
touch the source audio: a `.kerf` project file references the audio and holds the tracks, clips,
markers, album details and the undo history.

Check it's available with `kerf-cli --version`. With the AppImage it's
`./kerf-<version>.AppImage kerf-cli …`.

## Working on a project step by step

The `.kerf` file holds the state between calls. Each call opens it, runs some operations and saves
it back (`--save`), so everything you do is in the file and can be undone later, even in the app.

```sh
# Start: open the recording and save it as a project next to it
kerf-cli run show.m4a --save show.kerf --ops - <<'EOF'
{"op": "info"}
EOF

# Every later call: operations on the project, saved in place
kerf-cli run show.kerf --save --ops - <<'EOF'
{"op": "set_markers", "markers": [{"at": 0, "name": "Intro"}, {"at": "3:57.1", "name": "Song"}]}
EOF
```

- Operations are JSON, one object per line (or a JSON array). `kerf-cli ops` lists them;
  `kerf-cli ops NAME` shows one's parameters. Don't guess parameters: look them up.
- Times are seconds (`83.5`), `"m:ss"` (`"1:23.5"`), `"h:mm:ss"` or exact samples (`"4008000smp"`).
- Each edit is one undo step. `{"op": "history"}` lists them; `{"op": "undo"}` takes back the last.
- A run stops at the first failed operation; with `--save`, what succeeded before it is saved.
  Read the output to see which step failed before running more.
- Ids (tracks, markers, clips) come from `{"op": "info"}` and stay the same across saves. Run
  `info` before editing by id.
- Add `--json` for machine-readable results. Saving fails with `modified` if something else
  changed the project since this call opened it: reopen it rather than forcing.
- `kerf-cli info/analyze/waveform/export/compare/detect/split` are shortcuts for common jobs;
  `kerf-cli --help` lists them.

## You can't hear, so measure and look

- `analyze`: peak, true peak, RMS, LUFS, loudness range and clipping of a range.
- `waveform` draws a PNG of any range; read the image. Zoom in by choosing `start`/`end`; `db_scale`
  shows quiet passages. From the shell: `kerf-cli waveform show.kerf -o w.png --start 3:30 --end 4:30`.
- `find_silences`, and `detect_songs` (song starts in live recordings, with a confidence for each).
- `compare` is a null test: against `{"history": n}` it shows exactly what an edit changed; against
  an exported file, whether the export matches.

Check each edit this way before moving on, and show the user what you found before anything is
written.

## A live recording to an album

1. `info` and `analyze` the recording.
2. `detect_songs` (with `songs` and `setlist` if known) without `apply`. Inspect weak or doubtful
   boundaries with `waveform`. Tell the user which ones you're unsure about.
   - Broadcasts (radio, TV) are heavily compressed and often have presenters talking over the
     music; detection finds false starts there. Use the setlist, the song lengths and the
     waveform, and other tools if available (a spectrogram, a speech transcript) to place starts.
3. Place the agreed starts in one step with `set_markers`, then let the user check them in the
   Kerf app (it opens the `.kerf`, with the history) before exporting.
4. Album details with `set_album`; `album_summary`, then `album_loudness` (e.g. -16 LUFS, -1 dBFS
   ceiling).
5. `export` with `kind: "tracks"` and `dry_run: true` first; show the file list and warnings, then
   export for real once the user agrees. Never use `force` without asking.
