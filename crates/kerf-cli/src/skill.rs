//! `kerf-cli skill`: the guide that teaches AI agents (such as Claude Code)
//! to use `kerf-cli`, built into the binary so every package carries it.
//!
//! Installing it writes the guide with the command that runs `kerf-cli` on
//! this computer: plain `kerf-cli` when it's on the PATH, the AppImage with
//! `kerf-cli` as its first argument, or the full path inside Kerf.app or the
//! Windows install folder.

use std::path::{Path, PathBuf};

/// The guide as kept in the repository (`skills/kerf/SKILL.md`).
const SKILL: &str = include_str!("../../../skills/kerf/SKILL.md");
const BEGIN: &str = "<!-- command -->\n";
const END: &str = "<!-- /command -->\n";

/// How to run this `kerf-cli` from a shell.
pub fn command() -> String {
    // Inside an AppImage the runtime names the image file.
    if let Some(image) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        return format!("{} kerf-cli", quote(Path::new(&image)));
    }
    let Ok(exe) = std::env::current_exe() else {
        return "kerf-cli".into();
    };
    // Compare canonical paths, but show the plain one (on Windows the
    // canonical form is a `\\?\` path that shells don't take).
    let canonical = exe.canonicalize().ok();
    if canonical.is_some() && on_path().and_then(|p| p.canonicalize().ok()) == canonical {
        "kerf-cli".into()
    } else {
        quote(&exe)
    }
}

/// The `kerf-cli` a shell would run, if any.
fn on_path() -> Option<PathBuf> {
    let name = format!("kerf-cli{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

fn quote(p: &Path) -> String {
    let s = p.display().to_string();
    if s.contains([' ', '\'', '"', '(', ')', '&']) {
        format!("\"{s}\"")
    } else {
        s
    }
}

/// The guide, telling the agent to run `kerf-cli` as `command`.
pub fn text(command: &str) -> String {
    let (Some(a), Some(b)) = (SKILL.find(BEGIN), SKILL.find(END)) else {
        return SKILL.to_string();
    };
    let how = if command == "kerf-cli" {
        "`kerf-cli` is on the PATH here: check it with `kerf-cli --version`.\n".to_string()
    } else {
        format!(
            "On this computer, run it as `{command}`: wherever this guide says `kerf-cli`, use \
             that instead (check it with `{command} --version`).\n"
        )
    };
    format!("{}{how}{}", &SKILL[..a], &SKILL[b + END.len()..])
}

/// Claude Code's folder for personal skills.
pub fn default_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("skills")
            .join("kerf"),
    )
}

/// Write the guide as `SKILL.md` in `dir`. An existing different guide is
/// only replaced with `force`. Returns the file written.
pub fn install(dir: &Path, force: bool) -> Result<PathBuf, String> {
    let file = dir.join("SKILL.md");
    let text = text(&command());
    match std::fs::read_to_string(&file) {
        Ok(old) if old == text => return Ok(file),
        Ok(_) if !force => {
            return Err(format!(
                "{} exists (use --force to replace it)",
                file.display()
            ))
        }
        _ => {}
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_replaces_the_placeholder() {
        let t = text("/opt/Kerf.AppImage kerf-cli");
        assert!(t.starts_with("---\nname: kerf\n"));
        assert!(t.contains("run it as `/opt/Kerf.AppImage kerf-cli`"));
        assert!(!t.contains(BEGIN) && !t.contains(END));
        assert!(!t.contains("skill install"), "no need to install it again");
        assert!(text("kerf-cli").contains("on the PATH here"));
    }
}
