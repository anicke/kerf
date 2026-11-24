use super::*;
use crate::io::{self, ExportSettings};
use crate::AudioBuffer;
use serde_json::{json, Value};

const SR: u32 = 44_100;

/// A stereo WAV: `secs` of a -12 dBFS sine.
fn tone(dir: &Path, name: &str, secs: f64) -> PathBuf {
    let n = (secs * SR as f64) as usize;
    let ch: Vec<f32> = (0..n)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / SR as f32).sin() * 0.25)
        .collect();
    let path = dir.join(name);
    io::write_wav(
        &path,
        &AudioBuffer::new(SR, vec![ch.clone(), ch]),
        ExportSettings::default(),
    )
    .unwrap();
    path
}

/// Run a JSON request and return the envelope as a value.
fn run(s: &mut Session, req: Value) -> Value {
    serde_json::from_str(&s.execute_json(&req.to_string())).unwrap()
}

fn ok(s: &mut Session, req: Value) -> Value {
    let v = run(s, req.clone());
    assert_eq!(v["ok"], true, "{req} failed: {v}");
    v["result"].clone()
}

fn err_code(s: &mut Session, req: Value) -> String {
    let v = run(s, req);
    assert_eq!(v["ok"], false, "expected an error: {v}");
    v["error"]["code"].as_str().unwrap().to_string()
}

#[test]
fn times_parse_in_every_notation() {
    let f = |t: Time| t.to_frames(48_000);
    assert_eq!(f(Time::Seconds(1.5)), Some(72_000));
    assert_eq!(f(Time::Text("1:00.5".into())), Some(2_904_000));
    assert_eq!(f(Time::Text("1:00:00".into())), Some(172_800_000));
    assert_eq!(f(Time::Text("2.5s".into())), Some(120_000));
    assert_eq!(f(Time::Text("12345smp".into())), Some(12_345));
    assert_eq!(f(Time::Text("12345 samples".into())), Some(12_345));
    assert_eq!(f(Time::Seconds(-1.0)), None);
    assert_eq!(f(Time::Text("soon".into())), None);
    let t: Time = serde_json::from_value(json!(2)).unwrap();
    assert_eq!(f(t), Some(96_000));
}

#[test]
fn every_request_round_trips_through_json() {
    let reqs = vec![
        Request::New { discard: true },
        Request::Open {
            path: "a.wav".into(),
            discard: false,
        },
        Request::Split {
            at: Time::secs(1.0),
            tracks: Some(vec![1]),
        },
        Request::Normalize {
            mode: Level::Loudness,
            target: Some(-16.0),
            ceiling_db: Some(-1.0),
            independent: false,
            start: None,
            end: None,
            tracks: None,
        },
        Request::Export {
            path: "out".into(),
            options: ExportOptions {
                kind: ExportKind::Tracks,
                preset: Preset::Cd,
                format: Some(Bits::Int24),
                ..Default::default()
            },
            dry_run: true,
            force: false,
        },
    ];
    for r in reqs {
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&j).unwrap(), r, "{j}");
    }
    // Aliases and defaults callers rely on.
    let r: Request = serde_json::from_value(
        json!({"op": "export", "path": "x", "kind": "tracks", "format": "16"}),
    )
    .unwrap();
    let Request::Export { options, .. } = r else {
        panic!()
    };
    assert_eq!(
        (options.kind, options.format),
        (ExportKind::Tracks, Some(Bits::Int16))
    );
}

#[test]
fn malformed_requests_are_rejected_clearly() {
    let mut s = Session::new(None);
    assert_eq!(
        err_code(&mut s, json!({"op": "explode"})),
        "invalid_request"
    );
    assert_eq!(err_code(&mut s, json!({"op": "split"})), "invalid_request");
    assert_eq!(
        err_code(&mut s, json!({"op": "split", "at": 1, "trakcs": [1]})),
        "invalid_request",
        "typos in field names are caught"
    );
    assert_eq!(
        err_code(&mut s, json!({"op": "split", "at": "later"})),
        "invalid_argument"
    );
    assert_eq!(err_code(&mut s, json!({"op": "undo"})), "nothing_to_undo");
    assert_eq!(
        err_code(&mut s, json!({"op": "open", "path": "/no/such.wav"})),
        "not_found"
    );
}

