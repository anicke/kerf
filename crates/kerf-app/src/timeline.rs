//! The multitrack timeline: ruler, track headers, waveform lanes and all
//! mouse interaction (selecting, moving and trimming clips, markers).

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CursorIcon, FontId, Id, Layout, Mesh, Pos2, Rect,
    Sense, Shape, Stroke, UiBuilder,
};
use kerf_core::edit;
use kerf_core::project::{Clip, ClipId, FadeShape, MarkerId, Project};
use kerf_core::util::{db_to_gain, format_time, format_time_as, gain_to_db, TimeFormat};
use kerf_core::{Frame, Samples, TrackId};

use crate::app::{Action, Dialog, KerfApp};

pub const HEADER_W: f32 = 200.0;
const RULER_H: f32 = 28.0;
const SCROLLBAR_H: f32 = 12.0;
const CLIP_HEADER_H: f32 = 15.0;
const EDGE_PX: f32 = 6.0;
const SNAP_PX: f32 = 8.0;
/// Height of the band at the top of a clip's waveform where fade handles live.
const HANDLE_BAND: f32 = 14.0;
const HANDLE_PX: f32 = 7.0;

const WAVE: Color32 = Color32::from_rgb(110, 190, 255);
const WAVE_MUTED: Color32 = Color32::from_rgb(110, 120, 130);
const CLIP_BG: Color32 = Color32::from_rgb(32, 52, 76);
const CLIP_HDR: Color32 = Color32::from_rgb(46, 74, 108);
const CLIP_SEL: Color32 = Color32::from_rgb(255, 200, 80);
const CLIPPED: Color32 = Color32::from_rgb(255, 80, 80);
const SELECTION: Color32 = Color32::from_rgba_premultiplied(60, 90, 130, 70);
const CURSOR: Color32 = Color32::from_rgb(240, 240, 240);
const PLAYHEAD: Color32 = Color32::from_rgb(80, 230, 120);
const MARKER: Color32 = Color32::from_rgb(255, 160, 50);
const FADE: Color32 = Color32::from_rgb(255, 220, 120);
const PREVIEW: Color32 = Color32::from_rgb(90, 220, 255);

