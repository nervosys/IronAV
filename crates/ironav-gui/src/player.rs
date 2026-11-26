//! The player window: picture, transport bar, and an "Agent" panel showing the live status
//! JSON an agent would get from the same player.

use crate::gpu;
use crate::picker::Picker;
use crate::player_style::{self, transport, Icon, ACCENT};
use crate::subtitles::Subtitles;
use eframe::egui::{self, Color32, RichText};
use ironav_core::Error;
use ironav_engine::desktop::{Link, CONNECTION_ENV};
use ironav_formats::FORMATS;
use ironav_player::{rgb_frame, to_png, AudioMode, OpenOptions, Player, SeekMode, State, TimedFrame};
use serde_json::Value;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const BAD: Color32 = Color32::from_rgb(248, 81, 73);
const OPEN_MEDIA: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
/// While the seek bar is dragged, a keyframe seek is sent at most this often.
const SCRUB_EVERY: Duration = Duration::from_millis(40);

/// A file being opened on another thread, so the window stays alive meanwhile.
struct Opening {
    path: String,
    rx: Receiver<ironav_core::Result<Player>>,
}

enum FilePurpose {
    Media,
    Subtitle(Weak<Player>),
}

pub struct PlayerApp {
    ctx: egui::Context,
    /// Pictures are shown by the GPU (planes uploaded, converted in a shader) when this is set and
    /// the picture's format allows; otherwise they are converted on the CPU into `tex`.
    gpu: bool,
    link: Option<Link>,
    player: Option<Arc<Player>>,
    subtitles: Subtitles,
    /// The file being opened; the result arrives here and replaces `player`.
    opening: Option<Opening>,
    /// A newer selection superseding the open in flight. Options overwrite rather than queue.
    pending: Option<String>,
    picker: Picker<FilePurpose>,
    snapshot: Option<Receiver<ironav_core::Result<String>>>,
    /// The picture last shown, kept so a seek does not flash black while the next one decodes.
    last: Option<Arc<TimedFrame>>,
    path: String,
    error: Option<Error>,
    tex: Option<egui::TextureHandle>,
    /// What the texture currently holds: frame identity and the size it was converted to.
    shown: Option<(usize, u32, u32)>,
    /// Position being dragged on the seek bar, as a fraction.
    scrub: Option<f32>,
    /// When and where the last keyframe seek of a drag was sent.
    scrub_sent: Option<(Instant, f32)>,
    show_agent: bool,
    note: Option<(String, std::time::Instant)>,
    fullscreen: bool,
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl PlayerApp {
    pub fn new(initial: Option<String>, gpu: bool, ctx: egui::Context) -> Self {
        player_style::install(&ctx);
        let mut a = Self { ctx, gpu, last: None, link: None, player: None, subtitles: Subtitles::default(), opening: None, pending: None, picker: Picker::default(), snapshot: None, path: String::new(), error: None, tex: None, shown: None, scrub: None, scrub_sent: None, show_agent: false, note: None, fullscreen: false };
        if let Some(p) = initial {
            a.open(&p);
        }
        a
    }

    fn open(&mut self, path: &str) {
        let path = path.to_owned();
        if self.opening.is_some() {
            self.pending = Some(path);
            return;
        }
        let old = self.player.take();
        self.subtitles.clear();
        self.tex = None;
        self.shown = None;
        self.last = None;
        self.scrub = None;
        self.scrub_sent = None;
        self.error = None;
        self.start_open(path, old);
    }

    fn start_open(&mut self, path: String, old: Option<Arc<Player>>) {
        let (tx, rx) = channel();
        let (p, ctx) = (path.clone(), self.ctx.clone());
        let detach = self.link.as_ref().map(Link::detach_player);
        let spawned = std::thread::Builder::new().name("ironav-open".into()).spawn(move || {
            if let Some(finish) = detach {
                finish();
            }
            // Joining the old decoder can wait on a disk or hardware. Finish it here before
            // opening the next one so they never compete for the same device.
            drop(old);
            let r = Player::open(&p, &OpenOptions { audio: AudioMode::Auto, autoplay: false, ..Default::default() });
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => self.opening = Some(Opening { path, rx }),
            Err(e) => self.error = Some(Error::new(ironav_core::ErrorCode::Internal, format!("cannot start the open: {e}"))),
        }
    }

    /// Take the player the open thread has finished, if it has.
    fn finish_open(&mut self) {
        let Some(o) = &self.opening else { return };
        let r = match o.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(Error::new(ironav_core::ErrorCode::Internal, "the open stopped without a result")),
        };
        let path = std::mem::take(&mut self.opening.as_mut().unwrap().path);
        self.opening = None;
        if let Some(next) = self.pending.take() {
            // A superseded result is never displayed. Dispose of its decoder on the worker.
            self.start_open(next, r.ok().map(Arc::new));
            return;
        }
        match r {
            Ok(p) => {
                // Repaint the moment a decoder thread queues a picture or fails, not on a timer.
                let ctx = self.ctx.clone();
                p.set_waker(Arc::new(move || ctx.request_repaint()));
                self.path = path;
                self.error = None;
                let p = Arc::new(p);
                p.play();
                if let Some(link) = &self.link {
                    link.set_player(Some(&p));
                }
                self.player = Some(p);
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn pick(&mut self) {
        let exts: Vec<&str> = FORMATS.iter().filter(|f| f.demux).flat_map(|f| f.extensions.iter().copied()).collect();
        let mut d = rfd::AsyncFileDialog::new().set_title("Open media").add_filter("Media files", &exts).add_filter("All files", &["*"]);
        if let Some(dir) = std::path::Path::new(&self.path).parent().filter(|d| !d.as_os_str().is_empty()) {
            d = d.set_directory(dir);
        }
        self.picker.open(FilePurpose::Media, d, &self.ctx);
    }

    fn say(&mut self, m: impl Into<String>) {
        self.note = Some((m.into(), std::time::Instant::now()));
    }

    fn agent_connection(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(link) = &self.link {
                if ui.button("Copy MCP configuration").clicked() {
                    let name = if cfg!(windows) { "ironav.exe" } else { "ironav" };
                    let command = std::env::current_exe().ok().map(|p| p.with_file_name(name))
                        .filter(|p| p.is_file()).map_or_else(|| name.to_owned(), |p| p.to_string_lossy().into_owned());
                    let config = serde_json::json!({ "mcpServers": { "ironav-player": {
                        "command": command, "args": ["mcp", "--attach"], "env": { (CONNECTION_ENV): link.connection_value() }
                    }}});
                    ui.ctx().copy_text(serde_json::to_string_pretty(&config).unwrap_or_default());
                    self.say("Copied MCP configuration");
                }
                if ui.button("Disable MCP").clicked() {
                    self.link = None;
                }
            } else if ui.button("Enable MCP").on_hover_text("Let a local MCP client inspect and control this player").clicked() {
                let ctx = self.ctx.clone();
                match Link::start(Arc::new(move || ctx.request_repaint())) {
                    Ok(link) => {
                        link.set_player(self.player.as_ref());
                        self.link = Some(link);
                    }
                    Err(e) => self.say(format!("Could not enable MCP ({})", e.message)),
                }
            }
        });
    }

    fn snapshot(&mut self) {
        let Some(p) = &self.player else { return };
        if self.snapshot.is_some() {
            return;
        }
        if !p.has_video() {
            self.say("This file has no video");
            return;
        }
        let Some(frame) = self.last.clone().or_else(|| p.frame()) else {
            self.say("No video frame is available yet");
            return;
        };
        let path = std::path::PathBuf::from(&self.path);
        let (send, result) = channel();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name("ironav-snapshot".into()).spawn(move || {
            let saved = (|| -> ironav_core::Result<String> {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("frame");
                let name = format!("{stem}-{}s.png", frame.time as u64);
                let target = path.with_file_name(name);
                let (w, h, rgb) = rgb_frame(&frame.frame, None, None);
                std::fs::write(&target, to_png(w, h, &rgb)?)?;
                Ok(format!("Saved {}", target.display()))
            })();
            let _ = send.send(saved);
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => self.snapshot = Some(result),
            Err(e) => self.say(format!("Could not start the snapshot ({e})")),
        }
    }

    fn finish_snapshot(&mut self) {
        let Some(result) = &self.snapshot else { return };
        let note = match result.try_recv() {
            Ok(Ok(note)) => note,
            Ok(Err(e)) => format!("Could not save the snapshot ({})", e.message),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => "The snapshot stopped before it was saved".into(),
        };
        self.snapshot = None;
        self.say(note);
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.key_pressed(egui::Key::F)) {
            self.toggle_fullscreen();
        }
        let Some(p) = &self.player else { return };
        let (toggle, left, right, up, down, mute, snap, home) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Home),
            )
        });
        let vol = p.volume();
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
            p.set_muted(!p.muted());
        }
        if snap {
            self.snapshot();
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(ui.visuals().panel_fill)
            .stroke(ui.visuals().window_stroke)
            .inner_margin(egui::Margin::symmetric(12, 6));
        let menu = egui::MenuBar::new().style(|style: &mut egui::Style| {
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        });
        egui::Panel::top("bar").frame(frame).show(ui, |ui| {
            menu.ui(ui, |ui| {
                ui.label(RichText::new("IronAV Player").size(17.0).strong());
                ui.separator();
                ui.menu_button("File", |ui| {
                    ui.set_min_width(172.0);
                    let shortcut = ui.ctx().format_shortcut(&OPEN_MEDIA);
                    let open = egui::Button::new("Open…").shortcut_text(shortcut);
                    if ui.add_enabled(!self.picker.busy(), open).clicked() {
                        ui.close();
                        self.pick();
                    }

                    let snapshot = egui::Button::new("Save snapshot").shortcut_text("S");
                    let can_snapshot = self.snapshot.is_none() && self.player.as_deref().is_some_and(Player::has_video);
                    if ui.add_enabled(can_snapshot, snapshot).clicked() {
                        ui.close();
                        self.snapshot();
                    }
                });
                let view_label = if self.link.is_some() { "View (MCP on)" } else { "View" };
                ui.menu_button(view_label, |ui| {
                    ui.set_min_width(172.0);
                    if ui.add(egui::Button::selectable(self.fullscreen, "Fullscreen").shortcut_text("F")).clicked() {
                        ui.close();
                        self.toggle_fullscreen();
                    }
                    if ui.add(egui::Button::selectable(self.show_agent, "Agent view")).clicked() {
                        ui.close();
                        self.show_agent = !self.show_agent;
                        self.ctx.request_repaint();
                    }
                });
                if let Some(p) = self.player.as_ref().filter(|p| p.has_video()) {
                    if let Some(dialog) = self.subtitles.menu(ui, p, !self.picker.busy()) {
                        self.picker.open(FilePurpose::Subtitle(Arc::downgrade(p)), dialog, ui.ctx());
                    }
                } else {
                    ui.add_enabled(false, egui::Button::new("Subtitles"));
                }

                let note = self.note.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(4)).map(|(message, _)| message.as_str());
                let file_name = std::path::Path::new(&self.path).file_name().and_then(|name| name.to_str());
                if let Some(text) = note.or(file_name) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).weak()).truncate()).on_hover_text(text);
                    });
                }
            });
        });
    }

    /// CPU fallback: bring the texture up to date with the picture that should be on screen.
    fn update_texture(&mut self, ctx: &egui::Context, f: &TimedFrame, key_id: usize, avail: egui::Vec2) {
        let ppp = ctx.pixels_per_point();
        let (mw, mh) = ((avail.x * ppp).max(2.0) as u32, (avail.y * ppp).max(2.0) as u32);
        let key = (key_id, mw, mh);
        if self.shown == Some(key) {
            return;
        }
        let img = picture(f, mw, mh);
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
        // While the bar is dragged, show the keyframe at the pointer (quick); on release, seek to the exact frame.
        if resp.dragged() {
            if let (Some(f), Some(d)) = (self.scrub, dur) {
                let due = self.scrub_sent.is_none_or(|(t, last)| last != f && t.elapsed() >= SCRUB_EVERY);
                if due {
                    p.seek_mode(f as f64 * d, SeekMode::Keyframe);
                    self.scrub_sent = Some((Instant::now(), f));
                }
            }
        }
        if (resp.drag_stopped() || resp.clicked()) && self.scrub.is_some() {
            if let (Some(f), Some(d)) = (self.scrub.take(), dur) {
                p.seek(f as f64 * d);
            }
            self.scrub_sent = None;
        }
        let shown_frac = self.scrub.map(f64::from).or_else(|| dur.filter(|d| *d > 0.0).map(|d| (pos / d).clamp(0.0, 1.0))).unwrap_or(0.0) as f32;
        let bar = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 5.0));
        let painter = ui.painter();
        painter.rect_filled(bar, 3.0, ui.visuals().widgets.inactive.bg_fill);
        painter.rect_filled(egui::Rect::from_min_max(bar.min, egui::pos2(bar.left() + bar.width() * shown_frac, bar.bottom())), 3.0, ACCENT);
        let radius = if resp.hovered() || resp.dragged() { 6.5 } else { 5.0 };
        painter.circle_filled(egui::pos2(bar.left() + bar.width() * shown_frac, bar.center().y), radius, ACCENT);

        ui.horizontal(|ui| {
            let (icon, label) = match state {
                State::Playing => (Icon::Pause, "Pause"),
                State::Ended => (Icon::Replay, "Replay"),
                _ => (Icon::Play, "Play"),
            };
            if transport(ui, icon, label, true, false).clicked() {
                p.toggle();
            }
            if transport(ui, Icon::Back, "Back 10 seconds", false, false).clicked() {
                p.seek_by(-10.0);
            }
            if transport(ui, Icon::Forward, "Forward 10 seconds", false, false).clicked() {
                p.seek_by(10.0);
            }
            let shown = self.scrub.zip(dur).map_or(pos, |(f, d)| f as f64 * d);
            ui.monospace(format!("{} / {}", fmt_time(shown), dur.map_or("?".into(), fmt_time)));
            if state == State::Buffering {
                ui.weak("buffering…");
            }
            ui.separator();
            let mut vol = p.volume() * 100.0;
            let muted = p.muted();
            if transport(ui, Icon::Speaker { muted }, if muted { "Unmute" } else { "Mute" }, false, muted).clicked() {
                p.set_muted(!muted);
            }
            if ui.add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").integer().show_value(true)).changed() {
                p.set_volume(vol / 100.0);
            }
            let looping = p.looping();
            if ui.add(egui::Button::selectable(looping, "Repeat")).on_hover_text("Repeat playback").clicked() {
                p.set_looping(!looping);
            }
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
        if !self.picker.busy() && ctx.input_mut(|input| input.consume_shortcut(&OPEN_MEDIA)) {
            self.pick();
        }
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.open(&p.display().to_string());
        }
        if let Some((purpose, path)) = self.picker.poll() {
            match purpose {
                FilePurpose::Media => self.open(&path.display().to_string()),
                FilePurpose::Subtitle(player) if self.player.as_ref().is_some_and(|p| Arc::as_ptr(p) == player.as_ptr()) => self.subtitles.open_file(path),
                FilePurpose::Subtitle(_) => {}
            }
        }
        self.finish_open();
        self.finish_snapshot();
        self.keys(&ctx);
        self.subtitles.update(&ctx);
        // The status JSON is built at most once per UI frame, and only for what shows it: the Agent
        // view and the audio-only level meter.
        let status: Option<Value> = self.player.as_deref().filter(|p| self.show_agent || !p.has_video()).map(Player::status);

        self.menu_bar(ui);

        egui::Panel::bottom("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.add_space(2.0);
            if self.show_agent {
                ui.separator();
                self.agent_connection(ui);
                if let Some(st) = &status {
                    let mut json = serde_json::to_string_pretty(st).unwrap_or_default();
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut json).code_editor().desired_width(f32::INFINITY).interactive(false));
                    });
                }
            }
        });

        let mut next_subtitle = None;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(ui, |ui| {
            let avail = ui.available_size();
            let full = ui.max_rect();
            let fresh = self.player.as_deref().and_then(Player::frame);
            let is_fresh = fresh.is_some();
            if is_fresh {
                self.last.clone_from(&fresh);
            }
            let mut picture_rect = None;
            if let Some(f) = fresh.or_else(|| self.last.clone()) {
                if self.gpu && gpu::supported(&f.frame) {
                    // The planes go to the GPU as they are; the shader converts and scales.
                    let size = egui::vec2(f.frame.width as f32, f.frame.height as f32);
                    let rect = egui::Rect::from_center_size(full.center(), size * (avail.x / size.x).min(avail.y / size.y));
                    ui.painter().add(gpu::VideoCallback::shape(rect, f));
                    self.tex = None;
                    self.shown = None;
                    picture_rect = Some(rect);
                } else {
                    self.update_texture(&ctx, &f, Arc::as_ptr(&f) as usize, avail);
                    if let Some(t) = &self.tex {
                        let sz = t.size_vec2();
                        let scale = (avail.x / sz.x).min(avail.y / sz.y);
                        let rect = egui::Rect::from_center_size(full.center(), sz * scale);
                        ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                        picture_rect = Some(rect);
                    }
                }
            }
            if let Some(rect) = picture_rect {
                if let Some(p) = self.player.as_ref().filter(|_| is_fresh) {
                    next_subtitle = self.subtitles.paint(ui, rect, p.position());
                }
            } else if let Some(p) = &self.player {
                if !p.has_video() {
                    if let Some(st) = &status {
                        audio_only(ui, full, p, st);
                    }
                }
            } else if let Some(o) = &self.opening {
                let path = self.pending.as_deref().unwrap_or(&o.path);
                let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((avail.y / 2.0 - 30.0).max(0.0));
                        ui.add(egui::Spinner::new().size(28.0).color(ACCENT));
                        ui.label(RichText::new(format!("Opening {name}…")).color(Color32::GRAY).size(16.0));
                    });
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Drop a file here or choose File → Open…").color(Color32::GRAY).size(18.0));
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

        self.schedule_repaint(&ctx, next_subtitle);
    }
}

