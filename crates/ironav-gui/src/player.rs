//! The player window: picture, transport bar, and an "Agent" panel showing the live status
//! JSON an agent would get from the same player.

use crate::audio_tools::AudioTools;
use crate::gpu;
use crate::picker::Picker;
use crate::player_style::{self, transport, Icon, Theme, CONTROL_SIZE, MENU_WIDTH, PANE_PADDING, PLAY_WIDTH, THEMES};
use crate::subtitles::Subtitles;
use eframe::egui::{self, Color32, RichText};
use ironav_core::Error;
use ironav_engine::desktop::{Link, CONNECTION_ENV};
use ironav_formats::FORMATS;
use ironav_player::{rgb_frame_with, to_png, AudioMode, OpenOptions, Player, SeekMode, State, TimedFrame};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const OPEN_MEDIA: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
const PLAYBACK_SPEEDS: &[f64] = &[0.5, 1.0, 1.5, 2.0, 4.0];
const SEEK_HEIGHT: f32 = 16.0;
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
    show_audio_tools: bool,
    audio_tools: AudioTools,
    note: Option<(String, std::time::Instant)>,
    fullscreen: bool,
    plot_fullscreen: bool,
    theme: &'static Theme,
    theme_file: Option<PathBuf>,
    theme_changed: bool,
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl PlayerApp {
    pub(crate) fn with_audio_gpu(mut self, enabled: bool) -> Self {
        self.audio_tools.enable_gpu(enabled);
        self
    }

    pub fn new(initial: Option<String>, gpu: bool, ctx: egui::Context, theme_file: Option<PathBuf>) -> Self {
        let theme = player_style::load_theme(theme_file.as_deref());
        let audio_tools = AudioTools::new(theme_file.as_ref().map(|file| file.with_file_name("audio-visualizer")));
        player_style::install(&ctx, theme);
        let mut a = Self {
            ctx, gpu, last: None, link: None, player: None, subtitles: Subtitles::default(),
            opening: None, pending: None, picker: Picker::default(), snapshot: None, path: String::new(),
            error: None, tex: None, shown: None, scrub: None, scrub_sent: None, show_agent: false,
            show_audio_tools: false, audio_tools, note: None, fullscreen: false, plot_fullscreen: false,
            theme, theme_file, theme_changed: false,
        };
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
        self.set_plot_fullscreen(false);
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
                self.audio_tools.attach(&p, self.show_audio_tools || !p.has_video());
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
        ui.horizontal_wrapped(|ui| {
            player_style::raised_controls(ui, self.theme);
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
                let (w, h, rgb) = rgb_frame_with(&frame.frame, frame.color, None, None);
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

    fn set_plot_fullscreen(&mut self, fullscreen: bool) {
        if fullscreen == self.plot_fullscreen {
            return;
        }
        let player = self.player.as_deref().filter(|p| p.has_audio());
        if fullscreen && player.is_none() {
            return;
        }
        self.plot_fullscreen = fullscreen;
        if let Some(player) = player {
            self.audio_tools.set_visible(player, fullscreen || self.show_audio_tools || !player.has_video());
        }
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen || self.fullscreen));
    }

    fn toggle_agent_view(&mut self) {
        self.show_agent = !self.show_agent;
        self.ctx.request_repaint();
    }

    fn set_color_theme(&mut self, theme: &'static Theme) {
        self.theme_changed |= self.theme.name != theme.name;
        self.theme = theme;
        player_style::install(&self.ctx, theme);
        self.ctx.request_repaint();
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if self.picker.busy() || egui::Popup::is_any_open(ctx) {
            return;
        }
        if self.plot_fullscreen && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.set_plot_fullscreen(false);
            return;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::F)) {
            if self.plot_fullscreen {
                self.set_plot_fullscreen(false);
            } else {
                self.toggle_fullscreen();
            }
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
        let frame = player_style::bar_frame(ui);
        let height = frame.total_margin().sum().y + ui.spacing().interact_size.y;
        let config = egui::containers::menu::MenuConfig::new().style(player_style::menu_style);
        let menu = egui::MenuBar::new().style(player_style::menu_style).config(config);
        egui::Panel::top("bar").exact_size(height).resizable(false).frame(frame).show(ui, |ui| {
            menu.ui(ui, |ui| {
                let can_snapshot = self.snapshot.is_none() && self.player.as_deref().is_some_and(Player::has_video);
                ui.menu_button("File", |ui| {
                    ui.set_min_width(MENU_WIDTH);
                    let shortcut = ui.ctx().format_shortcut(&OPEN_MEDIA);
                    let open = egui::Button::new("Open…").shortcut_text(shortcut);
                    if ui.add_enabled(!self.picker.busy(), open).clicked() {
                        ui.close();
                        self.pick();
                    }

                    let snapshot = egui::Button::new("Save snapshot").shortcut_text("S");
                    if ui.add_enabled(can_snapshot, snapshot).clicked() {
                        ui.close();
                        self.snapshot();
                    }
                });
                let view_label = if self.link.is_some() { "View (MCP on)" } else { "View" };
                ui.menu_button(view_label, |ui| {
                    ui.set_min_width(MENU_WIDTH);
                    if ui.add(egui::Button::selectable(self.fullscreen, "Fullscreen").shortcut_text("F")).clicked() {
                        ui.close();
                        self.toggle_fullscreen();
                    }
                    if ui.add(egui::Button::selectable(self.show_agent, "Agent view")).clicked() {
                        ui.close();
                        self.toggle_agent_view();
                    }
                    let video_with_audio = self.player.as_deref().filter(|p| p.has_video() && p.has_audio());
                    let audio_tools = egui::Button::selectable(self.show_audio_tools, "Audio tools");
                    if ui.add_enabled(video_with_audio.is_some(), audio_tools).clicked() {
                        self.show_audio_tools = !self.show_audio_tools;
                        if let Some(player) = video_with_audio {
                            self.audio_tools.set_visible(player, self.show_audio_tools);
                        }
                        ui.close();
                    }
                    let has_audio = self.player.as_deref().is_some_and(Player::has_audio);
                    if ui.add_enabled(has_audio, egui::Button::new("Full screen audio plot")).clicked() {
                        ui.close();
                        self.set_plot_fullscreen(true);
                    }
                    ui.menu_button("Theme", |ui| {
                        ui.set_min_width(MENU_WIDTH);
                        let height = (ui.ctx().content_rect().height() * 0.58).clamp(100.0, 460.0);
                        egui::ScrollArea::vertical().max_height(height).show(ui, |ui| {
                            for theme in THEMES {
                                if ui.add(egui::Button::selectable(self.theme.name == theme.name, theme.name)).clicked() {
                                    self.set_color_theme(theme);
                                    ui.close();
                                }
                            }
                        });
                    });
                });
                if let Some(p) = self.player.as_ref().filter(|p| p.has_video()) {
                    if let Some(dialog) = self.subtitles.menu(ui, p, !self.picker.busy()) {
                        self.picker.open(FilePurpose::Subtitle(Arc::downgrade(p)), dialog, ui.ctx());
                    }
                } else {
                    ui.add_enabled(false, egui::Button::new("Subtitles"));
                }

                ui.add_space(8.0);
                ui.scope(|ui| {
                    ui.spacing_mut().button_padding.x = 8.0;
                    player_style::raised_controls(ui, self.theme);
                    if ui.add_enabled(!self.picker.busy(), egui::Button::new("Open…")).clicked() {
                        self.pick();
                    }
                    if ui.add_enabled(can_snapshot, egui::Button::new("Snapshot")).clicked() {
                        self.snapshot();
                    }
                    if ui.add(egui::Button::selectable(self.show_agent, "Agent view")).clicked() {
                        self.toggle_agent_view();
                    }
                });

                let note = self.note.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(4)).map(|(message, _)| (message.as_str(), message.as_str()));
                let file_name = std::path::Path::new(&self.path).file_name().and_then(|name| name.to_str());
                let detail = note.or_else(|| self.subtitles.notice()).or_else(|| file_name.map(|name| (name, name)));
                if let Some((text, tooltip)) = detail {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).weak()).truncate()).on_hover_text(tooltip);
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

    fn set_playback_rate(&mut self, rate: f64) {
        if let Some(player) = &self.player {
            if let Err(error) = player.play_at_rate(rate) {
                self.say(error.message);
            }
        }
    }

    fn playback_shortcuts(&mut self, ctx: &egui::Context) {
        if self.player.is_none() || self.picker.busy() || ctx.egui_wants_keyboard_input() || egui::Popup::is_any_open(ctx) {
            return;
        }
        let keys = ctx.input_mut(|input| {
            let mut keys = Vec::new();
            input.events.retain(|event| {
                if let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = event {
                    if *modifiers == egui::Modifiers::NONE && matches!(key, egui::Key::J | egui::Key::K | egui::Key::L) {
                        if !repeat {
                            keys.push(*key);
                        }
                        return false;
                    }
                }
                true
            });
            keys
        });
        if !keys.is_empty() {
            ctx.request_repaint();
        }
        for key in keys {
            let Some(player) = &self.player else { return };
            let direction = match key {
                egui::Key::J if player.has_video() => -1.0,
                egui::Key::L => 1.0,
                egui::Key::K => {
                    player.pause();
                    continue;
                }
                _ => continue,
            };
            let rate = player.playback_rate();
            let speed = if rate * direction < 0.0 {
                1.0
            } else if matches!(player.state(), State::Paused | State::Ended) {
                rate.abs()
            } else {
                PLAYBACK_SPEEDS.iter().copied().find(|&speed| speed > rate.abs()).unwrap_or(rate.abs())
            };
            self.set_playback_rate(speed * direction);
        }
    }

    fn agent_panel(&mut self, ui: &mut egui::Ui) {
        let frame = player_style::bar_frame(ui);
        let status = self.player.as_deref().map(Player::status);
        // Keep the status to a compact scrollable row when space is needed by the plot and transport.
        let status_height = (ui.available_height() - 256.0).clamp(24.0, 160.0);
        let mut height = frame.total_margin().sum().y + ui.spacing().interact_size.y;
        if status.is_some() {
            height += ui.spacing().item_spacing.y + status_height;
        }
        egui::Panel::bottom("agent").exact_size(height).resizable(false).frame(frame).show(ui, |ui| {
            self.agent_connection(ui);
            if let Some(status) = &status {
                let mut json = serde_json::to_string_pretty(status).unwrap_or_default();
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().min_scrolled_height(0.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut json).code_editor().desired_width(f32::INFINITY).interactive(false));
                });
            }
        });
    }

    fn transport_panel(&self, ui: &egui::Ui) -> egui::Panel {
        let frame = player_style::bar_frame(ui);
        let spacing = ui.spacing();
        let height = if self.player.is_some() {
            let timeline_height = SEEK_HEIGHT.max(spacing.interact_size.y);
            frame.total_margin().sum().y + timeline_height + spacing.item_spacing.y + CONTROL_SIZE
        } else {
            0.0
        };
        egui::Panel::bottom("transport").exact_size(height).resizable(false).frame(frame)
    }

    fn audio_dock(&mut self, ui: &mut egui::Ui) {
        let Some(player) = self.player.as_deref().filter(|p| self.show_audio_tools && p.has_video() && p.has_audio()) else { return };
        let theme = self.theme;
        let frame = egui::Frame::new().fill(theme.surface).inner_margin(egui::Margin::same(PANE_PADDING));
        // Even a dock widened on a larger window leaves room for the picture after a resize.
        let max_width = (ui.available_width() - 190.0).clamp(270.0, 430.0);
        let expand = egui::Panel::right("video audio tools").default_size(314.0).size_range(270.0..=max_width)
            .frame(frame).show(ui, |ui| self.audio_tools.show(ui, player, theme, true));
        if expand.inner {
            self.set_plot_fullscreen(true);
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.player else { return };
        let theme = self.theme;
        let (pos, dur, state) = (p.position(), p.info().duration, p.state());
        let rate = p.playback_rate();

        let shown = self.scrub.zip(dur).map_or(pos, |(f, d)| f as f64 * d);
        let time = format!("{} / {}", fmt_time(shown), dur.map_or("?".into(), fmt_time));
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(time).monospace()).truncate());
            if state == State::Buffering {
                ui.add(egui::Label::new(RichText::new("Buffering…").small().weak()).truncate());
            }

            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), SEEK_HEIGHT), egui::Sense::click_and_drag());
            let track = egui::Rect::from_center_size(rect.center(), egui::vec2((rect.width() - 10.0).max(1.0), 4.0));
            let frac_at = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0);
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if let Some(pt) = resp.interact_pointer_pos() {
                if resp.dragged() || resp.clicked() {
                    self.scrub = Some(frac_at(pt.x));
                }
            }
            // Throttle drag previews, then seek to the exact position on release.
            if resp.dragged() {
                if let (Some(f), Some(d)) = (self.scrub, dur) {
                    let due = self.scrub_sent.is_none_or(|(t, last)| last != f && t.elapsed() >= SCRUB_EVERY);
                    if due {
                        let mode = if rate < 0.0 { SeekMode::Accurate } else { SeekMode::Keyframe };
                        p.seek_mode(f as f64 * d, mode);
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
            let painter = ui.painter();
            painter.rect_filled(track, 2.0, ui.visuals().widgets.inactive.bg_fill);
            let playhead = track.left() + track.width() * shown_frac;
            if shown_frac > 0.0 {
                painter.rect_filled(egui::Rect::from_min_max(track.min, egui::pos2(playhead, track.bottom())), 2.0, theme.accent);
            }
            if resp.hovered() || resp.dragged() || resp.has_focus() {
                painter.circle_filled(egui::pos2(playhead, track.center().y), 5.0, theme.accent);
            }
        });

        let gap = ui.spacing().item_spacing.x;
        let middle = PLAY_WIDTH + 2.0 * (CONTROL_SIZE + gap);
        let side = ((ui.available_width() - middle - 2.0 * gap) / 2.0).max(0.0);
        let side_group = egui::vec2(side, CONTROL_SIZE);
        let middle_group = egui::vec2(middle, CONTROL_SIZE);
        let left_to_right = egui::Layout::left_to_right(egui::Align::Center);
        let mut selected_rate = None;
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(side_group, left_to_right, |ui| {
                ui.set_min_size(side_group);
                ui.spacing_mut().item_spacing.x = 4.0;
                player_style::raised_controls(ui, theme);
                for direction in [-1.0, 1.0] {
                    let reverse = direction < 0.0;
                    let multiplier = (rate * direction > 0.0).then_some(rate.abs());
                    ui.add_enabled_ui(!reverse || p.has_video(), |ui| {
                        let response = player_style::speed_button(ui, theme, reverse, multiplier);
                        egui::Popup::menu(&response).show(|ui| {
                            for &speed in PLAYBACK_SPEEDS {
                                let signed_rate = speed * direction;
                                let item = egui::Button::selectable(rate == signed_rate, format!("{speed}×"));
                                if ui.add(item).clicked() {
                                    selected_rate = Some(signed_rate);
                                    ui.close();
                                }
                            }
                        });
                    });
                }
            });
            ui.allocate_ui_with_layout(middle_group, left_to_right, |ui| {
                ui.set_min_size(middle_group);
                player_style::raised_controls(ui, theme);
                if transport(ui, theme, Icon::Back, "Back 10 seconds").clicked() {
                    p.seek_by(-10.0);
                }
                let (icon, label) = match state {
                    State::Playing => (Icon::Pause, "Pause (K)"),
                    State::Ended => (Icon::Replay, "Replay"),
                    _ => (Icon::Play, "Play"),
                };
                if transport(ui, theme, icon, label).clicked() {
                    p.toggle();
                }
                if transport(ui, theme, Icon::Forward, "Forward 10 seconds").clicked() {
                    p.seek_by(10.0);
                }
            });
            let right_to_left = egui::Layout::right_to_left(egui::Align::Center);
            ui.allocate_ui_with_layout(side_group, right_to_left, |ui| {
                ui.set_min_size(side_group);
                player_style::raised_controls(ui, theme);
                let repeat_width = 54.0;
                let spacing = ui.spacing_mut();
                spacing.button_padding.x = 6.0;
                spacing.item_spacing.x = 4.0;
                // Reserve room for the editable percentage as well as Mute and Repeat.
                spacing.slider_width = (side - CONTROL_SIZE - repeat_width - 44.0 - 3.0 * spacing.item_spacing.x).clamp(32.0, 82.0);
                let enabled = p.looping();
                let repeat = egui::Button::selectable(enabled, "Repeat").min_size(egui::vec2(repeat_width, CONTROL_SIZE));
                if ui.add(repeat).clicked() {
                    p.set_looping(!enabled);
                }
                let mut vol = p.volume() * 100.0;
                let slider = ui.add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").integer().show_value(true));
                slider.widget_info(|| egui::WidgetInfo::slider(slider.enabled(), f64::from(vol), "Volume"));
                if slider.changed() {
                    p.set_volume(vol / 100.0);
                }
                slider.on_hover_text("Volume");
                let muted = p.muted();
                if transport(ui, theme, Icon::Speaker { muted }, if muted { "Unmute" } else { "Mute" }).clicked() {
                    p.set_muted(!muted);
                }
            });
        });
        if let Some(rate) = selected_rate {
            self.set_playback_rate(rate);
        }
    }
}