#[derive(Clone, Debug)]
pub enum Drag {
    Select {
        anchor: Frame,
        anchor_track: usize,
    },
    RulerSelect {
        anchor: Frame,
    },
    MoveClip {
        clip: ClipId,
        grab: i64,
        track: usize,
        pos: Frame,
    },
    TrimStart {
        clip: ClipId,
        pos: Frame,
    },
    TrimEnd {
        clip: ClipId,
        pos: Frame,
    },
    FadeIn {
        clip: ClipId,
        len: Frame,
        shape: FadeShape,
    },
    FadeOut {
        clip: ClipId,
        len: Frame,
        shape: FadeShape,
    },
    Marker {
        id: MarkerId,
        pos: Frame,
    },
    /// Dragging one edge of the time selection; `anchor` is the other edge.
    SelectionEdge {
        anchor: Frame,
    },
    Scrollbar {
        grab: f32,
    },
    /// Dragging the playhead to hear the audio; `resume` playback afterwards.
    Scrub {
        resume: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Zone {
    Header,
    Body,
    LeftEdge,
    RightEdge,
    FadeIn,
    FadeOut,
}

/// Screen x of a clip's fade-in and fade-out handles (where each fade ends).
fn fade_handle_xs(geo: &Geo, c: &Clip) -> (f32, f32) {
    (
        geo.x((c.position + c.fade_in_frames()) as f64),
        geo.x((c.end() - c.fade_out_frames()) as f64),
    )
}

/// Screen geometry for one frame.
struct Geo {
    ruler: Rect,
    headers: Rect,
    lanes: Rect,
    scrollbar: Rect,
    spp: f64,
    scroll: f64,
    vscroll: f32,
    /// Top of each track from the top of the first, then the total height.
    tops: Vec<f32>,
    /// Amplitude zoom and dB scale for waveforms.
    vzoom: f32,
    db: bool,
}

impl Geo {
    /// Display position (-1..1) for a sample value, with vertical zoom and
    /// optional dB scale (60 dB range).
    fn amp(&self, v: f32) -> f32 {
        let v = v * self.vzoom;
        if self.db {
            let a = v.abs();
            if a <= 1e-6 {
                return 0.0;
            }
            (1.0 + 20.0 * a.log10() / 60.0).clamp(0.0, 1.0).copysign(v)
        } else {
            v.clamp(-1.0, 1.0)
        }
    }

    fn x(&self, frame: f64) -> f32 {
        self.lanes.left() + ((frame - self.scroll) / self.spp) as f32
    }

    fn frame_f(&self, x: f32) -> f64 {
        self.scroll + (x - self.lanes.left()) as f64 * self.spp
    }

    fn frame(&self, x: f32) -> Frame {
        self.frame_f(x).max(0.0).round() as Frame
    }

    /// Screen y of the top of track `i` (`i == tracks` gives the bottom of the last).
    fn row_top(&self, i: usize) -> f32 {
        let y = self
            .tops
            .get(i)
            .or(self.tops.last())
            .copied()
            .unwrap_or(0.0);
        self.lanes.top() + y - self.vscroll
    }

    fn row(&self, i: usize) -> Rect {
        Rect::from_min_max(
            pos2(self.lanes.left(), self.row_top(i)),
            pos2(self.lanes.right(), self.row_top(i + 1)),
        )
    }

    /// Index of the track at screen `y`, or `None` above/below all tracks.
    /// (`n` is the track count, kept for symmetry with the callers.)
    fn track_at(&self, y: f32, n: usize) -> Option<usize> {
        let rel = y - self.lanes.top() + self.vscroll;
        let i = self.tops.partition_point(|&t| t <= rel);
        (rel >= 0.0 && i >= 1 && i - 1 < n).then(|| i - 1)
    }

    fn track_at_clamped(&self, y: f32, n: usize) -> usize {
        let rel = y - self.lanes.top() + self.vscroll;
        let i = self.tops.partition_point(|&t| t <= rel).saturating_sub(1);
        i.min(n.saturating_sub(1))
    }
}

fn hit_clip(p: &Project, geo: &Geo, pos: Pos2) -> Option<(usize, ClipId, Zone)> {
    let ti = geo.track_at(pos.y, p.tracks.len())?;
    let top = geo.row_top(ti);
    for c in p.tracks[ti].clips.iter().rev() {
        let x0 = geo.x(c.position as f64);
        let x1 = geo.x(c.end() as f64);
        if pos.x < x0 - EDGE_PX / 2.0 || pos.x > x1 + EDGE_PX / 2.0 {
            continue;
        }
        let wide = x1 - x0 > EDGE_PX * 3.0;
        let band = top + CLIP_HEADER_H;
        let in_band = pos.y >= band && pos.y < band + HANDLE_BAND;
        let (hx_in, hx_out) = fade_handle_xs(geo, c);
        let zone = if wide && in_band && (pos.x - hx_in).abs() <= HANDLE_PX {
            Zone::FadeIn
        } else if wide && in_band && (pos.x - hx_out).abs() <= HANDLE_PX {
            Zone::FadeOut
        } else if wide && (pos.x - x0).abs() < EDGE_PX {
            Zone::LeftEdge
        } else if wide && (pos.x - x1).abs() < EDGE_PX {
            Zone::RightEdge
        } else if pos.x < x0 || pos.x > x1 {
            continue;
        } else if pos.y < top + CLIP_HEADER_H + 1.0 {
            Zone::Header
        } else {
            Zone::Body
        };
        return Some((ti, c.id, zone));
    }
    None
}

/// Snap `f` to nearby clip edges, markers, the cursor or the selection (within
/// a few pixels). Holding Alt disables snapping.
fn snap(
    app: &KerfApp,
    p: &Project,
    geo: &Geo,
    f: Frame,
    exclude: Option<ClipId>,
    alt: bool,
) -> Frame {
    if !app.view.snap || alt {
        return f;
    }
    let tol = SNAP_PX as f64 * geo.spp;
    let mut best = (tol, f);
    let mut consider = |c: Frame| {
        let d = (c as f64 - f as f64).abs();
        if d <= best.0 {
            best = (d, c);
        }
    };
    consider(0);
    consider(app.cursor);
    if let Some((a, b)) = app.sel.range {
        consider(a);
        consider(b);
    }
    for m in &p.markers {
        consider(m.position);
    }
    for t in &p.tracks {
        for c in t.clips.iter().filter(|c| Some(c.id) != exclude) {
            consider(c.position);
            consider(c.end());
        }
    }
    best.1
}

/// The project as it should be drawn: with an in-progress drag applied.
fn preview(p: &mut Project, drag: &Option<Drag>) {
    let _ = match drag {
        Some(Drag::MoveClip {
            clip, track, pos, ..
        }) => match p.tracks.get(*track).map(|t| t.id) {
            Some(tid) => edit::move_clip(p, *clip, tid, *pos),
            None => Ok(()),
        },
        Some(Drag::TrimStart { clip, pos }) => edit::trim_clip_start(p, *clip, *pos),
        Some(Drag::TrimEnd { clip, pos }) => edit::trim_clip_end(p, *clip, *pos),
        Some(Drag::Marker { id, pos }) => edit::move_marker(p, *id, *pos),
        Some(Drag::FadeIn { clip, len, shape }) => edit::set_clip_fade_in(p, *clip, *len, *shape),
        Some(Drag::FadeOut { clip, len, shape }) => edit::set_clip_fade_out(p, *clip, *len, *shape),
        _ => Ok(()),
    };
}

pub fn show(app: &mut KerfApp, ui: &mut egui::Ui) {
    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let ctx = ui.ctx().clone();
    let lanes = Rect::from_min_max(
        pos2(full.left() + HEADER_W, full.top() + RULER_H),
        pos2(full.right(), full.bottom() - SCROLLBAR_H),
    );
    app.view.lane_width = lanes.width().max(1.0);
    app.view.lane_height = lanes.height().max(1.0);
    let n_tracks = app.project().tracks.len();
    app.update_view_content();

    // --- Scroll / zoom input --------------------------------------------------
    let (scroll_delta, zoom, hover, mods) = ui.input(|i| {
        (
            i.smooth_scroll_delta,
            i.zoom_delta(),
            i.pointer.hover_pos(),
            i.modifiers,
        )
    });
    if let Some(h) = hover.filter(|h| full.contains(*h)) {
        if zoom != 1.0 {
            app.view
                .zoom(1.0 / zoom as f64, (h.x - lanes.left()).max(0.0));
        } else if mods.alt && scroll_delta.y != 0.0 {
            let factor = if scroll_delta.y > 0.0 { 1.25 } else { 0.8 };
            app.view.vzoom = (app.view.vzoom * factor).clamp(1.0, 256.0);
        } else {
            app.view.scroll -= scroll_delta.x as f64 * app.view.spp;
            app.view.vscroll -= scroll_delta.y;
        }
    }
    app.view.clamp();
    let tops = app.view.track_tops(app.project());
    let max_v = (tops[n_tracks] - lanes.height()).max(0.0);
    app.view.vscroll = app.view.vscroll.clamp(0.0, max_v);

    // Follow the playhead: by pages, or keeping it centred.
    if app.player.is_playing() && app.drag.is_none() {
        use crate::prefs::FollowMode;
        let ph = app.player.position() as f64;
        let visible = app.view.visible_frames();
        match app.prefs.follow {
            FollowMode::Page => {
                if ph > app.view.scroll + visible * 0.98 || ph < app.view.scroll {
                    app.view.scroll = ph - visible * 0.02;
                }
            }
            // Near the start and end the clamp lets the playhead move instead.
            FollowMode::Center => app.view.scroll = ph - visible * 0.5,
            FollowMode::Off => {}
        }
        app.view.clamp();
        if app.prefs.follow == FollowMode::Center {
            ctx.request_repaint();
        }
    }

    // Draw at whole physical pixels: while scrolling (smoothly, or keeping
    // the playhead centred) every waveform column then keeps its shape and
    // only moves, instead of shimmering as its samples shift.
    let px_frames = app.view.spp / ctx.pixels_per_point() as f64;
    let draw_scroll = (app.view.scroll / px_frames).round() * px_frames;

    let geo = Geo {
        ruler: Rect::from_min_max(
            pos2(lanes.left(), full.top()),
            pos2(full.right(), full.top() + RULER_H),
        ),
        headers: Rect::from_min_max(
            pos2(full.left(), lanes.top()),
            pos2(lanes.left(), lanes.bottom()),
        ),
        lanes,
        scrollbar: Rect::from_min_max(pos2(lanes.left(), lanes.bottom()), full.right_bottom()),
        spp: app.view.spp,
        scroll: draw_scroll,
        vscroll: app.view.vscroll,
        tops,
        vzoom: app.view.vzoom,
        db: app.view.db_scale,
    };

    // Interaction regions (headers are handled per track below).
    let lanes_resp = ui.interact(geo.lanes, Id::unique("lanes"), Sense::click_and_drag());
    let ruler_resp = ui.interact(geo.ruler, Id::unique("ruler"), Sense::click_and_drag());
    let sb_resp = ui.interact(
        geo.scrollbar,
        Id::unique("hscroll"),
        Sense::click_and_drag(),
    );

    let base = app.project().clone();
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let press_origin = ui.input(|i| i.pointer.press_origin());

    lane_interaction(
        app,
        &ctx,
        &base,
        &geo,
        &lanes_resp,
        pointer,
        press_origin,
        mods,
    );
    ruler_interaction(app, &base, &geo, &ruler_resp, pointer, press_origin, mods);
    scrollbar_interaction(app, &base, &geo, &sb_resp, pointer, press_origin);

    // Cursor feedback when hovering clips.
    if app.drag.is_none() {
        if let Some(h) = hover.filter(|h| geo.lanes.contains(*h)) {
            let zone = hit_clip(&base, &geo, h).map(|x| x.2);
            let handle = matches!(zone, Some(Zone::FadeIn | Zone::FadeOut));
            if !handle && selection_edge_at(app, &base, &geo, h).is_some() {
                ctx.set_cursor_icon(CursorIcon::ResizeColumn);
            } else {
                match zone {
                    Some(Zone::LeftEdge | Zone::RightEdge) => {
                        ctx.set_cursor_icon(CursorIcon::ResizeHorizontal)
                    }
                    Some(Zone::Header) => ctx.set_cursor_icon(CursorIcon::Grab),
                    Some(Zone::FadeIn | Zone::FadeOut) => {
                        ctx.set_cursor_icon(CursorIcon::ResizeColumn)
                    }
                    _ => {}
                }
            }
        }
    } else if matches!(app.drag, Some(Drag::MoveClip { .. })) {
        ctx.set_cursor_icon(CursorIcon::Grabbing);
    }

    // --- Drawing -------------------------------------------------------------
    let mut display = app.project().clone();
    preview(&mut display, &app.drag);
    let p = &display;
    let painter = ui.painter_at(full);
    let visuals = ui.visuals().clone();
    painter.rect_filled(full, 0.0, visuals.extreme_bg_color);

    let lane_painter = painter.with_clip_rect(geo.lanes);
    let targets = app.target_tracks();
    let explicit = !app.sel.tracks.is_empty();
    for (i, t) in p.tracks.iter().enumerate() {
        let row = geo.row(i);
        if !row.intersects(geo.lanes) {
            continue;
        }
        let selected = explicit && app.sel.tracks.contains(&t.id);
        let bg = if selected {
            Color32::from_gray(38)
        } else if i % 2 == 0 {
            Color32::from_gray(26)
        } else {
            Color32::from_gray(30)
        };
        lane_painter.rect_filled(row, 0.0, bg);
        if let Some((a, b)) = app.sel.range {
            if targets.contains(&t.id) {
                let r = Rect::from_min_max(
                    pos2(geo.x(a as f64), row.top()),
                    pos2(geo.x(b as f64).max(geo.x(a as f64) + 1.0), row.bottom()),
                );
                lane_painter.rect_filled(r, 0.0, SELECTION);
            }
        }
        let audible = p.is_audible(t);
        // Bodies first, then fade curves and handles for every clip on top, so
        // overlapping clips show both halves of a crossfade.
        for c in &t.clips {
            draw_clip(&lane_painter, p, c, row, &geo, audible);
        }
        for c in &t.clips {
            draw_clip_overlay(&lane_painter, c, row, &geo, app.sel.clip == Some(c.id));
        }
        // Re-draw the selection tint over clips so it stays visible.
        if let Some((a, b)) = app.sel.range {
            if targets.contains(&t.id) {
                let r = Rect::from_min_max(
                    pos2(geo.x(a as f64), row.top()),
                    pos2(geo.x(b as f64), row.bottom()),
                );
                lane_painter.rect_filled(
                    r,
                    0.0,
                    Color32::from_rgba_unmultiplied(140, 180, 255, 40),
                );
                lane_painter.vline(
                    r.left(),
                    row.y_range(),
                    Stroke::new(1.0, Color32::from_rgb(140, 180, 255)),
                );
                lane_painter.vline(
                    r.right(),
                    row.y_range(),
                    Stroke::new(1.0, Color32::from_rgb(140, 180, 255)),
                );
            }
        }
        lane_painter.hline(
            geo.lanes.x_range(),
            row.bottom() - 0.5,
            Stroke::new(1.0, Color32::from_gray(50)),
        );
    }

    if p.tracks.is_empty() {
        painter.text(
            geo.lanes.center(),
            Align2::CENTER_CENTER,
            match app.keys.label(crate::app::Action::Import) {
                k if k.is_empty() => {
                    "Drop audio files here, or use File → Import Audio".to_string()
                }
                k => format!("Drop audio files here, or press {k} to import"),
            } + "\n\nWAV · FLAC · MP3 · OGG · AIFF · M4A/AAC · CAF, and more with FFmpeg",
            FontId::proportional(18.0),
            visuals.weak_text_color(),
        );
    }

    // Markers, cursor, playhead.
    let full_lanes_y = geo.lanes.y_range();
    for m in &p.markers {
        let x = geo.x(m.position as f64);
        lane_painter.vline(
            x,
            full_lanes_y,
            Stroke::new(1.0, MARKER.gamma_multiply(0.6)),
        );
    }
    // Proposed song markers while the Detect Song Starts dialog is open.
    if let Some(crate::app::Dialog::DetectSongs(state)) = &app.dialog {
        for (pos, name) in &state.preview {
            let x = geo.x(*pos as f64);
            lane_painter.vline(x, full_lanes_y, Stroke::new(2.0, PREVIEW));
            // Label below the clip title bar, on a backing so it reads over waveforms.
            let galley = painter.layout_no_wrap(name.clone(), FontId::proportional(12.0), PREVIEW);
            let at = pos2(x + 4.0, geo.lanes.top() + CLIP_HEADER_H + 4.0);
            let lanes_painter = painter.with_clip_rect(geo.lanes);
            lanes_painter.rect_filled(
                Rect::from_min_size(at - vec2(2.0, 1.0), galley.size() + vec2(4.0, 2.0)),
                2.0,
                Color32::from_black_alpha(190),
            );
            lanes_painter.galley(at, galley, PREVIEW);
        }
    }
    let cx = geo.x(app.cursor as f64);
    lane_painter.vline(cx, full_lanes_y, Stroke::new(1.0, CURSOR));
    if app.player.is_playing() {
        let px = geo.x(app.player.position() as f64);
        lane_painter.vline(px, full_lanes_y, Stroke::new(2.0, PLAYHEAD));
    }

    draw_ruler(&painter, app, p, &geo, &visuals);
    draw_scrollbar(
        &painter,
        app,
        p,
        &geo,
        sb_resp.hovered() || sb_resp.dragged(),
    );
    track_headers(app, ui, p, &geo);
    track_resize_handles(app, ui, p, &geo);

    // Header column corner.
    let corner = Rect::from_min_max(full.left_top(), pos2(lanes.left(), lanes.top()));
    painter.rect_filled(corner, 0.0, visuals.panel_fill);
    painter.text(
        corner.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        {
            let mut label = "Tracks".to_string();
            if app.view.vzoom > 1.0 {
                label.push_str(&format!("  ·  ×{:.1}", app.view.vzoom));
            }
            if app.view.db_scale {
                label.push_str("  ·  dB");
            }
            label
        },
        FontId::proportional(13.0),
        visuals.weak_text_color(),
    );
}

#[allow(clippy::too_many_arguments)]
fn lane_interaction(
    app: &mut KerfApp,
    ctx: &egui::Context,
    p: &Project,
    geo: &Geo,
    resp: &egui::Response,
    pointer: Option<Pos2>,
    press_origin: Option<Pos2>,
    mods: egui::Modifiers,
) {
    let n = p.tracks.len();
    if resp.drag_started() {
        if let Some(o) = press_origin {
            let f = geo.frame(o.x);
            let hit = hit_clip(p, geo, o);
            let on_handle = matches!(hit, Some((_, _, Zone::FadeIn | Zone::FadeOut)));
            let edge = if on_handle {
                None
            } else {
                selection_edge_at(app, p, geo, o)
            };
            app.drag = Some(match hit {
                _ if edge.is_some() => Drag::SelectionEdge {
                    anchor: edge.unwrap(),
                },
                Some((ti, id, Zone::Header)) => {
                    let c = p.clip(id).unwrap();
                    app.sel.clip = Some(id);
                    Drag::MoveClip {
                        clip: id,
                        grab: f as i64 - c.position as i64,
                        track: ti,
                        pos: c.position,
                    }
                }
                Some((_, id, Zone::LeftEdge)) => Drag::TrimStart {
                    clip: id,
                    pos: p.clip(id).unwrap().position,
                },
                Some((_, id, zone @ (Zone::FadeIn | Zone::FadeOut))) => {
                    let c = p.clip(id).unwrap();
                    app.sel.clip = Some(id);
                    // Keep the curve of a fade you made; otherwise use the chosen shape.
                    let existing = if zone == Zone::FadeIn {
                        c.fade_in
                    } else {
                        c.fade_out
                    };
                    let shape = existing
                        .filter(|f| !f.auto)
                        .map_or(app.fade_shape, |f| f.shape);
                    if zone == Zone::FadeIn {
                        Drag::FadeIn {
                            clip: id,
                            len: c.fade_in_frames(),
                            shape,
                        }
                    } else {
                        Drag::FadeOut {
                            clip: id,
                            len: c.fade_out_frames(),
                            shape,
                        }
                    }
                }
                Some((_, id, Zone::RightEdge)) => Drag::TrimEnd {
                    clip: id,
                    pos: p.clip(id).unwrap().end(),
                },
                _ => {
                    let anchor = snap(app, p, geo, f, None, mods.alt);
                    let ti = geo.track_at_clamped(o.y, n);
                    app.sel.clip = None;
                    Drag::Select {
                        anchor,
                        anchor_track: ti,
                    }
                }
            });
        }
    }

    if resp.dragged() {
        if let (Some(pos), Some(drag)) = (pointer, app.drag.clone()) {
            let f = geo.frame(pos.x);
            app.drag = Some(match drag {
                Drag::Select {
                    anchor,
                    anchor_track,
                } => {
                    let f = snap(app, p, geo, f, None, mods.alt);
                    let ti = geo.track_at_clamped(pos.y, n);
                    let (a, b) = (anchor.min(f), anchor.max(f));
                    app.sel.range = (b > a).then_some((a, b));
                    app.cursor = f;
                    if n > 0 {
                        let (t0, t1) = (anchor_track.min(ti), anchor_track.max(ti));
                        app.sel.tracks = p.tracks[t0..=t1].iter().map(|t| t.id).collect();
                    }
                    drag
                }
                Drag::MoveClip { clip, grab, .. } => {
                    let c = p.clip(clip).unwrap();
                    let raw = (f as i64 - grab).max(0) as Frame;
                    let s = snap(app, p, geo, raw, Some(clip), mods.alt);
                    let pos = if s != raw {
                        s
                    } else {
                        let e = snap(app, p, geo, raw + c.length, Some(clip), mods.alt);
                        e.saturating_sub(c.length)
                    };
                    Drag::MoveClip {
                        clip,
                        grab,
                        track: geo.track_at_clamped(pos_y(pointer), n),
                        pos,
                    }
                }
                Drag::SelectionEdge { anchor } => {
                    let f = snap(app, p, geo, f, None, mods.alt);
                    let (a, b) = (anchor.min(f), anchor.max(f));
                    app.sel.range = (b > a).then_some((a, b));
                    app.cursor = f;
                    Drag::SelectionEdge { anchor }
                }
                Drag::TrimStart { clip, .. } => Drag::TrimStart {
                    clip,
                    pos: snap(app, p, geo, f, Some(clip), mods.alt),
                },
                Drag::TrimEnd { clip, .. } => Drag::TrimEnd {
                    clip,
                    pos: snap(app, p, geo, f, Some(clip), mods.alt),
                },
                Drag::FadeIn { clip, shape, .. } => {
                    let c = p.clip(clip).unwrap();
                    let f = snap(app, p, geo, f, Some(clip), mods.alt);
                    let len = f.saturating_sub(c.position).min(c.length);
                    Drag::FadeIn { clip, len, shape }
                }
                Drag::FadeOut { clip, shape, .. } => {
                    let c = p.clip(clip).unwrap();
                    let f = snap(app, p, geo, f, Some(clip), mods.alt);
                    let len = c.end().saturating_sub(f).min(c.length);
                    Drag::FadeOut { clip, len, shape }
                }
                other => other,
            });
            // Auto-scroll when dragging past the edges.
            if pos.x > geo.lanes.right() - 4.0 {
                app.view.scroll += 12.0 * geo.spp;
            } else if pos.x < geo.lanes.left() + 4.0 {
                app.view.scroll = (app.view.scroll - 12.0 * geo.spp).max(0.0);
            }
        }
    }

    if resp.drag_stopped() {
        match app.drag.take() {
            Some(Drag::MoveClip {
                clip, track, pos, ..
            }) => {
                if let Some(tid) = p.tracks.get(track).map(|t| t.id) {
                    app.edit("Move clip", |p| edit::move_clip(p, clip, tid, pos));
                }
            }
            Some(Drag::TrimStart { clip, pos }) => {
                app.edit("Trim clip", |p| edit::trim_clip_start(p, clip, pos));
            }
            Some(Drag::TrimEnd { clip, pos }) => {
                app.edit("Trim clip", |p| edit::trim_clip_end(p, clip, pos));
            }
            Some(Drag::FadeIn { clip, len, shape }) => {
                app.edit("Fade in", |p| edit::set_clip_fade_in(p, clip, len, shape));
            }
            Some(Drag::FadeOut { clip, len, shape }) => {
                app.edit("Fade out", |p| edit::set_clip_fade_out(p, clip, len, shape));
            }
            Some(Drag::Select { .. } | Drag::SelectionEdge { .. }) => app.finish_selection(),
            other => app.drag = other,
        }
    }

    if resp.clicked() {
        if let Some(pos) = pointer {
            let f = snap(app, p, geo, geo.frame(pos.x), None, mods.alt);
            let ti = geo.track_at(pos.y, n);
            let hit = hit_clip(p, geo, pos);
            if mods.shift {
                let anchor = app
                    .sel
                    .range
                    .map_or(app.cursor, |(a, b)| if f < a { b } else { a });
                let (a, b) = (anchor.min(f), anchor.max(f));
                app.sel.range = (b > a).then_some((a, b));
            } else {
                app.sel.range = None;
                app.seek(f);
            }
            match ti.map(|i| p.tracks[i].id) {
                Some(id) if mods.command => toggle(&mut app.sel.tracks, id),
                Some(id) if !mods.shift => app.sel.tracks = vec![id],
                Some(id) => {
                    if !app.sel.tracks.contains(&id) {
                        app.sel.tracks.push(id);
                    }
                }
                None => app.sel.tracks.clear(),
            }
            app.sel.clip = hit.filter(|h| h.2 == Zone::Header).map(|h| h.1);
        }
    }

    if resp.double_clicked() {
        if let Some((ti, id, _)) = pointer.and_then(|pos| hit_clip(p, geo, pos)) {
            let c = p.clip(id).unwrap();
            app.sel.range = Some((c.position, c.end()));
            app.sel.tracks = vec![p.tracks[ti].id];
            app.sel.clip = Some(id);
            app.cursor = c.position;
        } else if let Some(pos) = pointer {
            app.select_between_markers(geo.frame(pos.x));
        }
    }

    if resp.secondary_clicked() {
        if let Some(pos) = pointer {
            let f = geo.frame(pos.x);
            let inside = app.sel.range.is_some_and(|(a, b)| f >= a && f <= b);
            if !inside {
                app.sel.range = None;
                app.cursor = f;
                if let Some(ti) = geo.track_at(pos.y, n) {
                    app.sel.tracks = vec![p.tracks[ti].id];
                }
            }
            app.sel.clip = hit_clip(p, geo, pos).map(|h| h.1);
        }
    }
    enum ClipCmd {
        Shape(ClipId, FadeShape),
        RemoveFades(ClipId),
    }
    let mut action = None;
    let mut clip_cmd = None;
    resp.context_menu(|ui| {
        let has_range = app.sel.range.is_some();
        let mut item = |ui: &mut egui::Ui, label: &str, a: Action, enabled: bool| {
            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                action = Some(a);
                ui.close();
            }
        };
        item(ui, "Split here", Action::Split, true);
        ui.separator();
        item(ui, "Cut", Action::Cut, has_range);
        item(ui, "Copy", Action::Copy, has_range);
        item(ui, "Paste", Action::Paste, app.clipboard.is_some());
        item(
            ui,
            "Delete",
            Action::Delete,
            has_range || app.sel.clip.is_some(),
        );
        item(ui, "Silence", Action::Silence, has_range);
        item(ui, "Trim to selection", Action::Crop, has_range);
        ui.separator();
        item(ui, "Normalize…", Action::Normalize, true);
        item(ui, "Gain…", Action::Gain, true);
        item(ui, "Fade in", Action::FadeIn, has_range);
        item(ui, "Fade out", Action::FadeOut, has_range);
        item(ui, "Remove fades", Action::RemoveFades, true);
        if let Some(c) = app.sel.clip.and_then(|id| p.clip(id)) {
            if c.fade_in.is_some() || c.fade_out.is_some() {
                ui.menu_button("Clip fade shape", |ui| {
                    let current = c.fade_in.or(c.fade_out).map(|f| f.shape);
                    for s in FadeShape::ALL {
                        if ui.radio(current == Some(s), s.label()).clicked() {
                            clip_cmd = Some(ClipCmd::Shape(c.id, s));
                            ui.close();
                        }
                    }
                });
                if ui.button("Remove clip fades").clicked() {
                    clip_cmd = Some(ClipCmd::RemoveFades(c.id));
                    ui.close();
                }
            }
        }
        ui.separator();
        item(ui, "Render track in place", Action::Bounce, true);
        item(ui, "Add marker", Action::AddMarker, true);
    });
    if let Some(a) = action {
        app.perform(ctx, a);
    }
    match clip_cmd {
        Some(ClipCmd::Shape(id, s)) => {
            app.edit("Fade shape", |p| edit::set_clip_fade_shape(p, id, s));
        }
        Some(ClipCmd::RemoveFades(id)) => {
            app.edit("Remove clip fades", |p| {
                edit::set_clip_fade_in(p, id, 0, FadeShape::default())?;
                edit::set_clip_fade_out(p, id, 0, FadeShape::default())
            });
        }
        None => {}
    }

    fn pos_y(p: Option<Pos2>) -> f32 {
        p.map_or(0.0, |p| p.y)
    }
}

/// If `pos` is on an edge of the time selection (on a selected track),
/// the *other* edge, which stays put while this one is dragged.
fn selection_edge_at(app: &KerfApp, p: &Project, geo: &Geo, pos: Pos2) -> Option<Frame> {
    let (a, b) = app.sel.range?;
    let ti = geo.track_at(pos.y, p.tracks.len())?;
    if !app.target_tracks().contains(&p.tracks[ti].id) {
        return None;
    }
    if (pos.x - geo.x(a as f64)).abs() <= EDGE_PX {
        Some(b)
    } else if (pos.x - geo.x(b as f64)).abs() <= EDGE_PX {
        Some(a)
    } else {
        None
    }
}

fn toggle(v: &mut Vec<TrackId>, id: TrackId) {
    if let Some(i) = v.iter().position(|x| *x == id) {
        v.remove(i);
    } else {
        v.push(id);
    }
}

fn marker_at(p: &Project, geo: &Geo, x: f32) -> Option<MarkerId> {
    p.markers
        .iter()
        .map(|m| (m.id, (geo.x(m.position as f64) - x).abs()))
        .filter(|(_, d)| *d < 6.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

/// Whether a press in the ruler at `pos` grabs the playhead (to scrub): the
/// playhead line while playing, or the cursor's handle at the bottom.
fn on_playhead_handle(app: &KerfApp, geo: &Geo, pos: Pos2) -> bool {
    let near = |f: Frame| (geo.x(f as f64) - pos.x).abs() <= 6.0;
    (app.player.is_playing() && near(app.player.position()))
        || (near(app.cursor) && pos.y >= geo.ruler.bottom() - 10.0)
}

fn ruler_interaction(
    app: &mut KerfApp,
    p: &Project,
    geo: &Geo,
    resp: &egui::Response,
    pointer: Option<Pos2>,
    press_origin: Option<Pos2>,
    mods: egui::Modifiers,
) {
    if resp.hovered()
        && app.drag.is_none()
        && pointer.is_some_and(|h| on_playhead_handle(app, geo, h))
    {
        resp.ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    if resp.drag_started() {
        if let Some(o) = press_origin {
            // Ctrl+drag anywhere, or grab the playhead: scrub.
            app.drag = Some(if mods.command || on_playhead_handle(app, geo, o) {
                app.begin_scrub(geo.frame(o.x))
            } else {
                match marker_at(p, geo, o.x) {
                    Some(id) => Drag::Marker {
                        id,
                        pos: p.markers.iter().find(|m| m.id == id).unwrap().position,
                    },
                    None => Drag::RulerSelect {
                        anchor: snap(app, p, geo, geo.frame(o.x), None, mods.alt),
                    },
                }
            });
        }
    }
    if resp.dragged() {
        if let (Some(pos), Some(drag)) = (pointer, app.drag.clone()) {
            let f = snap(app, p, geo, geo.frame(pos.x), None, mods.alt);
            match drag {
                Drag::Marker { id, .. } => app.drag = Some(Drag::Marker { id, pos: f }),
                Drag::RulerSelect { anchor } => {
                    let (a, b) = (anchor.min(f), anchor.max(f));
                    app.sel.range = (b > a).then_some((a, b));
                    app.sel.tracks.clear();
                    app.cursor = f;
                }
                Drag::Scrub { .. } => {
                    resp.ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
                    app.scrub_to(geo.frame(pos.x));
                    // Follow the mouse past the edges.
                    if pos.x > geo.lanes.right() - 4.0 {
                        app.view.scroll += 12.0 * geo.spp;
                    } else if pos.x < geo.lanes.left() + 4.0 {
                        app.view.scroll = (app.view.scroll - 12.0 * geo.spp).max(0.0);
                    }
                }
                _ => {}
            }
        }
    }
    if resp.drag_stopped() {
        match app.drag.take() {
            Some(Drag::Marker { id, pos }) => {
                app.edit("Move marker", |p| edit::move_marker(p, id, pos));
            }
            Some(Drag::RulerSelect { .. }) => app.finish_selection(),
            Some(Drag::Scrub { resume }) => app.end_scrub(resume),
            None => {}
            other => app.drag = other,
        }
    }
    if resp.clicked() {
        if let Some(pos) = pointer {
            app.sel.range = None;
            let f = snap(app, p, geo, geo.frame(pos.x), None, mods.alt);
            app.seek(f);
        }
    }
    if resp.double_clicked() {
        if let Some(id) = pointer.and_then(|pos| marker_at(p, geo, pos.x)) {
            let name = p
                .markers
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.name.clone())
                .unwrap_or_default();
            app.dialog = Some(Dialog::RenameMarker(id, name));
        } else if let Some(pos) = pointer {
            // Between markers: select that song.
            app.select_between_markers(geo.frame(pos.x));
        }
    }
    let menu_id = Id::unique("ruler-marker-menu");
    if resp.secondary_clicked() {
        let hit = pointer.and_then(|pos| marker_at(p, geo, pos.x));
        resp.ctx.data_mut(|d| d.insert_temp(menu_id, hit));
    }
    let hit: Option<MarkerId> = resp.ctx.data(|d| d.get_temp(menu_id)).flatten();
    resp.context_menu(|ui| {
        if let Some(id) = hit {
            if ui.button("Rename marker…").clicked() {
                let name = p
                    .markers
                    .iter()
                    .find(|m| m.id == id)
                    .map(|m| m.name.clone())
                    .unwrap_or_default();
                app.dialog = Some(Dialog::RenameMarker(id, name));
                ui.close();
            }
            if ui.button("Delete marker").clicked() {
                app.edit("Delete marker", |p| edit::remove_marker(p, id));
                ui.close();
            }
        } else if ui.button("Add marker here").clicked() {
            let n = p.markers.len() + 1;
            let pos = app.cursor;
            app.edit("Add marker", |p| {
                Ok(p.add_marker(pos, format!("Marker {n}")))
            });
            ui.close();
        }
    });
}

fn scroll_total(app: &KerfApp, _p: &Project) -> f64 {
    app.view
        .scroll_extent()
        .max(app.view.scroll + app.view.visible_frames())
}

fn scrollbar_interaction(
    app: &mut KerfApp,
    p: &Project,
    geo: &Geo,
    resp: &egui::Response,
    pointer: Option<Pos2>,
    press_origin: Option<Pos2>,
) {
    let total = scroll_total(app, p);
    let w = geo.scrollbar.width() as f64;
    let thumb_x = geo.scrollbar.left() + (app.view.scroll / total * w) as f32;
    let thumb_w = ((app.view.visible_frames() / total * w) as f32).max(20.0);
    if resp.drag_started() {
        if let Some(o) = press_origin {
            let grab = if o.x >= thumb_x && o.x <= thumb_x + thumb_w {
                o.x - thumb_x
            } else {
                thumb_w / 2.0
            };
            app.drag = Some(Drag::Scrollbar { grab });
        }
    }
    if let (Some(Drag::Scrollbar { grab }), Some(pos)) = (&app.drag, pointer) {
        if resp.dragged() || resp.drag_started() {
            app.view.scroll = (((pos.x - grab - geo.scrollbar.left()) as f64) / w * total).max(0.0);
        }
    }
    if resp.drag_stopped() && matches!(app.drag, Some(Drag::Scrollbar { .. })) {
        app.drag = None;
    }
    if resp.clicked() {
        if let Some(pos) = pointer {
            app.view.scroll = (((pos.x - geo.scrollbar.left()) as f64) / w * total
                - app.view.visible_frames() / 2.0)
                .max(0.0);
        }
    }
}

fn draw_scrollbar(painter: &egui::Painter, app: &KerfApp, p: &Project, geo: &Geo, active: bool) {
    let total = scroll_total(app, p);
    let w = geo.scrollbar.width() as f64;
    painter.rect_filled(geo.scrollbar, 0.0, Color32::from_gray(20));
    let x = geo.scrollbar.left() + (app.view.scroll / total * w) as f32;
    let tw = ((app.view.visible_frames() / total * w) as f32).max(20.0);
    let thumb = Rect::from_min_size(
        pos2(x, geo.scrollbar.top() + 2.0),
        vec2(tw, geo.scrollbar.height() - 4.0),
    );
    painter.rect_filled(
        thumb,
        4.0,
        Color32::from_gray(if active { 120 } else { 80 }),
    );
}

fn draw_ruler(
    painter: &egui::Painter,
    app: &KerfApp,
    p: &Project,
    geo: &Geo,
    visuals: &egui::Visuals,
) {
    let r = geo.ruler;
    painter.rect_filled(r, 0.0, visuals.panel_fill);
    let rp = painter.with_clip_rect(r);
    let sr = p.sample_rate as f64;
    let secs_per_px = geo.spp / sr;
    const STEPS: [f64; 22] = [
        0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0,
        30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
    ];
    let step = STEPS
        .iter()
        .copied()
        .find(|s| s / secs_per_px >= 90.0)
        .unwrap_or(7200.0);
    let start_s = geo.scroll / sr;
    let end_s = start_s + geo.lanes.width() as f64 * secs_per_px;
    let minor = step / 5.0;
    let mut t = (start_s / minor).floor() * minor;
    let text_color = visuals.text_color();
    while t <= end_s {
        let x = geo.x(t * sr);
        let major = ((t / step).round() * step - t).abs() < minor * 0.01;
        let h = if major { 10.0 } else { 4.0 };
        rp.vline(
            x,
            (r.bottom() - h)..=r.bottom(),
            Stroke::new(1.0, Color32::from_gray(110)),
        );
        if major {
            let frame = (t * sr).round() as Frame;
            let label = match app.prefs.time_format {
                TimeFormat::MinSec => {
                    let l = format_time(frame, p.sample_rate);
                    if step >= 1.0 {
                        l.trim_end_matches(".000").to_string()
                    } else {
                        l
                    }
                }
                TimeFormat::Seconds if step >= 1.0 => format!("{}", frame / p.sample_rate as Frame),
                fmt => format_time_as(frame, p.sample_rate, fmt),
            };
            rp.text(
                pos2(x + 3.0, r.top() + 2.0),
                Align2::LEFT_TOP,
                label,
                FontId::monospace(11.0),
                text_color,
            );
        }
        t += minor;
    }
    if let Some((a, b)) = app.sel.range {
        let sel = Rect::from_min_max(
            pos2(geo.x(a as f64), r.bottom() - 4.0),
            pos2(geo.x(b as f64), r.bottom()),
        );
        rp.rect_filled(sel, 0.0, Color32::from_rgb(140, 180, 255));
    }
    for m in &p.markers {
        let x = geo.x(m.position as f64);
        let flag = vec![
            pos2(x, r.top() + 14.0),
            pos2(x + 8.0, r.top() + 18.0),
            pos2(x, r.top() + 22.0),
        ];
        rp.add(Shape::convex_polygon(flag, MARKER, Stroke::NONE));
        rp.vline(x, (r.top() + 14.0)..=r.bottom(), Stroke::new(1.5, MARKER));
        rp.text(
            pos2(x + 10.0, r.top() + 13.0),
            Align2::LEFT_TOP,
            &m.name,
            FontId::proportional(11.0),
            MARKER,
        );
    }
    let cx = geo.x(app.cursor as f64);
    rp.add(Shape::convex_polygon(
        vec![
            pos2(cx - 5.0, r.bottom() - 7.0),
            pos2(cx + 5.0, r.bottom() - 7.0),
            pos2(cx, r.bottom()),
        ],
        CURSOR,
        Stroke::NONE,
    ));
    if app.player.is_playing() {
        let px = geo.x(app.player.position() as f64);
        rp.vline(px, r.y_range(), Stroke::new(2.0, PLAYHEAD));
    }
    painter.hline(
        r.x_range(),
        r.bottom() - 0.5,
        Stroke::new(1.0, Color32::from_gray(60)),
    );
}

fn draw_clip(painter: &egui::Painter, p: &Project, c: &Clip, row: Rect, geo: &Geo, audible: bool) {
    let x0 = geo.x(c.position as f64);
    let x1 = geo.x(c.end() as f64);
    let clip_area = painter.clip_rect();
    if x1 < clip_area.left() || x0 > clip_area.right() {
        return;
    }
    let rect = Rect::from_min_max(
        pos2(x0, row.top() + 1.0),
        pos2(x1.max(x0 + 1.0), row.bottom() - 2.0),
    );
    painter.rect_filled(rect, 3.0, CLIP_BG);
    let header = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + CLIP_HEADER_H));
    painter.rect_filled(header, 3.0, CLIP_HDR);
    let Some(src) = p.sources.get(&c.source) else {
        return;
    };

    // Title: source name and clip gain.
    let mut title = src.name.clone();
    if (c.gain - 1.0).abs() > 1e-6 {
        title.push_str(&format!("  {:+.1} dB", gain_to_db(c.gain as f64)));
    }
    let text_pos = pos2(x0.max(clip_area.left()) + 4.0, rect.top() + 1.0);
    painter.with_clip_rect(rect.intersect(clip_area)).text(
        text_pos,
        Align2::LEFT_TOP,
        title,
        FontId::proportional(11.0),
        Color32::from_gray(220),
    );

    let wave = Rect::from_min_max(pos2(rect.left(), header.bottom() + 1.0), rect.max);
    let buf = &src.audio;
    let sc = buf.num_channels();
    let ch_h = wave.height() / sc as f32;
    let color = if audible { WAVE } else { WAVE_MUTED };
    let fades = c.has_active_fades();
    let vis0 = x0.max(clip_area.left()).floor();
    let vis1 = x1.min(clip_area.right()).ceil();

    for ch in 0..sc {
        let mid = wave.top() + ch_h * (ch as f32 + 0.5);
        painter.hline(
            rect.x_range(),
            mid,
            Stroke::new(1.0, Color32::from_gray(60)),
        );
    }

    if geo.spp >= 1.0 {
        // One column per physical pixel, on a fixed grid of the timeline
        // (column k covers frames k·w .. (k+1)·w), so scrolling moves the
        // columns without changing what each one shows.
        let ppp = painter.ctx().pixels_per_point();
        let col_w = 1.0 / ppp;
        let fpc = geo.spp / ppp as f64;
        let mut mesh = Mesh::default();
        for (px, f_a) in wave_columns(geo, vis0, vis1, ppp) {
            let s0 = (f_a - c.position as f64 + c.source_start as f64).max(c.source_start as f64);
            let s1 =
                (f_a + fpc - c.position as f64 + c.source_start as f64).min(c.source_end() as f64);
            if s1 > s0 {
                let i0 = s0.floor() as usize;
                let i1 = (s1.ceil() as usize).max(i0 + 1);
                let g = if fades {
                    c.gain_at_source(((i0 + i1) / 2) as i64)
                } else {
                    c.gain
                };
                for ch in 0..sc {
                    let (lo, hi) = src.peaks.min_max(buf, ch, i0, i1);
                    let (lo, hi) = (lo * g, hi * g);
                    let mid = wave.top() + ch_h * (ch as f32 + 0.5);
                    let half = ch_h * 0.5 - 1.0;
                    let y_hi = mid - geo.amp(hi) * half;
                    let y_lo = (mid - geo.amp(lo) * half).max(y_hi + 1.0);
                    let col = if hi > 1.0 || lo < -1.0 {
                        CLIPPED
                    } else {
                        color
                    };
                    mesh.add_colored_rect(
                        Rect::from_min_max(pos2(px, y_hi), pos2(px + col_w, y_lo)),
                        col,
                    );
                }
            }
        }
        painter.add(Shape::mesh(mesh));
    } else {
        // Sample-level view: connect individual samples.
        let f0 = geo.frame_f(vis0).floor().max(c.position as f64) as Frame;
        let f1 = (geo.frame_f(vis1).ceil() as Frame + 1).min(c.end());
        for ch in 0..sc {
            let mid = wave.top() + ch_h * (ch as f32 + 0.5);
            let half = ch_h * 0.5 - 1.0;
            let data = buf.channel(ch);
            let pts: Vec<Pos2> = (f0..f1)
                .map(|f| {
                    let si = (f - c.position + c.source_start) as usize;
                    let v = data[si] * c.gain_at_source(si as i64);
                    pos2(geo.x(f as f64), mid - geo.amp(v) * half)
                })
                .collect();
            if geo.spp < 0.2 {
                for &pt in &pts {
                    painter.circle_filled(pt, 2.0, color);
                    painter.vline(
                        pt.x,
                        (mid.min(pt.y))..=(mid.max(pt.y)),
                        Stroke::new(1.0, color.gamma_multiply(0.5)),
                    );
                }
            }
            painter.add(Shape::line(pts, Stroke::new(1.5, color)));
        }
    }

    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, Color32::from_gray(70)),
        egui::StrokeKind::Inside,
    );
}