#[test]
fn edit_session_with_undo_and_info() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "take.wav", 10.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));

    let info = ok(&mut s, json!({"op": "info"}));
    assert_eq!(info["type"], "project");
    assert_eq!(info["sample_rate"], SR);
    assert_eq!(info["channels"], 2);
    assert_eq!(info["length"]["frames"], 10 * SR as u64);
    let track = info["tracks"][0]["id"].as_u64().unwrap();

    ok(&mut s, json!({"op": "split", "at": "0:04"}));
    let r = ok(&mut s, json!({"op": "delete", "start": 2, "end": "3s"}));
    assert_eq!(r["summary"], "Delete 0:02.000–0:03.000");
    ok(
        &mut s,
        json!({"op": "add_marker", "at": 5, "name": "Encore"}),
    );
    let info = ok(&mut s, json!({"op": "info"}));
    assert_eq!(info["length"]["frames"], 9 * SR as u64);
    assert_eq!(info["tracks"][0]["clips"].as_array().unwrap().len(), 3);
    assert_eq!(info["markers"][0]["name"], "Encore");
    assert_eq!(info["dirty"], true);

    let h = ok(&mut s, json!({"op": "history"}));
    assert_eq!(h["undo"].as_array().unwrap().len(), 3);
    ok(&mut s, json!({"op": "undo"}));
    ok(&mut s, json!({"op": "undo"}));
    let info = ok(&mut s, json!({"op": "info"}));
    assert_eq!(info["length"]["frames"], 10 * SR as u64);
    assert!(info["markers"].as_array().unwrap().is_empty());
    ok(&mut s, json!({"op": "redo"}));

    // An edit that changes nothing says so and leaves no undo step.
    let r = ok(&mut s, json!({"op": "split", "at": 60, "tracks": [track]}));
    assert!(r["summary"].as_str().unwrap().ends_with("nothing changed"));
    assert_eq!(
        ok(&mut s, json!({"op": "history"}))["undo"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        err_code(&mut s, json!({"op": "split", "at": 1, "tracks": [999]})),
        "not_found"
    );
}

#[test]
fn levels_gain_and_normalize() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "a.wav", 4.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    let l = ok(&mut s, json!({"op": "analyze"}));
    assert!((l["peak_db"].as_f64().unwrap() + 12.04).abs() < 0.1, "{l}");
    assert_eq!(l["clipped_samples"], 0);

    ok(&mut s, json!({"op": "gain", "db": 20}));
    let l = ok(&mut s, json!({"op": "analyze", "start": 1, "end": 2}));
    assert!(l["clipped_samples"].as_u64().unwrap() > 0);

    let r = ok(
        &mut s,
        json!({"op": "normalize", "mode": "peak", "target": -1}),
    );
    assert_eq!(r["type"], "normalized");
    let l = ok(&mut s, json!({"op": "analyze"}));
    assert!((l["peak_db"].as_f64().unwrap() + 1.0).abs() < 0.05, "{l}");
}

#[test]
fn cut_and_paste_through_the_session_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "a.wav", 6.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    assert_eq!(
        err_code(&mut s, json!({"op": "paste", "at": 0})),
        "nothing_selected"
    );
    ok(&mut s, json!({"op": "cut", "start": 0, "end": 1}));
    ok(&mut s, json!({"op": "paste", "at": 5}));
    let info = ok(&mut s, json!({"op": "info"}));
    assert_eq!(info["length"]["frames"], 6 * SR as u64);
    assert_eq!(info["tracks"][0]["clips"][1]["start"]["seconds"], 5.0);
}

