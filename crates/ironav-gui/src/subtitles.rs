use crate::player_style::MENU_WIDTH;
use eframe::egui::{self, Color32};
use ironav_codec::bitmapsub::SubRect;
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
    /// The textures of the bitmap subtitle on screen, with its number on the track: uploaded once.
    cache: Option<(usize, Vec<egui::TextureHandle>)>,
}

impl Subtitles {
    fn select(&mut self, source: Option<Source>) {
        if let Some(loading) = &self.loading {
            loading.cancelled.store(true, Ordering::Relaxed);
        }
        self.source = source;
        self.result = None;
        self.cache = None;
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

    pub(crate) fn open_file(&mut self, path: PathBuf) {
        self.select(Some(Source { path, index: None }));
    }

    pub(crate) fn menu(&mut self, ui: &mut egui::Ui, player: &Player, can_pick: bool) -> Option<rfd::AsyncFileDialog> {
        let path = Path::new(&player.info().path);
        let mut picker = None;
        ui.menu_button("Subtitles", |ui| {
            ui.set_min_width(MENU_WIDTH);
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
                ui.add(egui::Label::new(source.path.file_name().unwrap_or_default().to_string_lossy()).truncate());
            }
            ui.separator();
            if ui.add_enabled(can_pick, egui::Button::new("Open subtitle file…")).clicked() {
                ui.close();
                let mut dialog = rfd::AsyncFileDialog::new()
                    .set_title("Open subtitle file")
                    .add_filter("Subtitles", &["srt", "vtt", "ass", "ssa", "sup"]);
                if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    dialog = dialog.set_directory(dir);
                }
                picker = Some(dialog);
            }
        });
        picker
    }

    pub(crate) fn notice(&self) -> Option<(&str, &str)> {
        match &self.result {
            None if self.source.is_some() => Some(("Loading subtitles…", "Loading subtitles…")),
            Some(Err(e)) => Some(("Subtitles unavailable", &e.message)),
            _ => None,
        }
    }

    /// Paint over the picture and return the next subtitle change on the playback clock. A text
    /// track draws its cues; a bitmap track (Blu-ray PGS, DVD VobSub) draws the event's pictures,
    /// scaled from the subtitle canvas to the picture, with the textures made once per event.
    pub(crate) fn paint(&mut self, ui: &egui::Ui, rect: egui::Rect, position: f64) -> Option<f64> {
        let track = self.result.as_ref()?.as_ref().ok()?;
        if track.is_bitmap() {
            let (shown, next) = track.bitmap_at(position);
            if let Some((id, bitmap)) = shown.filter(|_| rect.width() >= 32.0 && rect.height() >= 32.0) {
                if self.cache.as_ref().is_none_or(|(n, _)| *n != id) {
                    let upload = |r: &SubRect| {
                        let image = egui::ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &r.rgba());
                        ui.ctx().load_texture("subtitle", image, egui::TextureOptions::LINEAR)
                    };
                    self.cache = Some((id, bitmap.rects.iter().map(upload).collect()));
                }
                let painter = ui.painter().with_clip_rect(rect);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                for (r, texture) in bitmap.rects.iter().zip(self.cache.iter().flat_map(|(_, t)| t)) {
                    painter.image(texture.id(), canvas_rect((bitmap.canvas_w, bitmap.canvas_h), r, rect), uv, Color32::WHITE);
                }
            }
            return next;
        }
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

