use crate::player_style::{self, Icon, Theme};
use eframe::egui::{self, Color32, RichText};
use ironav_player::{
    AudioAnalysis, AudioEffects, Player, DECAY_REFRESH_HZ, EQ_FREQUENCIES, SPECTRUM_BANDS, SPECTRUM_HISTORY,
};
use std::path::PathBuf;

mod color_scale;
mod gpu;
mod plots;
use color_scale::ColorScale;
pub(crate) use gpu::install as install_gpu;
use plots::{EqCurve, PlotKind};

const TEXTURE_UV: egui::Rect = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));

#[derive(Default)]
pub(crate) struct AudioTools {
    settings: AudioEffects,
    plot: PlotKind,
    scale: ColorScale,
    analysis: AudioAnalysis,
    gpu: Option<gpu::PlotState>,
    eq_curve: Option<EqCurve>,
    preference_file: Option<PathBuf>,
    preferences_changed: bool,
    image: Option<egui::TextureHandle>,
    legend: Option<(ColorScale, egui::TextureHandle)>,
    rendered: Option<(Option<u64>, ColorScale, Color32)>,
}

impl AudioTools {
    pub(crate) fn new(preference_file: Option<PathBuf>) -> Self {
        let mut tools = Self::default();
        if let Some(text) = preference_file.as_ref().and_then(|path| std::fs::read_to_string(path).ok()) {
            let mut settings = text.lines().map(str::trim);
            if let Some(plot) = settings.next().and_then(|name| PlotKind::ALL.into_iter().find(|plot| plot.name() == name)) {
                tools.plot = plot;
            }
            if let Some(scale) = settings.next().and_then(|name| ColorScale::ALL.into_iter().find(|scale| scale.name() == name)) {
                tools.scale = scale;
            }
        }
        tools.preference_file = preference_file;
        tools
    }

    pub(crate) fn attach(&mut self, player: &Player, visible: bool) {
        self.analysis = AudioAnalysis::default();
        self.rendered = None;
        if let Some(gpu) = &mut self.gpu {
            gpu.reset();
        }
        self.set_visible(player, visible);
        if self.settings != AudioEffects::default() && player.has_audio() {
            player.set_audio_effects(self.settings);
            // Opening prebuffers in the background. Requeue that audio with the chosen settings.
            player.seek(player.position());
        }
    }

    pub(crate) fn enable_gpu(&mut self, enabled: bool) {
        self.gpu = enabled.then(gpu::PlotState::default);
    }

    pub(crate) fn set_visible(&self, player: &Player, visible: bool) {
        if self.gpu.is_some() {
            player.set_audio_visualization_pcm(visible);
        } else {
            player.set_audio_visualization(visible);
        }
    }

    pub(crate) fn repaint_interval(&self) -> std::time::Duration {
        if self.gpu.is_some() && self.plot == PlotKind::Decay {
            std::time::Duration::from_millis(1_000 / u64::from(DECAY_REFRESH_HZ))
        } else {
            std::time::Duration::from_millis(33)
        }
    }

