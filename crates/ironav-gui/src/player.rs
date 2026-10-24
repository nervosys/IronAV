//! The player window: picture, transport bar, and an "Agent" panel showing the live status
//! JSON an agent would get from the same player.

use eframe::egui::{self, Color32, RichText};
use ironav_core::Error;
use ironav_formats::FORMATS;
use ironav_player::{rgb_frame, to_png, AudioMode, OpenOptions, Player, State, TimedFrame};
use std::sync::Arc;
use std::time::Duration;

const ACCENT: Color32 = Color32::from_rgb(249, 115, 22);
const BAD: Color32 = Color32::from_rgb(248, 81, 73);

pub struct PlayerApp {
    player: Option<Player>,
    path: String,
    error: Option<Error>,
    tex: Option<egui::TextureHandle>,
    /// What the texture currently holds: frame identity and the size it was converted to.
    shown: Option<(usize, u32, u32)>,
    /// Position being dragged on the seek bar, as a fraction.
    scrub: Option<f32>,
    show_agent: bool,
    note: Option<(String, std::time::Instant)>,
    fullscreen: bool,
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl PlayerApp {
    pub fn new(initial: Option<String>) -> Self {
        let mut a = Self { player: None, path: String::new(), error: None, tex: None, shown: None, scrub: None, show_agent: false, note: None, fullscreen: false };
        if let Some(p) = initial {
            a.open(&p);
        }
        a
    }

    fn open(&mut self, path: &str) {
        self.player = None; // stop the old one first so two decoders never compete for the GPU
        self.tex = None;
        self.shown = None;
        match Player::open(path, &OpenOptions { audio: AudioMode::Auto, ..Default::default() }) {
            Ok(p) => {
                self.path = path.to_string();
                self.error = None;
                self.player = Some(p);
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn pick(&mut self) {
        let exts: Vec<&str> = FORMATS.iter().filter(|f| f.demux).flat_map(|f| f.extensions.iter().copied()).collect();
        let mut d = rfd::FileDialog::new().set_title("Open media").add_filter("Media files", &exts).add_filter("All files", &["*"]);
        if let Some(dir) = std::path::Path::new(&self.path).parent().filter(|d| d.is_dir()) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.pick_file() {
            self.open(&p.display().to_string());
        }
    }

    fn say(&mut self, m: impl Into<String>) {
        self.note = Some((m.into(), std::time::Instant::now()));
    }

    fn snapshot(&mut self) {
        let Some(p) = &self.player else { return };
        let r = p.snapshot_png(None, None);
        match r {
            Ok((_, _, png)) => {
                let stem = std::path::Path::new(&self.path).file_stem().and_then(|s| s.to_str()).unwrap_or("frame").to_string();
                let name = format!("{stem}-{}s.png", p.position() as u64);
                let dir = std::path::Path::new(&self.path).parent().filter(|d| d.is_dir()).map(|d| d.to_path_buf()).unwrap_or_default();
                let target = dir.join(&name);
                match std::fs::write(&target, png) {
                    Ok(()) => self.say(format!("saved {}", target.display())),
                    Err(e) => self.say(format!("cannot save: {e}")),
                }
            }
            Err(e) => self.say(e.message),
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.player else { return };
        let (toggle, left, right, up, down, mute, full, snap, home) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Home),
            )
        });
        let vol = p.status()["volume"].as_f64().unwrap_or(1.0) as f32;
        if toggle {
            p.toggle();
        }
        if left {
            p.seek_by(-5.0);
        }
        if right {
            p.seek_by(5.0);
        }
        if home {
            p.seek(0.0);
        }
        if up {
            p.set_volume((vol + 0.05).min(2.0));
        }
        if down {
            p.set_volume((vol - 0.05).max(0.0));
        }
        if mute {
            let on = !p.status()["muted"].as_bool().unwrap_or(false);
            p.set_muted(on);
        }
        if full {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if snap {
            self.snapshot();
        }
    }

    /// Bring the texture up to date with the picture that should be on screen.
    fn update_texture(&mut self, ctx: &egui::Context, avail: egui::Vec2) {
        let Some(p) = &self.player else { return };
        let Some(f) = p.frame() else { return };
        let ppp = ctx.pixels_per_point();
        let (mw, mh) = ((avail.x * ppp).max(2.0) as u32, (avail.y * ppp).max(2.0) as u32);
        let key = (Arc::as_ptr(&f) as usize, mw, mh);
        if self.shown == Some(key) {
            return;
        }
        let img = picture(&f, mw, mh);
        match &mut self.tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR)),
        }
        self.shown = Some(key);
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.player else { return };
        let (pos, dur, state) = (p.position(), p.info().duration, p.state());

