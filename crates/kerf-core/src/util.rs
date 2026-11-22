//! Small numeric and formatting helpers.

/// Convert decibels to a linear amplitude factor.
pub fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Convert a linear amplitude factor to decibels (`-inf` for 0).
pub fn gain_to_db(gain: f64) -> f64 {
    if gain <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * gain.log10()
    }
}

/// Format a frame position as `h:mm:ss.mmm` (hours omitted when zero).
pub fn format_time(frames: u64, sample_rate: u32) -> String {
    let sr = sample_rate.max(1) as u64;
    let total_ms = frames * 1000 / sr;
    let ms = total_ms % 1000;
    let s = (total_ms / 1000) % 60;
    let m = (total_ms / 60_000) % 60;
    let h = total_ms / 3_600_000;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

/// How times are shown and typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TimeFormat {
    /// `1:23.456` (hours added when needed).
    #[default]
    MinSec,
    /// `83.456`
    Seconds,
    /// Sample frames: `4006272`.
    Samples,
    /// CD frames, 75 per second: `01:23:34` (mm:ss:ff).
    CdFrames,
}

impl TimeFormat {
    pub const ALL: [TimeFormat; 4] = [Self::MinSec, Self::Seconds, Self::Samples, Self::CdFrames];

    pub fn label(self) -> &'static str {
        match self {
            Self::MinSec => "Minutes:seconds (m:ss.mmm)",
            Self::Seconds => "Seconds",
            Self::Samples => "Samples",
            Self::CdFrames => "CD frames (mm:ss:ff)",
        }
    }
}

/// Format `frames` in the given format.
pub fn format_time_as(frames: u64, sample_rate: u32, fmt: TimeFormat) -> String {
    let sr = sample_rate.max(1) as u64;
    match fmt {
        TimeFormat::MinSec => format_time(frames, sample_rate),
        TimeFormat::Seconds => format!("{}.{:03}", frames / sr, frames % sr * 1000 / sr),
        TimeFormat::Samples => frames.to_string(),
        TimeFormat::CdFrames => {
            let cd = frames * 75 / sr;
            format!("{:02}:{:02}:{:02}", cd / (75 * 60), (cd / 75) % 60, cd % 75)
        }
    }
}

/// Parse a typed time. Accepts the format's own notation and also plain
/// forms: `83.5` (seconds; samples in [`TimeFormat::Samples`]), `1:23.5`
/// (m:ss), `1:02:03.5` (h:mm:ss; mm:ss:ff in [`TimeFormat::CdFrames`]).
pub fn parse_time(text: &str, sample_rate: u32, fmt: TimeFormat) -> Option<u64> {
    let t = text.trim();
    if t.is_empty() || t.starts_with('-') {
        return None;
    }
    let sr = sample_rate as f64;
    let secs_to_frames = |s: f64| (s.is_finite() && s >= 0.0).then(|| (s * sr).round() as u64);
    let parts: Vec<&str> = t.split(':').collect();
    let num = |s: &str| s.trim().parse::<f64>().ok().filter(|v| *v >= 0.0);
    match parts.as_slice() {
        [one] => {
            if fmt == TimeFormat::Samples && !one.contains('.') {
                one.trim().parse::<u64>().ok()
            } else {
                secs_to_frames(num(one)?)
            }
        }
        [m, s] => secs_to_frames(num(m)? * 60.0 + num(s)?),
        [a, b, c] if fmt == TimeFormat::CdFrames => {
            let (m, s, f) = (num(a)?, num(b)?, num(c)?);
            Some(((m * 60.0 + s) * sr).round() as u64 + (f * sr / 75.0).round() as u64)
        }
        [h, m, s] => secs_to_frames(num(h)? * 3600.0 + num(m)? * 60.0 + num(s)?),
        _ => None,
    }
}

/// Make a string safe to use as a file name on Windows, macOS and Linux.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').to_string();
    if trimmed.is_empty() {
        "untitled".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        for db in [-60.0, -6.0, 0.0, 3.5, 12.0] {
            assert!((gain_to_db(db_to_gain(db)) - db).abs() < 1e-9);
        }
        assert_eq!(gain_to_db(0.0), f64::NEG_INFINITY);
    }

    #[test]
    fn time_format() {
        assert_eq!(format_time(48_000 * 61 + 24_000, 48_000), "1:01.500");
        assert_eq!(format_time(48_000 * 3600, 48_000), "1:00:00.000");
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_file_name("a/b:c?"), "a_b_c_");
        assert_eq!(sanitize_file_name("  ."), "untitled");
    }

    #[test]
    fn time_formats_roundtrip() {
        let sr = 44_100;
        let f = 44_100 * 83 + 20_286; // 83.46 s
        assert_eq!(format_time_as(f, sr, TimeFormat::MinSec), "1:23.460");
        assert_eq!(format_time_as(f, sr, TimeFormat::Seconds), "83.460");
        assert_eq!(format_time_as(f, sr, TimeFormat::Samples), f.to_string());
        assert_eq!(format_time_as(f, sr, TimeFormat::CdFrames), "01:23:34");
        for fmt in TimeFormat::ALL {
            let back = parse_time(&format_time_as(f, sr, fmt), sr, fmt).unwrap();
            // Display precision: 1 ms, or 1/75 s for CD frames.
            let tol = if fmt == TimeFormat::CdFrames {
                sr as u64 / 75
            } else {
                sr as u64 / 1000
            };
            assert!(back.abs_diff(f) <= tol, "{fmt:?}: {back} vs {f}");
        }
    }

    #[test]
    fn parsing_accepts_plain_forms() {
        let sr = 48_000;
        assert_eq!(parse_time("1.5", sr, TimeFormat::MinSec), Some(72_000));
        assert_eq!(
            parse_time(" 2:00 ", sr, TimeFormat::MinSec),
            Some(48_000 * 120)
        );
        assert_eq!(
            parse_time("1:00:00", sr, TimeFormat::MinSec),
            Some(48_000 * 3600)
        );
        assert_eq!(
            parse_time("1:00:00", sr, TimeFormat::CdFrames),
            Some(48_000 * 60),
            "mm:ss:ff"
        );
        assert_eq!(
            parse_time("00:01:15", sr, TimeFormat::CdFrames),
            Some(48_000 + 9600)
        );
        assert_eq!(parse_time("1234", sr, TimeFormat::Samples), Some(1234));
        assert_eq!(
            parse_time("1.5", sr, TimeFormat::Samples),
            Some(72_000),
            "decimals are seconds"
        );
        for bad in ["", "-1", "abc", "1:2:3:4", "1::2"] {
            assert_eq!(parse_time(bad, sr, TimeFormat::MinSec), None, "{bad:?}");
        }
    }
}
