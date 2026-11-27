use eframe::egui::{self, Color32};
use std::io::Write;
use std::path::{Path, PathBuf};

mod palettes;
pub(crate) use palettes::THEMES;

pub(crate) const CONTROL_SIZE: f32 = 34.0;
pub(crate) const PLAY_WIDTH: f32 = 42.0;
pub(crate) const MENU_WIDTH: f32 = 184.0;
pub(crate) const PANE_PADDING: i8 = 12;
const CONTROL_RADIUS: u8 = 6;

pub(crate) struct Theme {
    pub(crate) name: &'static str,
    mode: egui::Theme,
    pub(crate) background: Color32,
    pub(crate) surface: Color32,
    elevated: Color32,
    pub(crate) border: Color32,
    focus: Color32,
    pub(crate) text: Color32,
    strong_text: Color32,
    pub(crate) muted_text: Color32,
    pub(crate) accent: Color32,
    accent_hover: Color32,
    selection: Color32,
    pub(crate) success: Color32,
    warning: Color32,
    pub(crate) danger: Color32,
    link: Color32,
    on_accent: Color32,
    on_accent_hover: Color32,
}

impl Theme {
    fn selection_text(&self) -> Color32 {
        if self.selection == self.accent { self.on_accent } else { self.strong_text }
    }
}

pub(crate) fn theme_file() -> Option<PathBuf> {
    let env_path = |key| std::env::var_os(key).map(PathBuf::from).filter(|path| path.is_absolute());
    let config = if cfg!(target_os = "windows") {
        env_path("APPDATA")
    } else if cfg!(target_os = "macos") {
        env_path("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        env_path("XDG_CONFIG_HOME").or_else(|| env_path("HOME").map(|home| home.join(".config")))
    }?;
    Some(config.join("IronAV").join("player-theme"))
}

pub(crate) fn load_theme(file: Option<&Path>) -> &'static Theme {
    file.and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|name| THEMES.iter().find(|theme| theme.name == name.trim()))
        .unwrap_or(&palettes::DEFAULT_THEME)
}

pub(crate) fn save_preference(file: &Path, value: &str) -> std::io::Result<()> {
    let directory = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    writeln!(temporary, "{value}")?;
    temporary.persist(file).map(|_| ()).map_err(|error| error.error)
}

pub(crate) fn install(ctx: &egui::Context, theme: &Theme) {
    ctx.set_theme(theme.mode);
    let mut style = (*ctx.style_of(theme.mode)).clone();
    style.visuals = match theme.mode {
        egui::Theme::Dark => egui::Visuals::dark(),
        egui::Theme::Light => egui::Visuals::light(),
    };
    let border = egui::Stroke::new(1.0, theme.border);
    style.visuals.panel_fill = theme.surface;
    style.visuals.window_fill = theme.elevated;
    style.visuals.window_stroke = border;
    style.visuals.window_corner_radius = egui::CornerRadius::same(8);
    style.visuals.extreme_bg_color = theme.background;
    style.visuals.faint_bg_color = theme.elevated;
    style.visuals.weak_text_color = Some(theme.muted_text);
    style.visuals.hyperlink_color = theme.link;
    style.visuals.warn_fg_color = theme.warning;
    style.visuals.error_fg_color = theme.danger;
    style.visuals.selection.bg_fill = theme.selection;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, theme.selection_text());
    // Interlinked's shared chrome uses 4/8/12 spacing and 6-point input corners.
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 5.0);
    style.spacing.interact_size.y = 28.0;
    let widgets = &mut style.visuals.widgets;
    widgets.noninteractive.bg_fill = theme.elevated;
    widgets.noninteractive.bg_stroke = border;
    widgets.noninteractive.fg_stroke.color = theme.text;
    let highlight = if theme.selection.is_opaque() {
        theme.selection.gamma_multiply(0.2)
    } else {
        theme.selection
    };
    for (widget, fill, text) in [
        (&mut widgets.inactive, Color32::TRANSPARENT, theme.text),
        (&mut widgets.hovered, highlight, theme.strong_text),
        (&mut widgets.active, highlight, theme.strong_text),
        (&mut widgets.open, highlight, theme.strong_text),
    ] {
        widget.bg_fill = theme.border;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = egui::Stroke::NONE;
        widget.fg_stroke.color = text;
        widget.corner_radius = egui::CornerRadius::same(CONTROL_RADIUS);
        widget.expansion = 0.0;
    }
    ctx.set_style_of(theme.mode, style);
}

pub(crate) fn bar_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(egui::Margin::symmetric(PANE_PADDING, 6))
}

pub(crate) fn menu_style(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(4.0, 2.0);
    style.spacing.button_padding = egui::vec2(8.0, 5.0);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
}

