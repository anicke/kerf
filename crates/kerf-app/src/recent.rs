//! Recently opened and saved projects: the list (kept in preferences), and
//! the quick-open dialog (Ctrl+Shift+O): type to filter, arrows to choose,
//! Enter to open.

use std::path::{Path, PathBuf};

use eframe::egui;

/// How many projects are remembered.
pub const MAX: usize = 10;

/// Put `path` first in `list` (once), keeping at most [`MAX`] entries.
pub fn remember(list: &mut Vec<PathBuf>, path: &Path) {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    list.retain(|p| p != &path);
    list.insert(0, path);
    list.truncate(MAX);
}

/// File name without extension, for display.
pub fn name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The folder, with the home folder shortened to `~`.
fn folder(path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new(""));
    match dirs::home_dir().and_then(|h| dir.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~/{}", rel.display()),
        None => dir.display().to_string(),
    }
}

/// Entries matching `filter` (case-insensitive, in the name or folder).
pub fn matching<'a>(list: &'a [PathBuf], filter: &str) -> Vec<&'a PathBuf> {
    let f = filter.trim().to_lowercase();
    list.iter()
        .filter(|p| f.is_empty() || p.display().to_string().to_lowercase().contains(&f))
        .collect()
}

#[derive(Default)]
pub struct QuickOpen {
    filter: String,
    selected: usize,
    focused: bool,
}

pub enum Outcome {
    None,
    Open(PathBuf),
    Close,
    Clear,
}

impl QuickOpen {
    pub fn ui(&mut self, ui: &mut egui::Ui, list: &[PathBuf]) -> Outcome {
        if list.is_empty() {
            ui.label("No recent projects yet. Projects you open or save appear here.");
            return if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                Outcome::Close
            } else {
                Outcome::None
            };
        }
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Type to filter…")
                .desired_width(f32::INFINITY),
        );
        if !self.focused {
            edit.request_focus();
            self.focused = true;
        }
        if edit.changed() {
            self.selected = 0;
        }
        let shown = matching(list, &self.filter);
        let (up, down, enter, esc) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if esc {
            return Outcome::Close;
        }
        if down {
            self.selected = (self.selected + 1).min(shown.len().saturating_sub(1));
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        let mut out = Outcome::None;
        if enter {
            if let Some(p) = shown.get(self.selected).filter(|p| p.exists()) {
                out = Outcome::Open((*p).clone());
            }
        }

        ui.add_space(4.0);
        if shown.is_empty() {
            ui.label(egui::RichText::new("No matches").weak());
        }
        for (i, path) in shown.iter().enumerate() {
            let exists = path.exists();
            let mut title = egui::RichText::new(name(path)).strong();
            let mut place = egui::RichText::new(folder(path)).weak().small();
            if !exists {
                title = title.strikethrough().weak();
                place = egui::RichText::new(format!("{} (not found)", folder(path)))
                    .weak()
                    .small();
            }
            let resp = ui
                .add_enabled_ui(exists, |ui| ui.selectable_label(i == self.selected, title))
                .inner
                .on_hover_text(path.display().to_string());
            ui.label(place);
            if resp.clicked() {
                out = Outcome::Open((*path).clone());
            }
            if i == self.selected && (up || down) {
                resp.scroll_to_me(None);
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("↑↓ choose · Enter open · Esc close")
                    .weak()
                    .small(),
            );
            if ui.small_button("Clear list").clicked() {
                out = Outcome::Clear;
            }
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_first_without_duplicates() {
        let mut l = Vec::new();
        for p in ["/a.kerf", "/b.kerf", "/a.kerf"] {
            remember(&mut l, Path::new(p));
        }
        assert_eq!(l, [PathBuf::from("/a.kerf"), PathBuf::from("/b.kerf")]);
        for i in 0..20 {
            remember(&mut l, Path::new(&format!("/p{i}.kerf")));
        }
        assert_eq!(l.len(), MAX);
        assert_eq!(l[0], PathBuf::from("/p19.kerf"));
    }

    #[test]
    fn same_file_by_another_path_is_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("show.kerf");
        std::fs::write(&p, "{}").unwrap();
        let mut l = Vec::new();
        remember(&mut l, &p);
        remember(&mut l, &dir.path().join(".").join("show.kerf"));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn filter_matches_name_or_folder() {
        let l = vec![
            PathBuf::from("/music/live/Show 2024.kerf"),
            PathBuf::from("/podcasts/ep1.kerf"),
        ];
        assert_eq!(matching(&l, "show").len(), 1);
        assert_eq!(matching(&l, "PODCASTS").len(), 1);
        assert_eq!(matching(&l, "").len(), 2);
        assert_eq!(matching(&l, "nothing").len(), 0);
    }
}
