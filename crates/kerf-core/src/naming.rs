//! File names for exported tracks from a template such as
//! `{nn} - {title}` or `{artist} - {album}/{nn} {title}`.
//!
//! `/` in the template makes folders. Field values are made safe for every
//! file system, and separators left dangling by empty fields are tidied away
//! (`{artist} - {title}` without an artist gives just the title).

use std::path::PathBuf;

/// The default for one file per track.
pub const DEFAULT_TRACK_NAMES: &str = "{nn} - {title}";

/// Every placeholder with what it stands for.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{nn}", "track number, two digits (01)"),
    ("{num}", "track number (1)"),
    ("{title}", "track title (the marker name)"),
    ("{artist}", "track artist, or else the album artist"),
    ("{album}", "album title"),
    ("{year}", "album year"),
    ("{genre}", "album genre"),
    ("{project}", "project file name"),
];

/// Values for one track.
#[derive(Clone, Debug, Default)]
pub struct Fields<'a> {
    /// 1-based.
    pub number: usize,
    pub total: usize,
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub year: &'a str,
    pub genre: &'a str,
    pub project: &'a str,
}

/// Check a template: known placeholders, balanced braces, no empty or `..`
/// folder names, and a name for the file itself.
pub fn check(template: &str) -> Result<(), String> {
    expand(template, &Fields::default()).map(|_| ())
}

/// The relative path (without extension) for one track.
pub fn expand(template: &str, f: &Fields) -> Result<PathBuf, String> {
    let template = template.trim();
    if template.is_empty() {
        return Err("the name is empty".into());
    }
    let parts: Vec<&str> = template.split(['/', '\\']).collect();
    let mut path = PathBuf::new();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if part.trim() == ".." || part.trim() == "." {
            return Err("folder names can't be . or ..".into());
        }
        let name = tidy(&fill(part, f)?);
        if name.is_empty() {
            if last {
                // e.g. "{title}" for an unnamed marker.
                path.push(format!("Track {}", number(f, true)));
            } else if part.contains('{') {
                // A folder made only of empty fields: leave it out.
                continue;
            } else {
                return Err("a folder name is empty (two / in a row?)".into());
            }
        } else {
            path.push(name);
        }
    }
    Ok(path)
}

fn number(f: &Fields, padded: bool) -> String {
    if padded {
        let width = f.total.to_string().len().max(2);
        format!("{:0width$}", f.number)
    } else {
        f.number.to_string()
    }
}

/// Replace the placeholders in one path component.
fn fill(part: &str, f: &Fields) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = part;
    while let Some(open) = rest.find(['{', '}']) {
        out.push_str(&rest[..open]);
        if rest[open..].starts_with('}') {
            return Err("a } without a {".into());
        }
        let Some(close) = rest[open..].find('}') else {
            return Err("a { without a }".into());
        };
        let name = &rest[open + 1..open + close];
        let value = match name {
            "nn" => number(f, true),
            "num" => number(f, false),
            "title" => safe(f.title),
            "artist" => safe(f.artist),
            "album" => safe(f.album),
            "year" => safe(f.year),
            "genre" => safe(f.genre),
            "project" => safe(f.project),
            _ => {
                return Err(format!(
                    "unknown field {{{name}}}; use {}",
                    PLACEHOLDERS
                        .iter()
                        .map(|(p, _)| *p)
                        .collect::<Vec<_>>()
                        .join(" ")
                ))
            }
        };
        out.push_str(&value);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    // Characters typed into the template itself must be safe too.
    Ok(out
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect())
}

/// A field value safe inside one file name ("" stays "").
fn safe(v: &str) -> String {
    if v.trim().is_empty() {
        String::new()
    } else {
        crate::util::sanitize_file_name(v)
    }
}

/// Tidy what empty fields leave behind: doubled separators, empty brackets,
/// separators at the ends, a trailing dot.
fn tidy(s: &str) -> String {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = squash(s);
    loop {
        let before = s.clone();
        for (from, to) in [
            ("()", ""),
            ("[]", ""),
            ("( )", ""),
            ("[ ]", ""),
            (" - - ", " - "),
            (" – – ", " – "),
            ("__", "_"),
        ] {
            s = s.replace(from, to);
        }
        s = squash(&s)
            .trim_matches(|c: char| c.is_whitespace() || "-–_,.".contains(c))
            .to_string();
        if s == before {
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(title: &'a str, artist: &'a str) -> Fields<'a> {
        Fields {
            number: 3,
            total: 12,
            title,
            artist,
            album: "Live at Home",
            year: "2026",
            genre: "",
            project: "show",
        }
    }

    fn name(t: &str, f: &Fields) -> String {
        expand(t, f).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn fills_fields_and_folders() {
        let f = fields("Opener", "The Band");
        assert_eq!(name(DEFAULT_TRACK_NAMES, &f), "03 - Opener");
        assert_eq!(name("{num}. {title}", &f), "3. Opener");
        let p = expand("{artist} - {album} ({year})/{nn} {title}", &f).unwrap();
        assert_eq!(
            p,
            PathBuf::from("The Band - Live at Home (2026)").join("03 Opener")
        );
    }

    #[test]
    fn empty_fields_leave_no_stray_separators() {
        let f = fields("Opener", "");
        assert_eq!(name("{artist} - {title}", &f), "Opener");
        assert_eq!(name("{nn} - {artist} - {title}", &f), "03 - Opener");
        assert_eq!(name("{title} [{genre}]", &f), "Opener");
        assert_eq!(
            name("{genre}/{title}", &f),
            "Opener",
            "empty folder dropped"
        );
        assert_eq!(name("{genre}", &f), "Track 03");
    }

    #[test]
    fn values_cannot_make_folders_or_bad_names() {
        let f = fields("AC/DC: Live?", "");
        assert_eq!(name("{title}", &f), "AC_DC_ Live");
        assert_eq!(name("Show: {title}", &f), "Show_ AC_DC_ Live");
    }

    #[test]
    fn three_digit_numbers_when_needed() {
        let f = Fields {
            number: 7,
            total: 120,
            title: "x",
            ..Default::default()
        };
        assert_eq!(name("{nn}", &f), "007");
    }

    #[test]
    fn bad_templates_are_explained() {
        assert!(check("{nn} - {titel}").unwrap_err().contains("{titel}"));
        assert!(check("{nn").is_err());
        assert!(check("nn}").is_err());
        assert!(check("../{title}").is_err());
        assert!(check("a//{title}").is_err());
        assert!(check("  ").is_err());
        assert!(check("{album}/{nn} {title}").is_ok());
    }
}