pub(crate) fn raised_controls(ui: &mut egui::Ui, theme: &Theme) {
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.inactive.weak_bg_fill = theme.elevated;
    widgets.inactive.bg_stroke = egui::Stroke::new(1.0, theme.border);
    for widget in [&mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
        widget.bg_stroke = egui::Stroke::new(1.0, theme.accent);
    }
}

pub(crate) enum Icon {
    Play,
    Pause,
    Replay,
    Back,
    Forward,
    Speed { reverse: bool },
    Speaker { muted: bool },
    Fullscreen { exit: bool },
}

pub(crate) fn transport(ui: &mut egui::Ui, theme: &Theme, icon: Icon, label: &str) -> egui::Response {
    let primary = matches!(icon, Icon::Play | Icon::Pause | Icon::Replay);
    let toggled = match icon {
        Icon::Speaker { muted } => Some(muted),
        _ => None,
    };
    let size = if matches!(icon, Icon::Fullscreen { .. }) {
        egui::Vec2::splat(ui.spacing().interact_size.y)
    } else {
        egui::vec2(if primary { PLAY_WIDTH } else { CONTROL_SIZE }, CONTROL_SIZE)
    };
    let button = egui::Button::new("").min_size(size).selected(toggled.unwrap_or(false));
    let (response, color) = if primary {
        ui.scope(|ui| {
            let widgets = &mut ui.visuals_mut().widgets;
            for (widget, fill, ink) in [
                (&mut widgets.inactive, theme.accent, theme.on_accent),
                (&mut widgets.hovered, theme.accent_hover, theme.on_accent_hover),
                (&mut widgets.active, theme.accent_hover, theme.on_accent_hover),
            ] {
                widget.bg_fill = fill;
                widget.weak_bg_fill = fill;
                widget.fg_stroke.color = ink;
                widget.bg_stroke = egui::Stroke::NONE;
            }
            let response = ui.add(button);
            let color = ui.style().interact(&response).fg_stroke.color;
            (response, color)
        }).inner
    } else {
        let response = ui.add(button);
        let color = if toggled == Some(true) { theme.selection_text() } else { ui.style().interact(&response).fg_stroke.color };
        (response, color)
    };
    response.widget_info(|| match toggled {
        Some(on) => egui::WidgetInfo::selected(egui::Role::Button, response.enabled(), on, label),
        None => egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), label),
    });
    paint_icon(ui.painter(), response.rect.center(), icon, color);
    if response.has_focus() {
        ui.painter().rect_stroke(response.rect, egui::CornerRadius::same(CONTROL_RADIUS), egui::Stroke::new(2.0, theme.focus), egui::StrokeKind::Inside);
    }
    response.on_hover_text(label).on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub(crate) fn speed_button(ui: &mut egui::Ui, theme: &Theme, reverse: bool, rate: Option<f64>) -> egui::Response {
    let label = if reverse { "Reverse playback speed" } else { "Forward playback speed" };
    let width = if rate.is_some() { 62.0 } else { CONTROL_SIZE };
    let response = ui.add(egui::Button::new("").min_size(egui::vec2(width, CONTROL_SIZE)));
    response.widget_info(|| {
        let name = rate.map_or_else(|| label.to_owned(), |r| format!("{label}, {r}×"));
        egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), name)
    });
    let color = if rate.is_some() { theme.accent } else { ui.style().interact(&response).fg_stroke.color };
    let center = if rate.is_some() {
        egui::pos2(response.rect.left() + CONTROL_SIZE / 2.0, response.rect.center().y)
    } else {
        response.rect.center()
    };
    paint_icon(ui.painter(), center, Icon::Speed { reverse }, color);
    if let Some(rate) = rate {
        ui.painter().text(
            egui::pos2(response.rect.right() - 8.0, center.y),
            egui::Align2::RIGHT_CENTER,
            format!("{rate}×"),
            egui::TextStyle::Button.resolve(ui.style()),
            theme.strong_text,
        );
    }
    response.on_hover_text(if reverse { "Reverse playback speed (J, silent)" } else { "Forward playback speed (L)" })
        .on_hover_cursor(egui::CursorIcon::PointingHand)
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
        Icon::Speed { reverse } => {
            let sign = if reverse { -1.0 } else { 1.0 };
            for x in [-4.0, 4.0] {
                painter.add(egui::Shape::convex_polygon(vec![
                    pos(x - sign * 3.4, -sign * 4.8),
                    pos(x + sign * 3.4, 0.0),
                    pos(x - sign * 3.4, sign * 4.8),
                ], color, egui::Stroke::NONE));
            }
        }
        Icon::Fullscreen { exit } => {
            let (corner, end) = if exit { (2.5, 6.5) } else { (6.5, 2.5) };
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                painter.add(egui::Shape::line(vec![pos(x * end, y * corner), pos(x * corner, y * corner), pos(x * corner, y * end)], stroke));
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
