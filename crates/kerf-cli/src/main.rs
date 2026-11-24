//! `kerf-cli`: the Kerf editing engine from the command line.
//!
//! Every command is a short list of operations from `kerf_core::ops`, run in
//! a fresh session per input file, so the CLI behaves exactly like the app
//! and the MCP server. Output is human-readable by default; `--json` prints
//! one JSON document per input for scripts and agents.

mod output;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use kerf_core::ops::{
    Bits, ExportKind, ExportOptions, FileType, OpError, Preset, Request, Response, Session, Time,
};
use kerf_core::AudioCache;

#[derive(Parser)]
#[command(
    name = "kerf-cli",
    version,
    about = "Kerf audio editor from the command line: inspect, detect songs, level and export.",
    after_help = "Times accept seconds (83.5), m:ss (1:23.5), h:mm:ss, or exact samples (4008000smp).\n\
                  Nothing is ever overwritten without --force; --dry-run shows what would be written."
)]
struct Cli {
    /// Print results as JSON (one document per input).
    #[arg(long, global = true)]
    json: bool,
    /// Decode audio into memory instead of the shared disk cache.
    #[arg(long, global = true)]
    no_cache: bool,
    /// No progress output.
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum Cmd {
    /// Show format, length, tracks, clips, markers and album details.
    Info {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    /// Measure peak, true peak, RMS, loudness (LUFS, range) and clipping.
    Analyze {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[command(flatten)]
        range: RangeArgs,
    },
    /// Find where each song starts in a live recording.
    Detect {
        input: PathBuf,
        #[command(flatten)]
        detect: DetectArgs,
        /// Save a project with the song markers placed.
        #[arg(long, value_name = "PROJECT")]
        save: Option<PathBuf>,
        /// Replace an existing project file.
        #[arg(long)]
        force: bool,
    },
    /// Turn a live recording into an album: detect songs, name, tag, level, export one file per song.
    Split {
        input: PathBuf,
        /// Output folder.
        #[arg(short, long)]
        out: PathBuf,
        /// Use the markers already in the project instead of detecting songs.
        #[arg(long, conflicts_with_all = ["songs", "sensitivity", "min_song"])]
        markers: bool,
        #[command(flatten)]
        detect: DetectArgs,
        #[command(flatten)]
        album: AlbumArgs,
        /// Level the whole album with one gain to this loudness (LUFS), e.g. -16.
        #[arg(long, value_name = "LUFS", allow_hyphen_values = true)]
        loudness: Option<f64>,
        /// Peak ceiling for --loudness (dBFS).
        #[arg(long, value_name = "DB", default_value_t = -1.0, allow_hyphen_values = true)]
        ceiling: f64,
        /// Also write the whole show as one file with a CUE sheet.
        #[arg(long)]
        cue: bool,
        #[command(flatten)]
        format: FormatArgs,
        /// Also save the edited project.
        #[arg(long, value_name = "PROJECT")]
        save: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Export audio: the mix, one file per marker, a CUE image, or stems.
    Export {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output file (mix, cue) or folder (tracks, stems). With several inputs, always a folder.
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long, value_enum, default_value_t = Kind::Mix)]
        kind: Kind,
        #[command(flatten)]
        range: RangeArgs,
        #[command(flatten)]
        format: FormatArgs,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Draw the waveform as a PNG: peaks, RMS, clipping in red, markers in yellow.
    Waveform {
        input: PathBuf,
        /// Image file to write.
        #[arg(short, long)]
        out: PathBuf,
        #[command(flatten)]
        range: RangeArgs,
        #[arg(long, default_value_t = 1200)]
        width: u32,
        #[arg(long, default_value_t = 300)]
        height: u32,
        /// Logarithmic (dB) amplitude scale, showing quiet passages.
        #[arg(long)]
        db: bool,
        #[arg(long)]
        force: bool,
    },
    /// Run operations given as JSON (an array, or one object per line) against an input.
    Run {
        /// File of operations, or - for standard input.
        #[arg(long)]
        ops: PathBuf,
        /// Opened before the operations run (optional: they can open files themselves).
        input: Option<PathBuf>,
        /// Continue after a failed operation.
        #[arg(long)]
        keep_going: bool,
    },
}

#[derive(Args, Clone, Default)]
struct RangeArgs {
    /// Start time (default: the beginning).
    #[arg(long)]
    start: Option<Time>,
    /// End time (default: the end).
    #[arg(long)]
    end: Option<Time>,
}

#[derive(Args, Clone, Default)]
struct DetectArgs {
    /// Exact number of songs, if known (the clearest gaps win).
    #[arg(long)]
    songs: Option<usize>,
    /// How readily a gap counts as a song change, 0..1 (default 0.5).
    #[arg(long)]
    sensitivity: Option<f32>,
    /// Shortest song in seconds (default 60).
    #[arg(long, value_name = "SECS")]
    min_song: Option<f64>,
    /// Song titles, one per line (numbering like "1." is ignored).
    #[arg(long, value_name = "FILE")]
    setlist: Option<PathBuf>,
}

#[derive(Args, Clone, Default)]
struct AlbumArgs {
    #[arg(long)]
    artist: Option<String>,
    /// Album title.
    #[arg(long)]
    album: Option<String>,
    #[arg(long)]
    year: Option<String>,
    #[arg(long)]
    genre: Option<String>,
    #[arg(long)]
    comment: Option<String>,
    /// Front cover image (JPEG or PNG), embedded in FLAC files.
    #[arg(long, value_name = "IMAGE")]
    cover: Option<PathBuf>,
}

#[derive(Args, Clone, Default)]
struct FormatArgs {
    /// cd: 16-bit 44.1 kHz WAV, CD-safe; flac: FLAC 24-bit; wav: 32-bit float WAV.
    #[arg(long, value_enum, default_value_t = PresetArg::Wav)]
    preset: PresetArg,
    /// Sample format override.
    #[arg(long, value_enum)]
    bits: Option<BitsArg>,
    /// File type override.
    #[arg(long, value_enum)]
    container: Option<ContainerArg>,
    /// Convert to this sample rate.
    #[arg(long)]
    rate: Option<u32>,
    /// Convert to this many channels.
    #[arg(long)]
    channels: Option<u16>,
    /// Turn off dither when reducing bit depth.
    #[arg(long)]
    no_dither: bool,
    /// Don't snap track boundaries to CD sectors (on by default with --preset cd).
    #[arg(long)]
    no_cd_safe: bool,
    /// File names for one file per track (default "{nn} - {title}"). Fields:
    /// {nn} {num} {title} {artist} {album} {year} {genre} {project}; "/" makes
    /// folders, e.g. "{artist} - {album}/{nn} {title}".
    #[arg(long, value_name = "TEMPLATE")]
    names: Option<String>,
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum PresetArg {
    #[default]
    Wav,
    Cd,
    Flac,
}

#[derive(Clone, Copy, ValueEnum)]
enum BitsArg {
    #[value(name = "16")]
    I16,
    #[value(name = "24")]
    I24,
    #[value(name = "32")]
    I32,
    Float,
}

#[derive(Clone, Copy, ValueEnum)]
enum ContainerArg {
    Wav,
    Flac,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Kind {
    Mix,
    Tracks,
    Cue,
    Stems,
}

impl FormatArgs {
    fn options(&self, kind: ExportKind) -> ExportOptions {
        ExportOptions {
            kind,
            preset: match self.preset {
                PresetArg::Wav => Preset::Wav,
                PresetArg::Cd => Preset::Cd,
                PresetArg::Flac => Preset::Flac,
            },
            format: self.bits.map(|b| match b {
                BitsArg::I16 => Bits::Int16,
                BitsArg::I24 => Bits::Int24,
                BitsArg::I32 => Bits::Int32,
                BitsArg::Float => Bits::Float32,
            }),
            container: self.container.map(|c| match c {
                ContainerArg::Wav => FileType::Wav,
                ContainerArg::Flac => FileType::Flac,
            }),
            sample_rate: self.rate,
            channels: self.channels,
            dither: self.no_dither.then_some(false),
            cd_safe: self.no_cd_safe.then_some(false),
            names: self.names.clone(),
            ..Default::default()
        }
    }

    fn extension(&self) -> &'static str {
        match (self.container, self.preset) {
            (Some(ContainerArg::Flac), _) | (None, PresetArg::Flac) => "flac",
            _ => "wav",
        }
    }
}

impl DetectArgs {
    fn request(&self, apply: bool) -> Result<Request, String> {
        let setlist = match &self.setlist {
            Some(p) => std::fs::read_to_string(p)
                .map_err(|e| format!("can't read setlist {}: {e}", p.display()))?
                .lines()
                .map(String::from)
                .collect(),
            None => Vec::new(),
        };
        Ok(Request::DetectSongs {
            start: None,
            end: None,
            tracks: None,
            songs: self.songs,
            sensitivity: self.sensitivity,
            min_song_secs: self.min_song,
            min_gap_secs: None,
            pre_roll_secs: None,
            placement: Default::default(),
            setlist,
            apply,
        })
    }
}

impl AlbumArgs {
    fn request(&self) -> Option<Request> {
        let a = self.clone();
        let any = a.artist.is_some()
            || a.album.is_some()
            || a.year.is_some()
            || a.genre.is_some()
            || a.comment.is_some()
            || a.cover.is_some();
        any.then_some(Request::SetAlbum {
            artist: a.artist,
            title: a.album,
            year: a.year,
            genre: a.genre,
            comment: a.comment,
            cover: a.cover,
        })
    }
}

/// Operations to run against one input, in a fresh session.
struct Job {
    input: Option<PathBuf>,
    steps: Vec<Request>,
    keep_going: bool,
    /// Where to save a waveform image produced by the steps.
    image: Option<PathBuf>,
}

impl Job {
    fn new(input: &Path, steps: Vec<Request>) -> Self {
        let mut all = vec![Request::Open {
            path: input.to_path_buf(),
            discard: false,
        }];
        all.extend(steps);
        Self {
            input: Some(input.to_path_buf()),
            steps: all,
            keep_going: false,
            image: None,
        }
    }
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into())
}

fn plan(cmd: Cmd) -> Result<Vec<Job>, String> {
    Ok(match cmd {
        Cmd::Info { inputs } => inputs
            .iter()
            .map(|i| Job::new(i, vec![Request::Info]))
            .collect(),
        Cmd::Analyze { inputs, range } => inputs
            .iter()
            .map(|i| {
                Job::new(
                    i,
                    vec![Request::Analyze {
                        start: range.start.clone(),
                        end: range.end.clone(),
                        tracks: None,
                    }],
                )
            })
            .collect(),
        Cmd::Detect {
            input,
            detect,
            save,
            force,
        } => {
            let mut steps = vec![detect.request(save.is_some())?];
            if let Some(path) = save {
                steps.push(Request::Save {
                    path: Some(path),
                    force,
                });
            }
            vec![Job::new(&input, steps)]
        }
        Cmd::Split {
            input,
            out,
            markers,
            detect,
            album,
            loudness,
            ceiling,
            cue,
            format,
            save,
            dry_run,
            force,
        } => {
            let mut steps = Vec::new();
            if !markers {
                steps.push(detect.request(true)?);
            }
            steps.extend(album.request());
            if let Some(target_lufs) = loudness {
                steps.push(Request::AlbumLoudness {
                    target_lufs,
                    ceiling_db: Some(ceiling),
                });
            }
            steps.push(Request::Export {
                path: out.clone(),
                options: format.options(ExportKind::Tracks),
                dry_run,
                force,
            });
            if cue {
                let image = out.join(format!("{}.{}", stem(&input), format.extension()));
                steps.push(Request::Export {
                    path: image,
                    options: format.options(ExportKind::Cue),
                    dry_run,
                    force,
                });
            }
            if let Some(path) = save.filter(|_| !dry_run) {
                steps.push(Request::Save {
                    path: Some(path),
                    force,
                });
            }
            vec![Job::new(&input, steps)]
        }
        Cmd::Export {
            inputs,
            out,
            kind,
            range,
            format,
            dry_run,
            force,
        } => {
            let many = inputs.len() > 1;
            inputs
                .iter()
                .map(|input| {
                    let path = match (many, kind) {
                        (false, _) => out.clone(),
                        (true, Kind::Mix | Kind::Cue) => {
                            out.join(format!("{}.{}", stem(input), format.extension()))
                        }
                        (true, _) => out.join(stem(input)),
                    };
                    let kind = match kind {
                        Kind::Mix => ExportKind::Mix,
                        Kind::Tracks => ExportKind::Tracks,
                        Kind::Cue => ExportKind::Cue,
                        Kind::Stems => ExportKind::Stems,
                    };
                    let options = ExportOptions {
                        start: range.start.clone(),
                        end: range.end.clone(),
                        ..format.options(kind)
                    };
                    Job::new(
                        input,
                        vec![Request::Export {
                            path,
                            options,
                            dry_run,
                            force,
                        }],
                    )
                })
                .collect()
        }
        Cmd::Waveform {
            input,
            out,
            range,
            width,
            height,
            db,
            force,
        } => {
            if out.exists() && !force {
                return Err(format!(
                    "{} exists (use --force to replace it)",
                    out.display()
                ));
            }
            let mut job = Job::new(
                &input,
                vec![Request::Waveform {
                    start: range.start,
                    end: range.end,
                    tracks: None,
                    width: Some(width),
                    height: Some(height),
                    db_scale: db,
                }],
            );
            job.image = Some(out);
            vec![job]
        }
        Cmd::Run {
            ops: file,
            input,
            keep_going,
        } => {
            let text = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())?
            } else {
                std::fs::read_to_string(&file)
                    .map_err(|e| format!("can't read {}: {e}", file.display()))?
            };
            let steps = parse_ops(&text)?;
            let mut job = match &input {
                Some(i) => Job::new(i, steps),
                None => Job {
                    input: None,
                    steps,
                    keep_going,
                    image: None,
                },
            };
            job.keep_going = keep_going;
            vec![job]
        }
    })
}