    pub(crate) fn show(&mut self, ui: &mut egui::Ui, player: &Player, theme: &Theme, docked: bool) -> bool {
        let expand = self.plot_toolbar(ui, theme, false);
        let height = if docked { 106.0 } else { (ui.available_height() * 0.52).clamp(105.0, 250.0) };
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        egui::ScrollArea::vertical().id_salt("audio tools").min_scrolled_height(0.0).auto_shrink([false; 2]).show(ui, |ui| {
            if !docked {
                let file = std::path::Path::new(&player.info().path);
                if let Some(name) = file.file_name() {
                    ui.add(egui::Label::new(RichText::new(name.to_string_lossy()).heading()).truncate());
                }
            }
            if let Some(audio) = &player.info().audio {
                ui.label(RichText::new(format!("{} Hz · {} ch", audio.sample_rate, audio.channels)).weak());
            }
            let level = player.audio_level().clamp(0.0, 1.0);
            let (meter, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
            ui.painter().rect_filled(meter, 3.0, theme.border);
            ui.painter().rect_filled(egui::Rect::from_min_size(meter.min, egui::vec2(meter.width() * level.sqrt(), meter.height())), 3.0, theme.success);
            response.on_hover_text(if level > 0.0 { format!("Peak {:.1} dBFS", 20.0 * level.log10()) } else { "Peak −∞ dBFS".into() });

            if self.plot.uses_level_colors() {
                self.color_legend(ui, theme);
            }
            self.draw_plot(ui, player, theme, height);
            ui.separator();

            let before = self.settings;
            ui.strong("Equalizer (dB)");
            let columns = EQ_FREQUENCIES.len() as f32;
            let width = ((ui.available_width() - (columns - 1.0) * ui.spacing().item_spacing.x) / columns).min(54.0);
            ui.horizontal(|ui| {
                for (&frequency, gain) in EQ_FREQUENCIES.iter().zip(&mut self.settings.eq_db) {
                    ui.allocate_ui_with_layout(egui::vec2(width, 110.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                        let label = if frequency < 1000 { frequency.to_string() } else { format!("{}k", frequency / 1000) };
                        ui.label(RichText::new(label).small().weak());
                        ui.spacing_mut().slider_width = 62.0;
                        let slider = egui::Slider::new(gain, -12.0..=12.0).vertical().step_by(0.5).show_value(false);
                        let response = ui.add(slider);
                        response.widget_info(|| egui::WidgetInfo::slider(response.enabled(), f64::from(*gain), format!("{frequency} Hz EQ")));
                        response.on_hover_text(format!("{frequency} Hz"));
                        ui.label(RichText::new(format!("{gain:+.1}")).small().monospace());
                    });
                }
            });
            ui.separator();
            ui.strong("Effects");
            ui.spacing_mut().slider_width = (ui.available_width() - 52.0).clamp(24.0, 160.0);
            effect_slider(ui, "Compression", &mut self.settings.compression)
                .on_hover_text("Reduces the difference between loud and quiet passages");
            effect_slider(ui, "Echo", &mut self.settings.echo)
                .on_hover_text("Mixes in a decaying 180 ms echo");
            let reset = ui.scope(|ui| {
                player_style::raised_controls(ui, theme);
                ui.add_enabled(self.settings != AudioEffects::default(), egui::Button::new("Reset audio")).clicked()
            }).inner;
            if reset {
                self.settings = AudioEffects::default();
            }
            if self.settings != before {
                player.set_audio_effects(self.settings);
                ui.ctx().request_repaint();
            }
        });
        expand
    }

    pub(crate) fn show_fullscreen(&mut self, ui: &mut egui::Ui, player: &Player, theme: &Theme) -> bool {
        let exit = self.plot_toolbar(ui, theme, true);
        if self.plot.uses_level_colors() {
            self.color_legend(ui, theme);
        }
        let height = (ui.available_height() - plots::AXIS_HEIGHT).max(1.0);
        self.draw_plot(ui, player, theme, height);
        exit
    }

    fn plot_toolbar(&mut self, ui: &mut egui::Ui, theme: &Theme, fullscreen: bool) -> bool {
        let preferences = (self.plot, self.scale);
        let toggle = ui.horizontal_wrapped(|ui| {
            player_style::raised_controls(ui, theme);
            ui.spacing_mut().item_spacing.x = 4.0;
            egui::ComboBox::from_id_salt("audio plot type")
                .selected_text(self.plot.name())
                .width(118.0)
                .show_ui(ui, |ui| {
                    for plot in PlotKind::ALL {
                        ui.selectable_value(&mut self.plot, plot, plot.name())
                            .on_hover_text(plot.description());
                    }
                })
                .response
                .on_hover_text("Audio plot type");
            egui::ComboBox::from_id_salt("audio plot color scale")
                .selected_text(self.scale.name())
                .width(88.0)
                .show_ui(ui, |ui| {
                    for scale in ColorScale::ALL {
                        ui.selectable_value(&mut self.scale, scale, scale.name());
                    }
                })
                .response
                .on_hover_text("Plot color scale");
            let label = if fullscreen { "Exit full screen (Esc)" } else { "Full screen audio plot" };
            player_style::transport(ui, theme, Icon::Fullscreen { exit: fullscreen }, label).clicked()
        }).inner;
        self.preferences_changed |= preferences != (self.plot, self.scale);
        toggle
    }

    fn draw_plot(&mut self, ui: &mut egui::Ui, player: &Player, theme: &Theme, height: f32) {
        if let Some(gpu) = &mut self.gpu {
            gpu.show(ui, player, gpu::PlotOptions {
                kind: self.plot, scale: self.scale, height, theme, effects: self.settings,
            });
        } else {
            self.software_plot(ui, player, theme, height);
        }
    }

    fn software_plot(&mut self, ui: &mut egui::Ui, player: &Player, theme: &Theme, height: f32) {
        match self.plot {
            PlotKind::Waterfall | PlotKind::Spectrum => player.audio_spectrum(&mut self.analysis.spectra),
            PlotKind::Waveform | PlotKind::Decay => player.audio_analysis(&mut self.analysis),
            PlotKind::Equalizer => {}
        }
        match self.plot {
            PlotKind::Waterfall => self.waterfall(ui, theme, height),
            PlotKind::Waveform => plots::waveform(ui, height, theme, self.scale, self.analysis.waveform.as_ref()),
            PlotKind::Decay => plots::decay(ui, height, theme, self.scale, self.analysis.decay.as_ref()),
            PlotKind::Spectrum => plots::spectrum(ui, height, theme, self.scale, self.analysis.spectra.last()),
            PlotKind::Equalizer => plots::equalizer(ui, height, theme, self.scale, player, self.settings, &mut self.eq_curve),
        }
    }

    fn color_legend(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        if self.legend.as_ref().is_none_or(|(scale, _)| *scale != self.scale) {
            let image = egui::ColorImage::new([256, 1], self.scale.colors().to_vec());
            let texture = ui.ctx().load_texture("audio plot color legend", image, egui::TextureOptions::LINEAR);
            self.legend = Some((self.scale, texture));
        }
        let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 16.0), egui::Sense::hover());
        let y = row.center().y;
        let font = egui::FontId::proportional(10.0);
        ui.painter().text(egui::pos2(row.left(), y), egui::Align2::LEFT_CENTER, "−90", font.clone(), theme.muted_text);
        ui.painter().text(egui::pos2(row.right(), y), egui::Align2::RIGHT_CENTER, "0 dBFS", font, theme.muted_text);
        if let Some((_, texture)) = &self.legend {
            let ramp = egui::Rect::from_min_max(egui::pos2(row.left() + 24.0, y - 4.0), egui::pos2(row.right() - 40.0, y + 4.0));
            ui.painter().image(texture.id(), ramp, TEXTURE_UV, Color32::WHITE);
            ui.painter().rect_stroke(ramp, 1.0, egui::Stroke::new(1.0, theme.border), egui::StrokeKind::Inside);
        }
    }

    fn waterfall(&mut self, ui: &mut egui::Ui, theme: &Theme, height: f32) {
        let history = &self.analysis.spectra;
        let signature = (history.last().map(|row| row.sequence), self.scale, theme.background);
        if self.rendered != Some(signature) {
            let shades = self.scale.colors();
            let mut image = egui::ColorImage::filled([SPECTRUM_BANDS, SPECTRUM_HISTORY], theme.background);
            let (pixels, _) = image.pixels.as_chunks_mut::<SPECTRUM_BANDS>();
            for (pixels, row) in pixels.iter_mut().zip(history.iter().rev()) {
                for (pixel, &level) in pixels.iter_mut().zip(&row.bands) {
                    *pixel = shades[usize::from(level)];
                }
            }
            if let Some(texture) = &mut self.image {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.image = Some(ui.ctx().load_texture("audio waterfall", image, egui::TextureOptions::LINEAR));
            }
            self.rendered = Some(signature);
        }
        let (allocated, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height + plots::AXIS_HEIGHT), egui::Sense::hover());
        let plot = egui::Rect::from_min_size(allocated.min, egui::vec2(allocated.width(), height));
        if let Some(texture) = &self.image {
            ui.painter().image(texture.id(), plot, TEXTURE_UV, Color32::WHITE);
        }
        ui.painter().rect_stroke(plot, 2.0, egui::Stroke::new(1.0, theme.border), egui::StrokeKind::Inside);
        plots::frequency_axis(ui.painter(), plot, theme);
        response.on_hover_text("Log frequency across · newest audio at the top");
    }
}

impl Drop for AudioTools {
    fn drop(&mut self) {
        if self.preferences_changed {
            if let Some(file) = &self.preference_file {
                let value = format!("{}\n{}", self.plot.name(), self.scale.name());
                let _ = player_style::save_preference(file, &value);
            }
        }
    }
}

fn effect_slider(ui: &mut egui::Ui, title: &str, amount: &mut f32) -> egui::Response {
    ui.label(title);
    let mut percent = *amount * 100.0;
    let response = ui.add(egui::Slider::new(&mut percent, 0.0..=100.0).integer().suffix("%"));
    response.widget_info(|| egui::WidgetInfo::slider(response.enabled(), f64::from(percent), title));
    if response.changed() {
        *amount = percent / 100.0;
    }
    response
}