/// Where a subtitle bitmap goes on the displayed `picture`, given the canvas it was made for.
fn canvas_rect(canvas: (u32, u32), r: &SubRect, picture: egui::Rect) -> egui::Rect {
    let (sx, sy) = (picture.width() / canvas.0.max(1) as f32, picture.height() / canvas.1.max(1) as f32);
    egui::Rect::from_min_size(picture.min + egui::vec2(r.x as f32 * sx, r.y as f32 * sy), egui::vec2(r.w as f32 * sx, r.h as f32 * sy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_codec::bitmapsub::testing::*;

    fn picture_shapes(output: &egui::FullOutput) -> Vec<egui::Rect> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Mesh(m) => out.push(m.calc_bounds()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        output.shapes.iter().for_each(|s| walk(&s.shape, &mut out));
        out
    }

    /// A subtitles overlay with the bitmap track of a `.sup` file loaded: a picture at 1.0 s to 2.0 s
    /// and another at 3.0 s to 4.0 s.
    fn loaded() -> (Subtitles, egui::Context) {
        let dir = std::env::temp_dir().join(format!("ironav-gui-bitmap-subs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let object = |x, y, w: u16, h: u16| PgsObject { id: 1, x, y, w, h, pixels: vec![1; w as usize * h as usize], crop: None, forced: false };
        let canvas = (1920, 1080);
        let palette = [[1, 235, 128, 128, 255]];
        let sets = vec![
            (0, pgs_clear_set(canvas)),
            (90_000, pgs_display_set(canvas, 2, &palette, &[object(480, 810, 960, 108)], 0)),
            (180_000, pgs_clear_set(canvas)),
            (270_000, pgs_display_set(canvas, 2, &palette, &[object(0, 0, 192, 108)], 0)),
            (360_000, pgs_clear_set(canvas)),
        ];
        let file = dir.join("in.sup");
        std::fs::write(&file, sup_file(&sets)).unwrap();
        let ctx = egui::Context::default();
        let mut subtitles = Subtitles::default();
        subtitles.open_file(file);
        let started = std::time::Instant::now();
        while subtitles.result.is_none() {
            subtitles.update(&ctx);
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the track did not load");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(subtitles.result.as_ref().unwrap().is_ok());
        (subtitles, ctx)
    }

    fn paint_at(subtitles: &mut Subtitles, ctx: &egui::Context, picture: egui::Rect, at: f64) -> (Vec<egui::Rect>, Option<f64>) {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        let mut next = None;
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| next = subtitles.paint(ui, picture, at));
        output.textures_delta.clear();
        (picture_shapes(&output), next)
    }

    #[test]
    fn a_bitmap_track_is_drawn_scaled_to_the_picture_and_follows_the_clock() {
        let (mut subtitles, ctx) = loaded();
        // the picture is half the canvas size and offset in the window
        let picture = egui::Rect::from_min_size(egui::pos2(20.0, 30.0), egui::vec2(960.0, 540.0));
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 0.5);
        assert!(shapes.is_empty() && next == Some(1.0), "{shapes:?} {next:?}");
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 1.5);
        assert_eq!(shapes, vec![egui::Rect::from_min_size(egui::pos2(260.0, 435.0), egui::vec2(480.0, 54.0))]);
        assert_eq!(next, Some(2.0));
        assert_eq!(subtitles.cache.as_ref().map(|c| c.0), Some(0));
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 2.5);
        assert!(shapes.is_empty() && next == Some(3.0));
        // a seek back and forth lands on the right event, not on what was shown before
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 3.2);
        assert_eq!(shapes, vec![egui::Rect::from_min_size(egui::pos2(20.0, 30.0), egui::vec2(96.0, 54.0))]);
        assert_eq!((next, subtitles.cache.as_ref().map(|c| c.0)), (Some(4.0), Some(1)));
        let (shapes, _) = paint_at(&mut subtitles, &ctx, picture, 1.0);
        assert_eq!(shapes.len(), 1, "an event is on screen from its start");
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 2.0);
        assert!(shapes.is_empty() && next == Some(3.0), "and up to, not at, its end");
        let (shapes, next) = paint_at(&mut subtitles, &ctx, picture, 9.0);
        assert!(shapes.is_empty() && next.is_none());
        // a different picture size moves and scales it
        let small = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(480.0, 270.0));
        let (shapes, _) = paint_at(&mut subtitles, &ctx, small, 1.5);
        assert_eq!(shapes, vec![egui::Rect::from_min_size(egui::pos2(120.0, 202.5), egui::vec2(240.0, 27.0))]);
    }

    #[test]
    fn canvas_positions_scale_to_the_picture() {
        let r = SubRect { x: 360, y: 240, w: 72, h: 48, forced: false, pixels: Vec::new(), palette: std::sync::Arc::from(Vec::new()) };
        let picture = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(1440.0, 960.0));
        assert_eq!(canvas_rect((720, 480), &r, picture), egui::Rect::from_min_size(egui::pos2(730.0, 500.0), egui::vec2(144.0, 96.0)));
    }
}
