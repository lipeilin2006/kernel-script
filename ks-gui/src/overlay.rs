use egui::{Context, Id, LayerId, Order};

use crate::lua_runtime::DrawCommand;

/// Paints one frame of `draw.*` commands.
///
/// `origin` is the overlay window's top-left in the same coordinate space as
/// the commands (absolute logical screen units, which is what
/// `memory.get_window_rect` returns), so the painter subtracts it to get
/// window-local points.
///
/// Despite the name, `ctx.zoom_factor()` makes egui's point space
/// `logical / zoom_factor`; everything is multiplied by `s = 1/zoom` so the
/// overlay keeps tracking the game window at any whole-UI scale (the host
/// fixes it at 0.8, like ks-td2).
pub fn paint_commands(ctx: &Context, commands: &[DrawCommand], origin: (f32, f32)) {
    if commands.is_empty() {
        return;
    }
    let (origin_x, origin_y) = origin;
    let scale = 1.0 / ctx.zoom_factor().max(1e-3);
    // Background layer: strictly below every egui window (settings panels,
    // the control window, the Lua error window), so ESP output never covers
    // the UI that produced it.
    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("draw_overlay")));
    for command in commands {
        match command {
            DrawCommand::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                thickness,
            } => {
                painter.line_segment(
                    [
                        egui::pos2((*x1 - origin_x) * scale, (*y1 - origin_y) * scale),
                        egui::pos2((*x2 - origin_x) * scale, (*y2 - origin_y) * scale),
                    ],
                    egui::Stroke::new(*thickness * scale, color32(*color)),
                );
            }
            DrawCommand::Rect {
                x,
                y,
                w,
                h,
                color,
                thickness,
            } => {
                painter.rect_stroke(
                    egui::Rect::from_min_size(
                        egui::pos2((*x - origin_x) * scale, (*y - origin_y) * scale),
                        egui::vec2(*w * scale, *h * scale),
                    ),
                    0.0,
                    egui::Stroke::new(*thickness * scale, color32(*color)),
                    egui::StrokeKind::Middle,
                );
            }
            DrawCommand::FilledRect { x, y, w, h, color } => {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2((*x - origin_x) * scale, (*y - origin_y) * scale),
                        egui::vec2(*w * scale, *h * scale),
                    ),
                    0.0,
                    color32(*color),
                );
            }
            DrawCommand::Circle {
                x,
                y,
                radius,
                color,
                thickness,
            } => {
                painter.circle_stroke(
                    egui::pos2((*x - origin_x) * scale, (*y - origin_y) * scale),
                    *radius * scale,
                    egui::Stroke::new(*thickness * scale, color32(*color)),
                );
            }
            DrawCommand::FilledCircle {
                x,
                y,
                radius,
                color,
            } => {
                painter.circle_filled(
                    egui::pos2((*x - origin_x) * scale, (*y - origin_y) * scale),
                    *radius * scale,
                    color32(*color),
                );
            }
            DrawCommand::Text {
                x,
                y,
                text,
                color,
                size,
            } => {
                let color = color32(*color);
                let galley = ctx.fonts_mut(|fonts| {
                    fonts.layout_no_wrap(
                        text.clone(),
                        egui::FontId::proportional(*size * scale),
                        color,
                    )
                });
                painter.galley(
                    egui::pos2((*x - origin_x) * scale, (*y - origin_y) * scale),
                    galley,
                    egui::Color32::TRANSPARENT,
                );
            }
        }
    }
}

/// Top-right overlay frame-rate readout: EMA-smoothed actual presentation
/// rate (Hz).
pub fn paint_fps(ctx: &Context, fps: f32) {
    egui::Area::new(Id::new("fps_meter"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
        .order(Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(160))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(7, 3))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("{fps:>6.1} FPS"))
                            .monospace()
                            .color(egui::Color32::from_rgb(130, 255, 170)),
                    );
                });
        });
}

fn color32(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}