/// Waveform columns between screen x `vis0` and `vis1`: one per physical
/// pixel, as (x, first timeline frame), on a fixed grid of the timeline so
/// that scrolling moves columns without changing what they show.
fn wave_columns(
    geo: &Geo,
    vis0: f32,
    vis1: f32,
    ppp: f32,
) -> impl Iterator<Item = (f32, f64)> + '_ {
    let fpc = geo.spp / ppp as f64;
    let k0 = (geo.frame_f(vis0) / fpc).floor() as i64;
    let k1 = (geo.frame_f(vis1) / fpc).ceil() as i64;
    (k0..k1).map(move |k| {
        let f = k as f64 * fpc;
        ((geo.x(f) * ppp).round() / ppp, f)
    })
}

/// Fade envelope, fade handles and selection outline, drawn above all clip bodies.
fn draw_clip_overlay(painter: &egui::Painter, c: &Clip, row: Rect, geo: &Geo, selected: bool) {
    let x0 = geo.x(c.position as f64);
    let x1 = geo.x(c.end() as f64);
    let clip_area = painter.clip_rect();
    if x1 < clip_area.left() || x0 > clip_area.right() {
        return;
    }
    let rect = Rect::from_min_max(
        pos2(x0, row.top() + 1.0),
        pos2(x1.max(x0 + 1.0), row.bottom() - 2.0),
    );
    let wave = Rect::from_min_max(
        pos2(rect.left(), rect.top() + CLIP_HEADER_H + 1.0),
        rect.max,
    );

    if c.has_active_fades() {
        let gmax = c.gain.max(1e-6);
        let mut pts = Vec::new();
        let mut px = x0.max(clip_area.left()).floor();
        let end = x1.min(clip_area.right()).ceil();
        while px <= end {
            let src_f = (geo.frame_f(px) - c.position as f64 + c.source_start as f64) as i64;
            let src_f = src_f.clamp(c.source_start as i64, c.source_end() as i64 - 1);
            let g = c.gain_at_source(src_f) / gmax;
            pts.push(pos2(px, wave.bottom() - g * wave.height()));
            px += 3.0;
        }
        painter.add(Shape::line(pts, Stroke::new(1.5, FADE)));
    }

    // Fade handles: drag to set fade-in / fade-out length.
    if x1 - x0 > EDGE_PX * 3.0 {
        let (hx_in, hx_out) = fade_handle_xs(geo, c);
        let y = wave.top() + HANDLE_BAND / 2.0;
        for (hx, active) in [
            (hx_in, c.fade_in_frames() > 0),
            (hx_out, c.fade_out_frames() > 0),
        ] {
            let r = Rect::from_center_size(pos2(hx, y), vec2(HANDLE_PX + 1.0, HANDLE_PX + 1.0));
            let fill = if active {
                FADE
            } else {
                Color32::from_gray(170)
            };
            painter.rect_filled(r, 1.5, fill);
            painter.rect_stroke(
                r,
                1.5,
                Stroke::new(1.0, Color32::from_gray(30)),
                egui::StrokeKind::Outside,
            );
        }
    }

    if selected {
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(2.0, CLIP_SEL),
            egui::StrokeKind::Inside,
        );
    }
}

