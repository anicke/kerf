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
}