#[test]
fn album_workflow_detect_name_level_export() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "show.wav", 12.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    for (at, name) in [(0, "Opener"), (4, "Ballad"), (8, "Closer")] {
        ok(&mut s, json!({"op": "add_marker", "at": at, "name": name}));
    }
    let id = ok(&mut s, json!({"op": "info"}))["markers"][1]["id"].clone();
    ok(
        &mut s,
        json!({"op": "update_marker", "id": id, "isrc": "USABC0000001"}),
    );
    ok(
        &mut s,
        json!({"op": "set_album", "artist": "The Band", "title": "Live"}),
    );

    let a = ok(&mut s, json!({"op": "album_summary"}));
    assert_eq!(a["tracks"].as_array().unwrap().len(), 3);
    assert_eq!(a["tracks"][1]["title"], "Ballad");
    assert!(a["tracks"][0]["flags"]
        .as_array()
        .unwrap()
        .contains(&json!("short")));

    let l = ok(&mut s, json!({"op": "album_loudness", "target_lufs": -16}));
    assert!(
        (l["after_lufs"].as_f64().unwrap() + 16.0).abs() < 0.01,
        "{l}"
    );
    assert_eq!(l["limited_by_ceiling"], false);
    // A loud target is held back by the peak ceiling.
    let l = ok(
        &mut s,
        json!({"op": "album_loudness", "target_lufs": -3, "ceiling_db": -6}),
    );
    assert!(l["after_peak_db"].as_f64().unwrap() <= -6.0 + 0.01, "{l}");
    assert_eq!(l["limited_by_ceiling"], true);
    let a = ok(&mut s, json!({"op": "album_summary"}));
    assert!((a["peak_db"].as_f64().unwrap() + 6.0).abs() < 0.05, "{a}");

    let out = dir.path().join("album");
    let dry = ok(
        &mut s,
        json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac", "dry_run": true}),
    );
    assert_eq!(dry["written"], false);
    let files = dry["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0]["path"], json!(out.join("01 - Opener.flac")));
    assert!(!out.exists(), "dry run writes nothing");

    // A name template with a folder from the album details.
    let dry = ok(
        &mut s,
        json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac", "dry_run": true,
               "names": "{artist} - {album}/{num}. {title}"}),
    );
    assert_eq!(
        dry["files"][1]["path"],
        json!(out.join("The Band - Live").join("2. Ballad.flac"))
    );
    // Names that clash or don't parse are refused before anything is written.
    for names in ["{album}", "{nn} {titel}"] {
        assert_eq!(
            err_code(
                &mut s,
                json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac", "names": names})
            ),
            "invalid_argument",
            "{names}"
        );
    }
    assert!(!out.exists());

    let r = ok(
        &mut s,
        json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac"}),
    );
    assert_eq!(r["written"], true);
    for f in r["files"].as_array().unwrap() {
        assert!(Path::new(f["path"].as_str().unwrap()).is_file());
    }
    // Never overwrites without force.
    assert_eq!(
        err_code(
            &mut s,
            json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac"})
        ),
        "exists"
    );
    let dry = ok(
        &mut s,
        json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac", "dry_run": true}),
    );
    assert!(dry["files"][0]["exists"].as_bool().unwrap());
    ok(
        &mut s,
        json!({"op": "export", "path": out, "kind": "tracks", "preset": "flac", "force": true}),
    );
}

#[test]
fn cd_export_reports_sector_errors_unless_cd_safe() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "show.wav", 6.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    ok(
        &mut s,
        json!({"op": "add_marker", "at": "3001smp", "name": "Two"}),
    );
    let r = ok(
        &mut s,
        json!({"op": "export", "path": dir.path(), "kind": "tracks", "preset": "cd", "cd_safe": false, "dry_run": true}),
    );
    assert_eq!(r["sector_errors"], 1);
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
    let r = ok(
        &mut s,
        json!({"op": "export", "path": dir.path(), "kind": "tracks", "preset": "cd", "dry_run": true}),
    );
    assert_eq!(r["sector_errors"], 0);
    assert_eq!(
        r["format"],
        "WAV 16-bit PCM, 44100 Hz, 2 ch, dithered with light noise shaping"
    );
}

#[test]
fn save_and_reopen_project() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "a.wav", 3.0);
    let proj = dir.path().join("p.kerf");
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    ok(&mut s, json!({"op": "add_marker", "at": 1, "name": "M"}));
    assert_eq!(err_code(&mut s, json!({"op": "save"})), "invalid_argument");
    ok(&mut s, json!({"op": "save", "path": proj}));
    // Saving again to its own file needs no force; another existing file does.
    ok(&mut s, json!({"op": "add_marker", "at": 2, "name": "N"}));
    ok(&mut s, json!({"op": "save"}));
    assert_eq!(
        err_code(&mut s, json!({"op": "save", "path": wav})),
        "exists"
    );

    let mut t = Session::new(None);
    ok(&mut t, json!({"op": "open", "path": proj}));
    let info = ok(&mut t, json!({"op": "info"}));
    assert_eq!(info["markers"].as_array().unwrap().len(), 2);
    assert_eq!(info["dirty"], false);
    assert_eq!(info["path"], json!(proj));
}

#[test]
fn responses_round_trip_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "a.wav", 2.0);
    let mut s = Session::new(None);
    s.execute(Request::Open {
        path: wav,
        discard: false,
    })
    .unwrap();
    for req in [
        Request::Info,
        Request::History,
        Request::Analyze {
            start: None,
            end: None,
            tracks: None,
        },
    ] {
        let r = s.execute(req).unwrap();
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&j).unwrap(), r, "{j}");
    }
}