/// Convert a decoded frame for display, fitted into `max_w` x `max_h` device pixels.
fn picture(f: &TimedFrame, max_w: u32, max_h: u32) -> egui::ColorImage {
    let (w, h, rgb) = rgb_frame_with(&f.frame, f.color, Some(max_w), Some(max_h));
    egui::ColorImage::from_rgb([w as usize, h as usize], &rgb)
}

impl Drop for PlayerApp {
    fn drop(&mut self) {
        if self.theme_changed {
            if let Some(file) = &self.theme_file {
                // Save once at shutdown, outside the playback and paint paths.
                let _ = player_style::save_preference(file, self.theme.name);
            }
        }
    }
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
        if let Some(player) = self.player.as_deref().filter(|_| self.plot_fullscreen) {
            // Advance the same playback clock and release stale video frames while the plot fills the window.
            if let Some(frame) = player.frame() {
                self.last = Some(frame);
            }
            let frame = egui::Frame::new().fill(self.theme.background).inner_margin(egui::Margin::same(PANE_PADDING));
            let exit = egui::CentralPanel::default().frame(frame).show(ui, |ui| {
                self.audio_tools.show_fullscreen(ui, player, self.theme)
            });
            if exit.inner {
                self.set_plot_fullscreen(false);
            }
            self.playback_shortcuts(&ctx);
            self.schedule_repaint(&ctx, None);
            return;
        }
        self.subtitles.update(&ctx);

