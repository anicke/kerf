//! End-to-end tests of the `kerf-cli` binary on generated audio.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const SR: u32 = 16_000;

/// Three "songs" (harmonic tones with a pulse) separated by noisy applause.
fn live_show(dir: &Path) -> PathBuf {
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut noise = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    let mut out = Vec::new();
    for (k, root) in [220.0f32, 196.0, 262.0].into_iter().enumerate() {
        if k > 0 {
            for _ in 0..3 * SR {
                out.push(noise() * 0.08);
            }
        }
        for i in 0..10 * SR {
            let t = i as f32 / SR as f32;
            let pulse = 0.7 + 0.3 * (std::f32::consts::TAU * 2.0 * t).sin().abs();
            let v: f32 = (1..=4)
                .map(|h| (std::f32::consts::TAU * root * h as f32 * t).sin() / h as f32)
                .sum();
            out.push(v * 0.2 * pulse);
        }
    }
    let path = dir.join("show.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for s in out {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn kerf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kerf-cli"))
        .arg("--no-cache")
        .args(args)
        .output()
        .unwrap()
}

fn json(args: &[&str]) -> (bool, Value) {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = kerf(&all);
    let text = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(text.lines().next().unwrap_or("null"))
        .unwrap_or_else(|e| panic!("{e}: {text}"));
    (out.status.success(), v)
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn split_live_show_into_named_tagged_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let show = live_show(dir.path());
    let setlist = dir.path().join("setlist.txt");
    std::fs::write(&setlist, "1. Opener\n2. Slow One\n3. Closer\n").unwrap();
    let out = dir.path().join("album");
    let project = dir.path().join("show.kerf");
    let args = |extra: &[&'static str]| {
        let mut a = vec![
            "split",
            s(&show),
            "--out",
            s(&out),
            "--songs",
            "3",
            "--min-song",
            "5",
            "--setlist",
            s(&setlist),
            "--preset",
            "flac",
            "--artist",
            "The Band",
            "--album",
            "Live",
            "--loudness",
            "-16",
            "--save",
            s(&project),
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        a.extend(extra.iter().map(|x| x.to_string()));
        a
    };
    let run = |extra: &[&'static str]| {
        let a = args(extra);
        json(&a.iter().map(String::as_str).collect::<Vec<_>>())
    };

    // Dry run: reports everything, writes nothing.
    let (ok, v) = run(&["--dry-run"]);
    assert!(ok, "{v}");
    let steps = v["steps"].as_array().unwrap();
    let ops: Vec<&str> = steps.iter().map(|s| s["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        [
            "open",
            "detect_songs",
            "set_album",
            "album_loudness",
            "export"
        ]
    );
    let songs = &steps[1]["result"]["songs"];
    assert_eq!(songs.as_array().unwrap().len(), 3);
    assert_eq!(songs[1]["name"], "Slow One");
    // The second song starts after the first song and its applause (13 s), minus pre-roll.
    let start = songs[1]["position"]["seconds"].as_f64().unwrap();
    assert!((12.0..13.5).contains(&start), "song 2 at {start}");
    assert!(!out.exists() && !project.exists());

    // The real run.
    let (ok, v) = run(&[]);
    assert!(ok, "{v}");
    let files = v["steps"][4]["result"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    let first = PathBuf::from(files[0]["path"].as_str().unwrap());
    assert_eq!(first, out.join("01 - Opener.flac"));
    assert!(files
        .iter()
        .all(|f| Path::new(f["path"].as_str().unwrap()).is_file()));
    assert!(project.is_file());

    // The tagged files open, and the saved project has the named markers.
    let (ok, v) = json(&["info", s(&first)]);
    assert!(ok, "{v}");
    let (ok, v) = json(&["info", s(&project)]);
    assert!(ok, "{v}");
    let markers = v["steps"][1]["result"]["markers"].as_array().unwrap();
    let names: Vec<&str> = markers
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Opener", "Slow One", "Closer"]);
    assert!((v["steps"][1]["result"]["master_gain_db"].as_f64().unwrap()).abs() > 0.1);

    // Refuses to overwrite, and says so with a failing exit code.
    let a = args(&[]);
    let o = kerf(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("use force"));
    let (ok, _) = run(&["--force"]);
    assert!(ok);
}

#[test]
fn export_and_analyze_many_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = live_show(dir.path());
    let b = dir.path().join("copy.wav");
    std::fs::copy(&a, &b).unwrap();
    let out = dir.path().join("cd");

    let o = kerf(&[
        "export",
        s(&a),
        s(&b),
        "--out",
        s(&out),
        "--preset",
        "cd",
        "--start",
        "0:05",
        "--end",
        "15",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for name in ["show.wav", "copy.wav"] {
        let r = hound::WavReader::open(out.join(name)).unwrap();
        let spec = r.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (44_100, 2, 16)
        );
        assert_eq!(r.duration(), 10 * 44_100);
    }

    let o = kerf(&["analyze", s(&a), s(&b)]);
    let text = String::from_utf8(o.stdout).unwrap();
    assert!(o.status.success());
    assert_eq!(text.matches("== ").count(), 2, "{text}");
    assert!(text.contains("LUFS"), "{text}");

    // One missing input fails the run but not the others.
    let o = kerf(&["info", s(&a), "/no/such/file.wav"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("show.wav"));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not found"));
}

#[test]
fn run_json_operations() {
    let dir = tempfile::tempdir().unwrap();
    let show = live_show(dir.path());
    let ops = dir.path().join("ops.jsonl");
    std::fs::write(
        &ops,
        r#"# trim the applause off the end and fade out
{"op": "split", "at": "0:20"}
{"op": "fade_out", "start": 18, "end": 20}
{"op": "nonsense"}
{"op": "history"}
"#,
    )
    .unwrap();
    let o = kerf(&["run", "--ops", s(&ops), s(&show)]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "a bad line fails before anything runs"
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("line 4"));

    std::fs::write(
        &ops,
        r#"[{"op": "delete", "start": 20, "end": 36}, {"op": "fade_out", "start": 18, "end": 20},
            {"op": "split", "at": 1, "tracks": [42]}, {"op": "info"}]"#,
    )
    .unwrap();
    let (ok, v) = json(&["run", "--ops", s(&ops), s(&show)]);
    assert!(!ok);
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 4, "stops at the failed step: {v}");
    assert_eq!(steps[3]["error"]["code"], "not_found");

    let (ok, v) = json(&["run", "--keep-going", "--ops", s(&ops), s(&show)]);
    assert!(!ok);
    let info = &v["steps"][4]["result"];
    assert_eq!(info["length"]["seconds"], 20.0, "{v}");
    assert!(
        info["tracks"][0]["clips"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["fade_out_secs"]
            .as_f64()
            .unwrap()
            > 1.9
    );
}

#[test]
fn usage_errors_exit_with_2() {
    let o = kerf(&["split", "x.wav"]);
    assert_eq!(o.status.code(), Some(2));
    let o = kerf(&["detect", "x.wav", "--setlist", "/no/setlist.txt"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn waveform_png() {
    let dir = tempfile::tempdir().unwrap();
    let show = live_show(dir.path());
    let png = dir.path().join("w.png");
    let o = kerf(&[
        "waveform",
        s(&show),
        "-o",
        s(&png),
        "--width",
        "500",
        "--db",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(&std::fs::read(&png).unwrap()[1..4], b"PNG");
    let o = kerf(&["waveform", s(&show), "-o", s(&png)]);
    assert_eq!(o.status.code(), Some(2), "won't overwrite without --force");
}