/// How far above a track's bottom edge the resize handle reaches.
const RESIZE_GRAB: f32 = 5.0;

/// Drag the line under a track (in its header or lane) to change its height;
/// with Ctrl/Cmd held every track follows. Double-click resets it.
fn track_resize_handles(app: &mut KerfApp, ui: &mut egui::Ui, p: &Project, geo: &Geo) {
    let area = geo.headers.union(geo.lanes);
    for (i, t) in p.tracks.iter().enumerate() {
        let bottom = geo.row_top(i + 1);
        let rect = Rect::from_min_max(
            pos2(area.left(), bottom - RESIZE_GRAB),
            pos2(area.right(), bottom + 1.0),
        )
        .intersect(area);
        if !rect.is_positive() {
            continue;
        }
        let id = t.id;
        let resp = ui.interact(
            rect,
            Id::unique(("track-resize", id.0)),
            Sense::click_and_drag(),
        );
        let resp = resp
            .on_hover_text("Drag to resize the track (Ctrl: all tracks) · double-click to reset");
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            ui.painter().with_clip_rect(area).hline(
                area.x_range(),
                bottom - 1.0,
                Stroke::new(2.0, Color32::from_rgb(90, 150, 220)),
            );
        }
        if resp.dragged() {
            let h = app.view.height_of(id) + resp.drag_delta().y;
            if ui.input(|i| i.modifiers.command) {
                app.view.track_heights.clear();
                app.view.track_height = h.clamp(crate::app::MIN_TRACK_H, crate::app::MAX_TRACK_H);
            } else {
                app.view.set_height(id, h);
            }
        }
        if resp.double_clicked() {
            app.view.track_heights.remove(&id);
        }
    }
}