/// A JSON array of requests, or one request per line (blank lines and lines
/// starting with `#` or `//` are skipped).
fn parse_ops(text: &str) -> Result<Vec<Request>, String> {
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text).map_err(|e| format!("invalid operations: {e}"));
    }
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with('#') && !l.starts_with("//")
        })
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 1)))
        .collect()
}

/// What happened to each step of a job.
pub struct Outcome {
    pub request: Request,
    pub result: Result<Response, OpError>,
}

fn run_job(job: &Job, cache: Option<Arc<AudioCache>>, show_progress: bool) -> Vec<Outcome> {
    let mut session = Session::new(cache);
    let mut out = Vec::new();
    for req in &job.steps {
        let label = output::op_name(req);
        let mut last = -1i64;
        let mut progress = |p: f64| {
            let pct = (p * 100.0) as i64;
            if show_progress && pct != last {
                last = pct;
                eprint!("\r{label}… {pct:>3}%");
            }
            true
        };
        let result = session.execute_with_progress(req.clone(), &mut progress);
        if show_progress && last >= 0 {
            eprint!("\r\x1b[2K");
        }
        let failed = result.is_err();
        out.push(Outcome {
            request: req.clone(),
            result,
        });
        if failed && !job.keep_going {
            break;
        }
    }
    out
}