        // Seek bar.
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::click_and_drag());
        let frac_at = |x: f32| ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        if let Some(pt) = resp.interact_pointer_pos() {
            if resp.dragged() || resp.clicked() {
                self.scrub = Some(frac_at(pt.x));
            }
        }
        if (resp.drag_stopped() || resp.clicked()) && self.scrub.is_some() {
            if let (Some(f), Some(d)) = (self.scrub.take(), dur) {
                p.seek(f as f64 * d);
            }
        }
        let shown_frac = self.scrub.map(f64::from).or_else(|| dur.filter(|d| *d > 0.0).map(|d| (pos / d).clamp(0.0, 1.0))).unwrap_or(0.0) as f32;
        let bar = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 6.0));
        let painter = ui.painter();
        painter.rect_filled(bar, 3.0, ui.visuals().widgets.inactive.bg_fill);
        painter.rect_filled(egui::Rect::from_min_max(bar.min, egui::pos2(bar.left() + bar.width() * shown_frac, bar.bottom())), 3.0, ACCENT);
        painter.circle_filled(egui::pos2(bar.left() + bar.width() * shown_frac, bar.center().y), 7.0, ACCENT);

        ui.horizontal(|ui| {
            let label = match state {
                State::Playing => "⏸",
                State::Ended => "⟲",
                _ => "▶",
            };
            if ui.button(RichText::new(label).size(18.0)).clicked() {
                p.toggle();
            }
            if ui.button("⏪").on_hover_text("back 10 s").clicked() {
                p.seek_by(-10.0);
            }
            if ui.button("⏩").on_hover_text("forward 10 s").clicked() {
                p.seek_by(10.0);
            }
            let shown = self.scrub.zip(dur).map_or(pos, |(f, d)| f as f64 * d);
            ui.monospace(format!("{} / {}", fmt_time(shown), dur.map_or("?".into(), fmt_time)));
            if state == State::Buffering {
                ui.weak("buffering…");
            }
            ui.separator();
            let st = p.status();
            let mut vol = st["volume"].as_f64().unwrap_or(1.0) as f32 * 100.0;
            let muted = st["muted"].as_bool().unwrap_or(false);
            if ui.selectable_label(muted, if muted { "🔇" } else { "🔊" }).clicked() {
                p.set_muted(!muted);
            }
            if ui.add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").integer().show_value(true)).changed() {
                p.set_volume(vol / 100.0);
            }
            let mut looping = st["looping"].as_bool().unwrap_or(false);
            if ui.checkbox(&mut looping, "Repeat").changed() {
                p.set_looping(looping);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.show_agent, "Agent view");
            });
        });
    }
}

/// Convert a decoded frame for display, fitted into `max_w` x `max_h` device pixels.
fn picture(f: &TimedFrame, max_w: u32, max_h: u32) -> egui::ColorImage {
    let (w, h, rgb) = rgb_frame(&f.frame, Some(max_w), Some(max_h));
    egui::ColorImage::from_rgb([w as usize, h as usize], &rgb)
}