fn track_headers(app: &mut KerfApp, ui: &mut egui::Ui, p: &Project, geo: &Geo) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .with_clip_rect(geo.headers)
        .rect_filled(geo.headers, 0.0, visuals.panel_fill);

    for (i, t) in p.tracks.iter().enumerate() {
        let rect = Rect::from_min_max(
            pos2(geo.headers.left(), geo.row_top(i)),
            pos2(geo.headers.right(), geo.row_top(i + 1)),
        );
        if !rect.intersects(geo.headers) {
            continue;
        }
        let id = t.id;
        let selected = app.sel.tracks.contains(&id);
        let clip = rect.intersect(geo.headers);
        let painter = ui.painter().with_clip_rect(clip);
        painter.rect_filled(
            rect,
            0.0,
            if selected {
                Color32::from_rgb(40, 56, 78)
            } else {
                visuals.panel_fill
            },
        );
        painter.hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, Color32::from_gray(50)),
        );
        painter.vline(
            rect.right() - 0.5,
            rect.y_range(),
            Stroke::new(1.0, Color32::from_gray(60)),
        );

        let resp = ui.interact(clip, Id::unique(("track-header", id.0)), Sense::click());
        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            if mods.command {
                toggle(&mut app.sel.tracks, id);
            } else if mods.shift {
                if let Some(last) = app.sel.tracks.last().and_then(|l| p.track_index(*l)) {
                    let (a, b) = (last.min(i), last.max(i));
                    app.sel.tracks = p.tracks[a..=b].iter().map(|t| t.id).collect();
                } else {
                    app.sel.tracks = vec![id];
                }
            } else {
                app.sel.tracks = vec![id];
            }
        }

        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(vec2(8.0, 6.0)))
                .layout(Layout::top_down(Align::Min)),
        );
        child.set_clip_rect(clip);
        child.spacing_mut().item_spacing.y = 4.0;
        child.spacing_mut().slider_width = HEADER_W - 90.0;

        child.horizontal(|ui| {
            let mut name = t.name.clone();
            let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(HEADER_W - 56.0));
            if r.changed() {
                let _ = app
                    .doc
                    .apply_merged("Rename track", &format!("name-{}", id.0), |p| {
                        if let Some(t) = p.track_mut(id) {
                            t.name = name;
                        }
                        Ok(())
                    });
            }
            if ui.small_button("✕").on_hover_text("Remove track").clicked() {
                app.edit("Remove track", |p| edit::remove_tracks(p, &[id]));
            }
        });
        child.horizontal(|ui| {
            if ui
                .selectable_label(t.muted, " M ")
                .on_hover_text("Mute")
                .clicked()
            {
                app.edit(if t.muted { "Unmute" } else { "Mute" }, |p| {
                    if let Some(t) = p.track_mut(id) {
                        t.muted = !t.muted;
                    }
                    Ok(())
                });
            }
            if ui
                .selectable_label(t.solo, " S ")
                .on_hover_text("Solo")
                .clicked()
            {
                app.edit(if t.solo { "Unsolo" } else { "Solo" }, |p| {
                    if let Some(t) = p.track_mut(id) {
                        t.solo = !t.solo;
                    }
                    Ok(())
                });
            }
            let chans = t
                .clips
                .first()
                .and_then(|c| p.sources.get(&c.source))
                .map(|s| match s.audio.num_channels() {
                    1 => "mono".to_string(),
                    2 => "stereo".to_string(),
                    n => format!("{n} ch"),
                })
                .unwrap_or_default();
            ui.label(egui::RichText::new(chans).weak().small());
        });
        let mut db = gain_to_db(t.gain as f64).max(-60.0);
        let r = child.add(
            egui::Slider::new(&mut db, -60.0..=12.0)
                .suffix(" dB")
                .fixed_decimals(1)
                .custom_formatter(|v, _| {
                    if v <= -60.0 {
                        "-inf".into()
                    } else {
                        format!("{v:.1}")
                    }
                }),
        );
        if r.double_clicked() {
            db = 0.0;
        }
        if r.changed() || r.double_clicked() {
            let gain = if db <= -60.0 {
                0.0
            } else {
                db_to_gain(db) as f32
            };
            let _ = app
                .doc
                .apply_merged("Track gain", &format!("gain-{}", id.0), |p| {
                    if let Some(t) = p.track_mut(id) {
                        t.gain = gain;
                    }
                    Ok(())
                });
        }
        let mut pan = t.pan;
        let r = child.add(
            egui::Slider::new(&mut pan, -1.0..=1.0).custom_formatter(|v, _| {
                if v.abs() < 0.005 {
                    "C".into()
                } else if v < 0.0 {
                    format!("L{:.0}", -v * 100.0)
                } else {
                    format!("R{:.0}", v * 100.0)
                }
            }),
        );
        if r.double_clicked() {
            pan = 0.0;
        }
        if r.changed() || r.double_clicked() {
            let _ = app
                .doc
                .apply_merged("Track pan", &format!("pan-{}", id.0), |p| {
                    if let Some(t) = p.track_mut(id) {
                        t.pan = pan;
                    }
                    Ok(())
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(tops: Vec<f32>, vscroll: f32) -> Geo {
        let r = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let lanes = Rect::from_min_max(pos2(200.0, 30.0), pos2(1000.0, 500.0));
        Geo {
            ruler: r,
            headers: r,
            lanes,
            scrollbar: r,
            spp: 1.0,
            scroll: 0.0,
            vscroll,
            tops,
            vzoom: 1.0,
            db: false,
        }
    }

    #[test]
    fn rows_follow_individual_track_heights() {
        // Three tracks: 100, 300 and 50 points tall.
        let g = geo(vec![0.0, 100.0, 400.0, 450.0], 0.0);
        assert_eq!(g.row(1).top(), 130.0);
        assert_eq!(g.row(1).height(), 300.0);
        assert_eq!(g.row_top(3), 480.0, "bottom of the last track");
        assert_eq!(g.track_at(29.0, 3), None);
        assert_eq!(g.track_at(30.0, 3), Some(0));
        assert_eq!(g.track_at(129.9, 3), Some(0));
        assert_eq!(g.track_at(130.0, 3), Some(1));
        assert_eq!(g.track_at(479.0, 3), Some(2));
        assert_eq!(g.track_at(480.0, 3), None);
        assert_eq!(g.track_at_clamped(0.0, 3), 0);
        assert_eq!(g.track_at_clamped(900.0, 3), 2);

        let scrolled = geo(vec![0.0, 100.0, 400.0, 450.0], 150.0);
        assert_eq!(scrolled.track_at(30.0, 3), Some(1));
        assert_eq!(scrolled.row_top(1), -20.0);
    }

    #[test]
    fn scrolling_moves_waveform_columns_without_changing_them() {
        let ppp = 1.28;
        let spp = 437.0;
        let columns = |view_scroll: f64| {
            // As in `show`: draw at whole physical pixels.
            let px_frames = spp / ppp as f64;
            let mut g = geo(vec![0.0, 100.0], 0.0);
            g.spp = spp;
            g.scroll = (view_scroll / px_frames).round() * px_frames;
            wave_columns(&g, 300.0, 900.0, ppp).collect::<Vec<_>>()
        };
        let base = columns(1_000_000.0);
        // Smooth scrolling lands on arbitrary fractions of a pixel.
        for d in [0.13, 0.5, 1.0, 7.77, 123.4] {
            let moved = columns(1_000_000.0 + d * spp);
            let by: std::collections::HashMap<u64, f32> =
                moved.iter().map(|&(x, f)| (f.to_bits(), x)).collect();
            let mut shifts = Vec::new();
            for (x, f) in &base {
                if let Some(x2) = by.get(&f.to_bits()) {
                    // Same column (same frames), moved by whole physical pixels.
                    let shift = (x - x2) * ppp;
                    assert!((shift - shift.round()).abs() < 1e-3, "{shift}");
                    shifts.push(shift.round() as i64);
                }
            }
            assert!(shifts.len() > 500, "most columns stay in view");
            assert!(shifts.windows(2).all(|w| w[0] == w[1]), "all move together");
        }
    }

    #[test]
    fn no_tracks() {
        let g = geo(vec![0.0], 0.0);
        assert_eq!(g.track_at(50.0, 0), None);
        assert_eq!(g.track_at_clamped(50.0, 0), 0);
    }
}
