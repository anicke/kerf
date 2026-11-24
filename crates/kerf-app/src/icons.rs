//! Toolbar buttons with icons drawn as shapes.
//!
//! Font glyphs such as ⏮ ▶ ⏹ 🔍 come from different fallback fonts with
//! different metrics, so they sit at different heights and differ in width
//! (the play button jumped when toggled). Painted icons in buttons of one
//! fixed size line up exactly and never change size.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke};

/// Width of every toolbar button, and the smallest height.
pub const WIDTH: f32 = 34.0;
pub const MIN_HEIGHT: f32 = 26.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    // Transport
    GoStart,
    Play,
    Stop,
    GoEnd,
    Loop,
    // Edit
    Undo,
    Redo,
    Split,
    Cut,
    Copy,
    Paste,
    Delete,
    // Processing
    FadeIn,
    FadeOut,
    Normalize,
    Gain,
    // View
    Snap,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomSelection,
    Follow,
    Center,
    DbScale,
    ZeroCrossings,
    MarkersPanel,
    HistoryPanel,
}

/// A fixed-size button showing `icon`, `height` tall (to match its
/// neighbours); `selected` draws it as switched on.
pub fn button(ui: &mut egui::Ui, icon: Icon, selected: bool, height: f32) -> egui::Response {
    let size = vec2(WIDTH, height.max(MIN_HEIGHT));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let v = ui.style().interact_selectable(&resp, selected);
        let painter = ui.painter();
        painter.rect(
            rect,
            v.corner_radius,
            v.weak_bg_fill,
            v.bg_stroke,
            egui::StrokeKind::Inside,
        );
        paint(
            painter,
            rect.center(),
            icon,
            v.fg_stroke.color,
            v.weak_bg_fill,
        );
    }
    resp
}

/// A toolbar button with a tooltip, greyed out when not `enabled`.
/// Returns whether it was clicked.
pub fn tool(
    ui: &mut egui::Ui,
    icon: Icon,
    selected: bool,
    enabled: bool,
    height: f32,
    tip: &str,
) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        button(ui, icon, selected, height)
            .on_hover_text(tip)
            .on_disabled_hover_text(tip)
            .clicked()
    })
    .inner
}

fn triangle(c: Pos2, half: f32, right: bool) -> Vec<Pos2> {
    // Optically centred: shifted toward the flat side.
    let w = half * 1.6;
    let (tip, back) = if right {
        (c.x + w * 0.6, c.x - w * 0.4)
    } else {
        (c.x - w * 0.6, c.x + w * 0.4)
    };
    vec![
        pos2(back, c.y - half),
        pos2(tip, c.y),
        pos2(back, c.y + half),
    ]
}

