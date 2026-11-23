//! Transport buttons (go to start, play/stop, go to end, loop) with icons
//! drawn as shapes.
//!
//! Font glyphs such as ⏮ ▶ ⏹ come from different fallback fonts with
//! different metrics, so they sit at different heights and ▶ and ⏹ differ in
//! width (the play button jumped when toggled). Painted icons in buttons of
//! one fixed size line up exactly and never change size.

use eframe::egui::{self, pos2, vec2, Color32, Pos2, Rect, Sense, Shape, Stroke};

/// Width of every transport button, and the smallest height.
pub const WIDTH: f32 = 34.0;
pub const MIN_HEIGHT: f32 = 26.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    GoStart,
    Play,
    Stop,
    GoEnd,
    Loop,
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
        paint(painter, rect.center(), icon, v.fg_stroke.color);
    }
    resp
}

fn triangle(c: Pos2, half: f32, right: bool) -> Vec<Pos2> {
    // Equilateral-ish, optically centred (shifted toward the flat side).
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

fn paint(p: &egui::Painter, c: Pos2, icon: Icon, color: Color32) {
    let fill = |pts: Vec<Pos2>| Shape::convex_polygon(pts, color, Stroke::NONE);
    let half = 6.0;
    match icon {
        Icon::Play => {
            p.add(fill(triangle(c, half, true)));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(10.0, 10.0)), 1.5, color);
        }
        Icon::GoStart | Icon::GoEnd => {
            let right = icon == Icon::GoEnd;
            let dir = if right { 1.0 } else { -1.0 };
            // The triangle and bar together are centred.
            let tri_c = pos2(c.x - dir * 1.5, c.y);
            p.add(fill(triangle(tri_c, 5.0, right)));
            let bar_x = c.x + dir * 4.5;
            p.rect_filled(
                Rect::from_center_size(pos2(bar_x, c.y), vec2(2.0, 10.0)),
                0.5,
                color,
            );
        }
        Icon::Loop => {
            // A rounded loop with an arrowhead on its top edge.
            let r = Rect::from_center_size(c, vec2(14.0, 9.0));
            let stroke = Stroke::new(1.8, color);
            p.rect_stroke(r, 3.0, stroke, egui::StrokeKind::Middle);
            let tip = pos2(c.x + 2.5, r.top());
            p.add(fill(vec![
                pos2(tip.x - 3.5, tip.y - 3.5),
                pos2(tip.x + 0.5, tip.y),
                pos2(tip.x - 3.5, tip.y + 3.5),
            ]));
        }
    }
}
