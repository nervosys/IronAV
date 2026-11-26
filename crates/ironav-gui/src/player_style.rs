use eframe::egui::{self, Color32};

pub(crate) const ACCENT: Color32 = Color32::from_rgb(255, 140, 72);
const INK: Color32 = Color32::from_rgb(34, 18, 9);

pub(crate) fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(22, 25, 30);
    style.visuals.window_fill = Color32::from_rgb(28, 32, 38);
    style.visuals.window_stroke = egui::Stroke::new(1.0, Color32::from_rgb(52, 57, 66));
    style.visuals.window_corner_radius = egui::CornerRadius::same(10);
    style.visuals.extreme_bg_color = Color32::from_rgb(15, 17, 21);
    style.visuals.selection.bg_fill = Color32::from_rgb(79, 49, 33);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 30.0;
    let widgets = &mut style.visuals.widgets;
    for (widget, fill, border) in [
        (&mut widgets.inactive, Color32::from_rgb(33, 37, 44), Color32::from_rgb(52, 57, 66)),
        (&mut widgets.hovered, Color32::from_rgb(46, 51, 60), Color32::from_rgb(80, 88, 102)),
        (&mut widgets.active, Color32::from_rgb(27, 30, 36), ACCENT),
        (&mut widgets.open, Color32::from_rgb(43, 46, 55), Color32::from_rgb(77, 84, 95)),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = egui::Stroke::new(1.0, border);
        widget.fg_stroke.color = Color32::from_rgb(232, 234, 238);
        widget.corner_radius = egui::CornerRadius::same(8);
        widget.expansion = 0.0;
    }
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub(crate) enum Icon {
    Play,
    Pause,
    Replay,
    Back,
    Forward,
    Speaker { muted: bool },
}

pub(crate) fn transport(ui: &mut egui::Ui, icon: Icon, label: &str, primary: bool, selected: bool) -> egui::Response {
    let size = egui::vec2(if primary { 44.0 } else { 34.0 }, 34.0);
    let button = egui::Button::new("").min_size(size).selected(selected);
    let response = if primary {
        ui.scope(|ui| {
            let widgets = &mut ui.visuals_mut().widgets;
            for (widget, fill) in [
                (&mut widgets.inactive, ACCENT),
                (&mut widgets.hovered, Color32::from_rgb(255, 163, 109)),
                (&mut widgets.active, Color32::from_rgb(229, 109, 51)),
            ] {
                widget.bg_fill = fill;
                widget.weak_bg_fill = fill;
                widget.bg_stroke = egui::Stroke::new(1.0, fill);
            }
            ui.add(button)
        }).inner
    } else {
        ui.add(button)
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), label));
    let color = if primary { INK } else if selected { ACCENT } else { ui.style().interact(&response).fg_stroke.color };
    paint_icon(ui.painter(), response.rect.center(), icon, color);
    response.on_hover_text(label).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn paint_icon(painter: &egui::Painter, center: egui::Pos2, icon: Icon, color: Color32) {
    let pos = |x, y| center + egui::vec2(x, y);
    let stroke = egui::Stroke::new(1.8, color);
    match icon {
        Icon::Play => {
            painter.add(egui::Shape::convex_polygon(vec![pos(-4.5, -6.7), pos(6.8, 0.0), pos(-4.5, 6.7)], color, egui::Stroke::NONE));
        }
        Icon::Pause => {
            for x in [-4.7, 1.2] {
                painter.rect_filled(egui::Rect::from_min_size(pos(x, -6.2), egui::vec2(3.5, 12.4)), 1.0, color);
            }
        }
        Icon::Replay => {
            let points = (0..=20).map(|i| {
                let a = 3.72 + i as f32 * 0.245;
                pos(a.cos() * 5.6, a.sin() * 5.6)
            }).collect();
            painter.add(egui::Shape::line(points, stroke));
            painter.add(egui::Shape::line(vec![pos(-5.3, -7.1), pos(-5.3, -2.5), pos(-0.9, -2.5)], stroke));
        }
        Icon::Back | Icon::Forward => {
            let sign = if matches!(icon, Icon::Back) { -1.0 } else { 1.0 };
            for x in [-3.0, 2.6] {
                painter.add(egui::Shape::line(vec![pos(x - sign * 2.4, -4.6), pos(x + sign * 2.4, 0.0), pos(x - sign * 2.4, 4.6)], stroke));
            }
        }
        Icon::Speaker { muted } => {
            painter.add(egui::Shape::closed_line(vec![pos(-7.5, -2.7), pos(-4.6, -2.7), pos(-1.0, -6.0), pos(-1.0, 6.0), pos(-4.6, 2.7), pos(-7.5, 2.7)], stroke));
            if muted {
                painter.line_segment([pos(2.0, -2.5), pos(7.0, 2.5)], stroke);
                painter.line_segment([pos(2.0, 2.5), pos(7.0, -2.5)], stroke);
            } else {
                for offset in [0.0, 3.0] {
                    painter.add(egui::Shape::line(vec![pos(1.6 + offset, -2.6 - offset * 0.55), pos(3.4 + offset, 0.0), pos(1.6 + offset, 2.6 + offset * 0.55)], stroke));
                }
            }
        }
    }
}