impl PlayerApp {
    /// Ask for the next repaint only when something will have changed by then: the next picture
    /// is due, the clock text and seek bar need a tick, or a message expires. A paused player, an
    /// ended one and an empty window ask for nothing and stay idle until input or a decoder
    /// thread wakes them.
    fn schedule_repaint(&self, ctx: &egui::Context, next_subtitle: Option<f64>) {
        let mut wait: Option<Duration> = None;
        let mut soonest = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        if let Some(p) = &self.player {
            match p.state() {
                State::Playing => {
                    // The clock and seek bar move at 20 Hz; audio-only files also drive a level meter.
                    soonest(Duration::from_millis(if p.has_video() { 50 } else { 33 }));
                    if let Some(due) = p.next_frame_due() {
                        soonest(Duration::from_secs_f64(due).max(Duration::from_millis(1)));
                    }
                    if let Some(t) = next_subtitle {
                        let due = (t - p.position()).clamp(0.001, 0.05);
                        soonest(Duration::from_secs_f64(due));
                    }
                }
                State::Buffering => soonest(Duration::from_millis(50)),
                State::Ended if p.looping() => soonest(Duration::from_millis(50)),
                _ => {}
            }
        }
        if self.opening.is_some() || self.snapshot.is_some() {
            soonest(Duration::from_millis(50));
        }
        if let Some((_, t)) = &self.note {
            if let Some(left) = Duration::from_secs(4).checked_sub(t.elapsed()) {
                soonest(left + Duration::from_millis(20));
            }
        }
        if let Some(w) = wait {
            ctx.request_repaint_after(w);
        }
    }
}

/// A level meter for files with no picture.
fn audio_only(ui: &mut egui::Ui, rect: egui::Rect, p: &Player, st: &Value) {
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

#[cfg(test)]
mod tests;