/// Write the waveform from the outcomes to `path`, keeping the base64 out of
/// the printed result.
fn save_image(outcomes: &mut [Outcome], path: &Path) -> Result<(), String> {
    use base64::Engine;
    for o in outcomes {
        if let Ok(Response {
            data: kerf_core::ops::Data::Waveform(w),
            ..
        }) = &mut o.result
        {
            let png = base64::engine::general_purpose::STANDARD
                .decode(&w.png_base64)
                .map_err(|e| e.to_string())?;
            std::fs::write(path, png).map_err(|e| format!("{}: {e}", path.display()))?;
            w.png_base64 = format!("(written to {})", path.display());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let jobs = match plan(cli.cmd) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let cache = if cli.no_cache {
        None
    } else {
        match AudioCache::open_default() {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                eprintln!("warning: no audio cache ({e}); decoding into memory");
                None
            }
        }
    };
    let show_progress = !cli.json && !cli.quiet && std::io::stderr().is_terminal();
    let many = jobs.len() > 1;
    let mut failed = false;
    for job in &jobs {
        let mut outcomes = run_job(job, cache.clone(), show_progress);
        if let Some(path) = &job.image {
            if let Err(e) = save_image(&mut outcomes, path) {
                eprintln!("error: {e}");
                failed = true;
            }
        }
        failed |= outcomes.iter().any(|o| o.result.is_err());
        if cli.json {
            println!("{}", output::json(job.input.as_deref(), &outcomes));
        } else {
            output::human(job.input.as_deref(), &outcomes, many);
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn ops_files_accept_arrays_and_lines() {
        let a = parse_ops(r#"[{"op": "info"}, {"op": "undo"}]"#).unwrap();
        let b = parse_ops("# comment\n{\"op\": \"info\"}\n\n{\"op\": \"undo\"}\n").unwrap();
        assert_eq!(a, b);
        assert!(parse_ops("{\"op\": \"nope\"}")
            .unwrap_err()
            .starts_with("line 1"));
    }

    #[test]
    fn negative_numbers_parse_as_values() {
        let cli = Cli::try_parse_from([
            "kerf-cli",
            "split",
            "a.wav",
            "-o",
            "x",
            "--loudness",
            "-16",
            "--ceiling",
            "-2",
        ])
        .unwrap();
        let Cmd::Split {
            loudness, ceiling, ..
        } = cli.cmd
        else {
            panic!()
        };
        assert_eq!((loudness, ceiling), (Some(-16.0), -2.0));
    }
}