        self.menu_bar(ui);
        if self.show_agent {
            self.agent_panel(ui);
        }
        if self.player.is_some() {
            self.transport_panel(ui).show(ui, |ui| self.controls(ui));
        }

        // Let menus and text fields claim focus before handling the letter keys.
        self.playback_shortcuts(&ctx);

        self.audio_dock(ui);

        let mut next_subtitle = None;
        let theme = self.theme;
        let frame = if self.player.as_deref().is_some_and(Player::has_video) {
            egui::Frame::new().fill(Color32::BLACK)
        } else {
            egui::Frame::new().fill(theme.background).inner_margin(egui::Margin::same(PANE_PADDING))
        };
        let expand = egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            let mut expand = false;
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
                if !p.has_video() && p.has_audio() {
                    expand = self.audio_tools.show(ui, p, theme, false);
                }
            } else if let Some(o) = &self.opening {
                let path = self.pending.as_deref().unwrap_or(&o.path);
                let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((avail.y / 2.0 - 30.0).max(0.0));
                        ui.add(egui::Spinner::new().size(28.0).color(theme.accent));
                        ui.label(RichText::new(format!("Opening {name}…")).weak().size(16.0));
                    });
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Drop a file here or choose File → Open…").weak().size(18.0));
                });
            }
            if let Some(e) = &self.error {
                ui.painter().text(full.center_bottom() - egui::vec2(0.0, 24.0), egui::Align2::CENTER_BOTTOM, format!("{} — {}", e.message, e.hint.clone().unwrap_or_default()), egui::FontId::proportional(14.0), theme.danger);
            }
            if let Some(p) = &self.player {
                if let Some(e) = p.error() {
                    ui.painter().text(full.center_bottom() - egui::vec2(0.0, 24.0), egui::Align2::CENTER_BOTTOM, e.message, egui::FontId::proportional(14.0), theme.danger);
                }
            }
            expand
        });
        if expand.inner {
            self.set_plot_fullscreen(true);
        }

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
                    let audio_tools = p.has_audio() && (self.plot_fullscreen || self.show_audio_tools || !p.has_video());
                    soonest(if audio_tools { self.audio_tools.repaint_interval() } else { Duration::from_millis(50) });
                    if let Some(due) = p.next_frame_due() {
                        soonest(Duration::from_secs_f64(due).max(Duration::from_millis(1)));
                    }
                    let rate = p.playback_rate();
                    if let Some(t) = next_subtitle.filter(|_| rate > 0.0) {
                        let due = ((t - p.position()) / rate).clamp(0.001, 0.05);
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

#[allow(dead_code)]
fn _uses(w: u32, h: u32, rgb: &[u8]) -> ironav_core::Result<Vec<u8>> {
    to_png(w, h, rgb)
}

#[cfg(test)]
mod tests;