#[test]
fn open_and_new_protect_unsaved_changes() {
    let dir = tempfile::tempdir().unwrap();
    let wav = tone(dir.path(), "a.wav", 2.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": wav}));
    ok(&mut s, json!({"op": "split", "at": 1}));
    assert_eq!(err_code(&mut s, json!({"op": "new"})), "unsaved_changes");
    assert_eq!(
        err_code(&mut s, json!({"op": "open", "path": wav})),
        "unsaved_changes"
    );
    assert_eq!(
        ok(&mut s, json!({"op": "history"}))["undo"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    ok(&mut s, json!({"op": "new", "discard": true}));
}

#[test]
fn find_silences_between_sounds() {
    let dir = tempfile::tempdir().unwrap();
    let a = tone(dir.path(), "a.wav", 1.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": a}));
    ok(&mut s, json!({"op": "import", "path": a, "at": 3}));
    // Mostly silence; a 2 s gap between the tones and nothing after.
    let r = ok(&mut s, json!({"op": "find_silences", "min_secs": 0.5}));
    let ranges = r["ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 1, "{r}");
    assert!((ranges[0]["start"]["seconds"].as_f64().unwrap() - 1.0).abs() < 0.01);
    assert!((ranges[0]["length"]["seconds"].as_f64().unwrap() - 2.0).abs() < 0.01);
    let r = ok(&mut s, json!({"op": "find_silences", "min_secs": 2.5}));
    assert!(r["ranges"].as_array().unwrap().is_empty());
}

#[test]
fn waveform_image_with_markers() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let a = tone(dir.path(), "a.wav", 4.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": a}));
    ok(&mut s, json!({"op": "add_marker", "at": 1, "name": "One"}));
    let r = ok(
        &mut s,
        json!({"op": "waveform", "width": 400, "height": 120}),
    );
    assert_eq!(r["type"], "waveform");
    assert_eq!(r["markers"][0]["x"], 100);
    assert_eq!(r["seconds_per_pixel"], 0.01);
    let png = base64::engine::general_purpose::STANDARD
        .decode(r["png_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[1..4], b"PNG");
    // Sizes are kept within limits.
    let r = ok(
        &mut s,
        json!({"op": "waveform", "width": 5, "start": 1, "end": 2}),
    );
    assert_eq!(r["width"], 200);
    assert!(r["markers"].as_array().unwrap().len() == 1);
}

#[test]
fn sessions_wrap_existing_documents_with_labelled_steps() {
    let dir = tempfile::tempdir().unwrap();
    let a = tone(dir.path(), "a.wav", 2.0);
    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": a}));
    ok(&mut s, json!({"op": "split", "at": 1}));
    ok(&mut s, json!({"op": "copy", "start": 0, "end": 0.5}));
    let clip = s.take_clipboard();
    assert!(clip.is_some());

    // Continue the same document elsewhere, with labelled steps.
    let mut t = Session::from_document(s.into_document(), None);
    t.set_label_prefix("AI: ");
    t.set_clipboard(clip);
    ok(&mut t, json!({"op": "paste", "at": 2}));
    ok(&mut t, json!({"op": "add_marker", "at": 1, "name": "M"}));
    let h = ok(&mut t, json!({"op": "history"}));
    assert_eq!(
        h["undo"],
        json!([
            "Split at 0:01.000",
            "AI: Paste 0:00.500 at 0:02.000",
            "AI: Add marker \"M\" at 0:01.000"
        ])
    );
}

/// Unedited 16-bit audio exported at 16 bits comes back bit-identical, even
/// with strong noise shaping on; once its level changes it is dithered.
#[test]
fn unedited_16_bit_audio_exports_bit_exact() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.wav");
    let ch: Vec<f32> = (0..SR as usize * 2)
        .map(|i| (i as f32 * 0.031).sin() * 0.3)
        .collect();
    let mut silent_then_tone = vec![0.0; SR as usize];
    silent_then_tone.extend(&ch[..SR as usize]);
    io::write_wav(
        &src,
        &AudioBuffer::new(SR, vec![ch, silent_then_tone]),
        ExportSettings {
            format: io::SampleFormat::Int16,
            dither: Dither::None,
            ..Default::default()
        },
    )
    .unwrap();
    let source = io::decode_file(&src).unwrap();

    let mut s = Session::new(None);
    ok(&mut s, json!({"op": "open", "path": src}));
    let out = dir.path().join("out.wav");
    let export = |s: &mut Session| {
        ok(
            s,
            json!({"op": "export", "path": out, "kind": "mix", "preset": "cd",
                   "dither": "strong", "force": true}),
        );
        io::decode_file(&out).unwrap()
    };
    let back = export(&mut s);
    for c in 0..2 {
        assert_eq!(back.channel(c), source.channel(c), "channel {c} changed");
    }

    // A level change takes the audio off the grid: now it gets dither,
    // but the silent second of channel 2 stays silent.
    ok(&mut s, json!({"op": "gain", "db": -0.5}));
    let back = export(&mut s);
    let lsb = 1.0 / 32768.0;
    let g = 10f32.powf(-0.5 / 20.0);
    let differs = back
        .channel(0)
        .iter()
        .zip(source.channel(0))
        .filter(|(b, s)| ((*b - *s * g) / lsb).abs() > 0.51)
        .count();
    assert!(differs > 1000, "dithered: {differs}");
    assert!(back.channel(1)[..SR as usize - 512]
        .iter()
        .all(|&v| v == 0.0));
}