/// Points along a circular arc (angles in degrees, 0 = right, 90 = down).
fn arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let n = 16;
    (0..=n)
        .map(|i| {
            let a = (from + (to - from) * i as f32 / n as f32).to_radians();
            pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect()
}

fn paint(p: &egui::Painter, c: Pos2, icon: Icon, color: Color32, bg: Color32) {
    let fill = |pts: Vec<Pos2>| Shape::convex_polygon(pts, color, Stroke::NONE);
    let line = |pts: Vec<Pos2>, w: f32| Shape::line(pts, Stroke::new(w, color));
    let thin = Stroke::new(1.6, color);
    match icon {
        Icon::Play => {
            p.add(fill(triangle(c, 6.0, true)));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(10.0, 10.0)), 1.5, color);
        }
        Icon::GoStart | Icon::GoEnd => {
            let right = icon == Icon::GoEnd;
            let dir = if right { 1.0 } else { -1.0 };
            p.add(fill(triangle(pos2(c.x - dir * 1.5, c.y), 5.0, right)));
            p.rect_filled(
                Rect::from_center_size(pos2(c.x + dir * 4.5, c.y), vec2(2.0, 10.0)),
                0.5,
                color,
            );
        }
        Icon::Loop => {
            let r = Rect::from_center_size(c, vec2(14.0, 9.0));
            p.rect_stroke(r, 3.0, Stroke::new(1.8, color), egui::StrokeKind::Middle);
            let tip = pos2(c.x + 2.5, r.top());
            p.add(fill(vec![
                pos2(tip.x - 3.5, tip.y - 3.5),
                pos2(tip.x + 0.5, tip.y),
                pos2(tip.x - 3.5, tip.y + 3.5),
            ]));
        }
        Icon::Undo | Icon::Redo => {
            // A hook: around the right (left for redo) and back along the top.
            let d = if icon == Icon::Undo { 1.0 } else { -1.0 };
            let centre = pos2(c.x + d * 1.5, c.y + 1.0);
            let mut pts = if d > 0.0 {
                arc(centre, 4.5, 90.0, -90.0)
            } else {
                arc(centre, 4.5, 90.0, 270.0)
            };
            pts.insert(0, pos2(c.x - d * 2.0, c.y + 5.5));
            let top = pts.last().copied().unwrap();
            let end = pos2(c.x - d * 4.0, top.y);
            pts.push(end);
            p.add(line(pts, 1.8));
            p.add(fill(vec![
                pos2(end.x + d * 0.5, end.y - 3.5),
                pos2(end.x - d * 3.5, end.y),
                pos2(end.x + d * 0.5, end.y + 3.5),
            ]));
        }
        Icon::Split => {
            p.rect_filled(
                Rect::from_center_size(pos2(c.x - 4.5, c.y), vec2(4.0, 8.0)),
                1.0,
                color,
            );
            p.rect_filled(
                Rect::from_center_size(pos2(c.x + 4.5, c.y), vec2(4.0, 8.0)),
                1.0,
                color,
            );
            p.add(line(vec![pos2(c.x, c.y - 7.0), pos2(c.x, c.y + 7.0)], 1.5));
        }
        Icon::Cut => {
            let (l, r) = (pos2(c.x - 3.2, c.y + 4.0), pos2(c.x + 3.2, c.y + 4.0));
            p.circle_stroke(l, 2.2, thin);
            p.circle_stroke(r, 2.2, thin);
            p.add(line(
                vec![pos2(l.x + 1.2, l.y - 2.0), pos2(c.x + 3.5, c.y - 6.5)],
                1.6,
            ));
            p.add(line(
                vec![pos2(r.x - 1.2, r.y - 2.0), pos2(c.x - 3.5, c.y - 6.5)],
                1.6,
            ));
        }
        Icon::Copy => {
            let back = Rect::from_center_size(pos2(c.x - 1.8, c.y - 1.8), vec2(8.0, 9.0));
            let front = back.translate(vec2(3.6, 3.6));
            p.rect_stroke(back, 1.5, thin, egui::StrokeKind::Middle);
            p.rect(front, 1.5, bg, thin, egui::StrokeKind::Middle);
        }
        Icon::Paste => {
            let board = Rect::from_center_size(pos2(c.x, c.y + 0.5), vec2(10.0, 12.0));
            p.rect_stroke(board, 1.5, thin, egui::StrokeKind::Middle);
            p.rect_filled(
                Rect::from_center_size(pos2(c.x, board.top()), vec2(5.0, 3.0)),
                1.0,
                color,
            );
            for dy in [0.0, 3.0] {
                p.add(line(
                    vec![pos2(c.x - 2.5, c.y + dy), pos2(c.x + 2.5, c.y + dy)],
                    1.2,
                ));
            }
        }
        Icon::Delete => {
            // A bin.
            p.add(line(
                vec![pos2(c.x - 5.5, c.y - 4.5), pos2(c.x + 5.5, c.y - 4.5)],
                1.6,
            ));
            p.add(line(
                vec![pos2(c.x - 1.8, c.y - 6.5), pos2(c.x + 1.8, c.y - 6.5)],
                1.6,
            ));
            p.add(Shape::closed_line(
                vec![
                    pos2(c.x - 4.0, c.y - 2.5),
                    pos2(c.x + 4.0, c.y - 2.5),
                    pos2(c.x + 3.0, c.y + 6.0),
                    pos2(c.x - 3.0, c.y + 6.0),
                ],
                thin,
            ));
        }
        Icon::FadeIn | Icon::FadeOut => {
            let (lo, hi) = (c.y + 5.5, c.y - 5.5);
            let pts = if icon == Icon::FadeIn {
                vec![
                    pos2(c.x - 6.5, lo),
                    pos2(c.x + 6.5, lo),
                    pos2(c.x + 6.5, hi),
                ]
            } else {
                vec![
                    pos2(c.x - 6.5, hi),
                    pos2(c.x + 6.5, lo),
                    pos2(c.x - 6.5, lo),
                ]
            };
            p.add(fill(pts));
        }
        Icon::Normalize => {
            // Bars brought up to a level line.
            p.add(line(
                vec![pos2(c.x - 7.0, c.y - 6.0), pos2(c.x + 7.0, c.y - 6.0)],
                1.2,
            ));
            for dx in [-4.5, 0.0, 4.5] {
                p.rect_filled(
                    Rect::from_min_max(
                        pos2(c.x + dx - 1.3, c.y - 3.5),
                        pos2(c.x + dx + 1.3, c.y + 6.0),
                    ),
                    0.5,
                    color,
                );
            }
        }
        Icon::Gain => {
            // A fader.
            p.add(line(vec![pos2(c.x, c.y - 7.0), pos2(c.x, c.y + 7.0)], 1.4));
            p.rect_filled(
                Rect::from_center_size(pos2(c.x, c.y + 1.0), vec2(9.0, 4.0)),
                1.0,
                color,
            );
        }
        Icon::Snap => {
            // A magnet: a thick U.
            // Left leg, down around the bottom (180° → 90° → 0°), right leg.
            let mut pts = vec![pos2(c.x - 4.5, c.y - 6.0)];
            pts.extend(arc(pos2(c.x, c.y), 4.5, 180.0, 0.0));
            pts.push(pos2(c.x + 4.5, c.y - 6.0));
            p.add(line(pts, 3.0));
            // A gap across both legs marks the poles, so it reads as a magnet.
            for x in [c.x - 4.5, c.x + 4.5] {
                p.line_segment(
                    [pos2(x - 2.0, c.y - 3.2), pos2(x + 2.0, c.y - 3.2)],
                    Stroke::new(1.2, bg),
                );
            }
        }
        Icon::ZoomIn | Icon::ZoomOut => {
            let lens = pos2(c.x - 1.5, c.y - 1.5);
            p.circle_stroke(lens, 4.6, thin);
            p.add(line(
                vec![pos2(lens.x + 3.4, lens.y + 3.4), pos2(c.x + 6.0, c.y + 6.0)],
                2.2,
            ));
            p.add(line(
                vec![pos2(lens.x - 2.3, lens.y), pos2(lens.x + 2.3, lens.y)],
                1.4,
            ));
            if icon == Icon::ZoomIn {
                p.add(line(
                    vec![pos2(lens.x, lens.y - 2.3), pos2(lens.x, lens.y + 2.3)],
                    1.4,
                ));
            }
        }
        Icon::ZoomFit => {
            // ↔ between two walls.
            p.add(line(
                vec![pos2(c.x - 7.0, c.y - 5.5), pos2(c.x - 7.0, c.y + 5.5)],
                1.6,
            ));
            p.add(line(
                vec![pos2(c.x + 7.0, c.y - 5.5), pos2(c.x + 7.0, c.y + 5.5)],
                1.6,
            ));
            p.add(line(vec![pos2(c.x - 3.5, c.y), pos2(c.x + 3.5, c.y)], 1.6));
            p.add(fill(triangle(pos2(c.x - 3.5, c.y), 3.0, false)));
            p.add(fill(triangle(pos2(c.x + 3.5, c.y), 3.0, true)));
        }
        Icon::ZoomSelection => {
            // [ ] around a highlighted span.
            let (t, b) = (c.y - 6.0, c.y + 6.0);
            p.add(line(
                vec![
                    pos2(c.x - 4.0, t),
                    pos2(c.x - 6.5, t),
                    pos2(c.x - 6.5, b),
                    pos2(c.x - 4.0, b),
                ],
                1.6,
            ));
            p.add(line(
                vec![
                    pos2(c.x + 4.0, t),
                    pos2(c.x + 6.5, t),
                    pos2(c.x + 6.5, b),
                    pos2(c.x + 4.0, b),
                ],
                1.6,
            ));
            p.rect_filled(Rect::from_center_size(c, vec2(6.0, 6.0)), 1.0, color);
        }
        Icon::Follow => {
            // A playhead moving right.
            p.add(line(
                vec![pos2(c.x - 4.0, c.y - 7.0), pos2(c.x - 4.0, c.y + 7.0)],
                1.6,
            ));
            p.add(line(vec![pos2(c.x - 1.0, c.y), pos2(c.x + 5.0, c.y)], 1.6));
            p.add(fill(triangle(pos2(c.x + 4.5, c.y), 3.5, true)));
        }
        Icon::Center => {
            // The playhead fixed in the middle, the audio moving past it.
            p.add(line(vec![pos2(c.x, c.y - 7.0), pos2(c.x, c.y + 7.0)], 1.6));
            for d in [-1.0, 1.0] {
                p.add(fill(vec![
                    pos2(c.x + d * 7.0, c.y - 3.5),
                    pos2(c.x + d * 3.0, c.y),
                    pos2(c.x + d * 7.0, c.y + 3.5),
                ]));
            }
        }
        Icon::DbScale => {
            p.text(
                c,
                Align2::CENTER_CENTER,
                "dB",
                FontId::proportional(12.0),
                color,
            );
        }
        Icon::ZeroCrossings => {
            // One sine period through a centre line, dot at the crossing.
            p.line_segment(
                [pos2(c.x - 7.5, c.y), pos2(c.x + 7.5, c.y)],
                Stroke::new(1.0, color.gamma_multiply(0.6)),
            );
            let pts: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let t = i as f32 / 24.0;
                    pos2(
                        c.x - 7.0 + 14.0 * t,
                        c.y - 5.0 * (t * std::f32::consts::TAU).sin(),
                    )
                })
                .collect();
            p.add(line(pts, 1.5));
            p.circle_filled(c, 2.0, color);
        }
        Icon::MarkersPanel => {
            // A list of flagged rows.
            for dy in [-4.5, 0.0, 4.5] {
                let y = c.y + dy;
                p.add(fill(vec![
                    pos2(c.x - 6.5, y - 1.8),
                    pos2(c.x - 3.5, y),
                    pos2(c.x - 6.5, y + 1.8),
                ]));
                p.add(line(vec![pos2(c.x - 1.5, y), pos2(c.x + 6.5, y)], 1.5));
            }
        }
        Icon::HistoryPanel => {
            // A clock face with an arrow turning back.
            let r = 6.0;
            let pts: Vec<Pos2> = (0..=20)
                .map(|i| {
                    let a = (-0.75 + 1.5 * i as f32 / 20.0) * std::f32::consts::PI;
                    pos2(c.x + r * a.sin(), c.y - r * a.cos())
                })
                .collect();
            let tail = pts[0];
            p.add(line(pts, 1.5));
            p.add(fill(vec![
                pos2(tail.x - 3.2, tail.y - 1.0),
                pos2(tail.x + 0.8, tail.y - 3.2),
                pos2(tail.x + 1.2, tail.y + 1.4),
            ]));
            p.add(line(
                vec![
                    pos2(c.x, c.y - 3.5),
                    pos2(c.x, c.y),
                    pos2(c.x + 2.5, c.y + 1.5),
                ],
                1.4,
            ));
        }
    }
}
