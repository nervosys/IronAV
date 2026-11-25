use eframe::egui::{self, Color32};
use ironav_core::{Error, ErrorCode, Result};
use ironav_player::{Player, SubtitleTrack};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq)]
struct Source {
    path: PathBuf,
    index: Option<usize>,
}

struct Loading {
    result: Receiver<Result<SubtitleTrack>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Loading {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(crate) struct Subtitles {
    source: Option<Source>,
    // Keep only one reader in flight, including after a track switch or media open.
    loading: Option<Loading>,
    result: Option<Result<SubtitleTrack>>,
}

impl Subtitles {
    fn select(&mut self, source: Option<Source>) {
        if let Some(loading) = &self.loading {
            loading.cancelled.store(true, Ordering::Relaxed);
        }
        self.source = source;
        self.result = None;
    }

    pub(crate) fn clear(&mut self) {
        self.select(None);
    }

    pub(crate) fn update(&mut self, ctx: &egui::Context) {
        if let Some(loading) = &self.loading {
            let result = match loading.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err(Error::new(ErrorCode::Internal, "subtitle loading stopped")),
            };
            if !loading.cancelled.load(Ordering::Relaxed) {
                self.result = Some(result);
            }
            self.loading = None;
        }
        if self.result.is_some() {
            return;
        }
        let Some(source) = self.source.clone() else { return };
        let (send, result) = channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (worker_cancelled, ctx) = (cancelled.clone(), ctx.clone());
        let spawn = std::thread::Builder::new().name("ironav-subtitles".into()).spawn(move || {
            let track = SubtitleTrack::load(&source.path, source.index, &worker_cancelled);
            let _ = send.send(track);
            ctx.request_repaint();
        });
        match spawn {
            Ok(_) => self.loading = Some(Loading { result, cancelled }),
            Err(e) => self.result = Some(Err(Error::new(ErrorCode::Internal, format!("cannot start subtitle loading: {e}")))),
        }
    }

    pub(crate) fn menu(&mut self, ui: &mut egui::Ui, player: &Player) {
        let path = Path::new(&player.info().path);
        ui.menu_button("Subtitles", |ui| {
            if ui.selectable_label(self.source.is_none(), "Off").clicked() {
                self.clear();
                ui.close();
            }
            for (i, track) in player.subtitle_tracks().iter().enumerate() {
                let selected = self.source.as_ref().is_some_and(|s| s.index == Some(track.index));
                if ui.selectable_label(selected, format!("Track {} ({})", i + 1, track.codec.name())).clicked() {
                    self.select(Some(Source { path: path.to_owned(), index: Some(track.index) }));
                    ui.ctx().request_repaint();
                    ui.close();
                }
            }
            if let Some(source) = self.source.as_ref().filter(|s| s.index.is_none()) {
                ui.label(source.path.file_name().unwrap_or_default().to_string_lossy());
            }
            if ui.button("Open subtitle file…").clicked() {
                ui.close();
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Open subtitle file")
                    .add_filter("Subtitles", &["srt", "vtt", "ass", "ssa"]);
                if let Some(dir) = path.parent().filter(|d| d.is_dir()) {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(path) = dialog.pick_file() {
                    self.select(Some(Source { path, index: None }));
                    ui.ctx().request_repaint();
                }
            }
        });
        match &self.result {
            None if self.source.is_some() => { ui.weak("Loading subtitles…"); }
            Some(Err(e)) => { ui.label("Subtitles unavailable").on_hover_text(&e.message); }
            _ => {}
        }
    }

    /// Paint over the picture and return the next subtitle change on the playback clock.
    pub(crate) fn paint(&self, ui: &egui::Ui, rect: egui::Rect, position: f64) -> Option<f64> {
        let track = self.result.as_ref()?.as_ref().ok()?;
        let (text, next) = track.at(position);
        if !text.is_empty() && rect.width() >= 32.0 && rect.height() >= 32.0 {
            let font_size = (rect.height() * 0.048).clamp(16.0, 38.0);
            let mut layout = egui::text::LayoutJob::simple(text, egui::FontId::proportional(font_size), Color32::WHITE, rect.width() * 0.88);
            layout.halign = egui::Align::Center;
            layout.wrap.max_rows = (rect.height() * 0.38 / (font_size * 1.3)).clamp(1.0, 6.0) as usize;
            let painter = ui.painter().with_clip_rect(rect);
            let galley = painter.layout_job(layout);
            let bottom = rect.center_bottom() - egui::vec2(0.0, (rect.height() * 0.06).max(7.0));
            let offset = bottom - galley.rect.center_bottom();
            painter.rect_filled(galley.rect.translate(offset).expand2(egui::vec2(8.0, 3.0)), 4.0, Color32::from_black_alpha(200));
            painter.galley(offset.to_pos2(), galley, Color32::WHITE);
        }
        next
    }
}