impl eframe::App for PlayerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.open(&p.display().to_string());
        }
        self.keys(&ctx);

        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("IronAV Player").color(ACCENT));
                ui.separator();
                if ui.button("Open…").clicked() {
                    self.pick();
                }
                if ui.add_enabled(self.player.as_ref().is_some_and(Player::has_video), egui::Button::new("Snapshot")).clicked() {
                    self.snapshot();
                }
                if let Some((m, t)) = &self.note {
                    if t.elapsed() < Duration::from_secs(4) {
                        ui.weak(m);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(std::path::Path::new(&self.path).file_name().and_then(|n| n.to_str()).unwrap_or(""));
                });
            });
        });

        egui::Panel::bottom("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.add_space(2.0);
            if self.show_agent {
                if let Some(p) = &self.player {
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.weak("same player, as an agent sees it:");
                        ui.monospace(format!("ironav play \"{}\" --json   then   {{\"cmd\":\"status\"}}", self.path));
                    });
                    let mut json = serde_json::to_string_pretty(&p.status()).unwrap_or_default();
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut json).code_editor().desired_width(f32::INFINITY).interactive(false));
                    });
                }
            }
        });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(ui, |ui| {
            let avail = ui.available_size();
            self.update_texture(&ctx, avail);
            let full = ui.max_rect();
            if let Some(t) = &self.tex {
                let sz = t.size_vec2();
                let scale = (avail.x / sz.x).min(avail.y / sz.y);
                let rect = egui::Rect::from_center_size(full.center(), sz * scale);
                ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            } else if let Some(p) = &self.player {
                if !p.has_video() {
                    audio_only(ui, full, p);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Drop a file here, or choose Open…").color(Color32::GRAY).size(18.0));
                });
            }
            if let Some(e) = &self.error {
                ui.painter().text(full.center_bottom() - egui::vec2(0.0, 24.0), egui::Align2::CENTER_BOTTOM, format!("{} — {}", e.message, e.hint.clone().unwrap_or_default()), egui::FontId::proportional(14.0), BAD);
            }
            if let Some(p) = &self.player {
                if let Some(e) = p.error() {
                    ui.painter().text(full.center_bottom() - egui::vec2(0.0, 24.0), egui::Align2::CENTER_BOTTOM, e.message, egui::FontId::proportional(14.0), BAD);
                }
            }
        });

        ctx.request_repaint_after(Duration::from_millis(if self.player.as_ref().is_some_and(|p| p.state() == State::Playing) { 8 } else { 100 }));
    }
}

/// A level meter for files with no picture.
fn audio_only(ui: &mut egui::Ui, rect: egui::Rect, p: &Player) {
    let st = p.status();
    let level = st["audio"]["level"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0).sqrt() as f32;
    let meter = egui::Rect::from_center_size(rect.center(), egui::vec2((rect.width() * 0.6).max(40.0), 14.0));
    let painter = ui.painter();
    painter.rect_filled(meter, 4.0, Color32::from_gray(40));
    painter.rect_filled(egui::Rect::from_min_size(meter.min, egui::vec2(meter.width() * level, meter.height())), 4.0, Color32::from_rgb(63, 185, 80));
    let name = std::path::Path::new(&p.info().path).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    painter.text(meter.center_top() - egui::vec2(0.0, 12.0), egui::Align2::CENTER_BOTTOM, name, egui::FontId::proportional(18.0), Color32::LIGHT_GRAY);
    painter.text(
        meter.center_bottom() + egui::vec2(0.0, 12.0),
        egui::Align2::CENTER_TOP,
        format!("{} Hz · {} ch · {}", st["audio"]["sample_rate"], st["audio"]["channels"], st["audio"]["device"].as_str().unwrap_or("")),
        egui::FontId::proportional(13.0),
        Color32::GRAY,
    );
}

#[allow(dead_code)]
fn _uses(w: u32, h: u32, rgb: &[u8]) -> ironav_core::Result<Vec<u8>> {
    to_png(w, h, rgb)
}
