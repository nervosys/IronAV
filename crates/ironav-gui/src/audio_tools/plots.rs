use super::color_scale::ColorScale;
use crate::player_style::Theme;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke};
use ironav_player::{
    AudioEffects, AudioPcm, Player, SpectralDecay, SpectrumFrame, Waveform, DECAY_FFT_SIZE,
    DECAY_SLICES, EQ_FREQUENCIES, SPECTRUM_BANDS, SPECTRUM_FFT_SIZE, SPECTRUM_MAX_HZ,
    SPECTRUM_MIN_HZ, WAVEFORM_BINS,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum PlotKind {
    #[default]
    Waterfall,
    Decay,
    Waveform,
    Spectrum,
    Equalizer,
}

impl PlotKind {
    pub(super) const ALL: [Self; 5] = [
        Self::Waterfall, Self::Decay, Self::Waveform, Self::Spectrum, Self::Equalizer,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Waterfall => "Waterfall",
            Self::Decay => "Spectral decay",
            Self::Waveform => "Waveform",
            Self::Spectrum => "Spectrum",
            Self::Equalizer => "EQ response",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            Self::Waterfall => "Time history of audio frequencies and levels",
            Self::Decay => "Cumulative spectral decay of the recent playback signal",
            Self::Waveform => "Minimum and maximum samples over a short time window",
            Self::Spectrum => "Current spectrum analyzer levels in dBFS",
            Self::Equalizer => "Frequency response of the active five-band equalizer",
        }
    }

    pub(super) fn uses_level_colors(self) -> bool {
        matches!(self, Self::Waterfall | Self::Decay | Self::Spectrum)
    }

    pub(super) fn left_margin(self) -> f32 {
        match self {
            Self::Waterfall => 0.0,
            Self::Waveform | Self::Decay => 14.0,
            Self::Spectrum | Self::Equalizer => 40.0,
        }
    }
}

pub(super) const AXIS_HEIGHT: f32 = 17.0;

const FREQUENCY_TICKS: [(f32, &str, Align2); 5] = [
    (30.0, "30 Hz", Align2::LEFT_TOP),
    (100.0, "100", Align2::CENTER_TOP),
    (1000.0, "1k", Align2::CENTER_TOP),
    (5000.0, "5k", Align2::CENTER_TOP),
    (16_000.0, "16k Hz", Align2::RIGHT_TOP),
];

fn frequency_x(rect: Rect, hz: f32) -> f32 {
    let position = (hz / SPECTRUM_MIN_HZ).ln() / (SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ).ln();
    rect.left() + rect.width() * position
}

pub(super) fn frequency_axis(painter: &egui::Painter, rect: Rect, theme: &Theme) {
    for (hz, label, align) in FREQUENCY_TICKS {
        painter.text(
            egui::pos2(frequency_x(rect, hz), rect.bottom() + 3.0), align, label,
            FontId::proportional(10.0), theme.muted_text,
        );
    }
}

pub(super) fn chart(ui: &mut egui::Ui, height: f32, left: f32, theme: &Theme) -> (Rect, egui::Response) {
    let (area, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height + AXIS_HEIGHT), egui::Sense::hover());
    let rect = Rect::from_min_max(egui::pos2(area.left() + left, area.top()), egui::pos2(area.right(), area.top() + height));
    ui.painter().rect_filled(rect, 2.0, theme.background);
    ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, theme.border), egui::StrokeKind::Inside);
    (rect, response)
}

fn level_grid(painter: &egui::Painter, rect: Rect, ticks: &[(f32, &str)], theme: &Theme) {
    for &(fraction, label) in ticks {
        let y = rect.bottom() - fraction * rect.height();
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], Stroke::new(1.0, theme.border));
        painter.text(egui::pos2(rect.left() - 4.0, y), Align2::RIGHT_CENTER, label, FontId::proportional(10.0), theme.muted_text);
    }
}

fn waiting(painter: &egui::Painter, rect: Rect, theme: &Theme) {
    painter.text(rect.center(), Align2::CENTER_CENTER, "Waiting for audio", FontId::proportional(12.0), theme.muted_text);
}

const LEVEL_TICKS: &[(f32, &str)] = &[(0.0, "−90"), (1.0 / 3.0, "−60"), (2.0 / 3.0, "−30"), (1.0, "0 dBFS")];
const EQ_TICKS: &[(f32, &str)] = &[(0.0, "−24"), (0.25, "−12"), (0.5, "0"), (0.75, "+12"), (1.0, "+24 dB")];

pub(super) fn decay_depth(rect: Rect) -> f32 {
    (rect.width() * 0.13).min(32.0)
}

pub(super) fn trace_indices(dark: bool) -> [usize; 2] {
    if dark { [168, 217] } else { [72, 147] }
}

fn wave_axes(painter: &egui::Painter, rect: Rect, stereo: bool, seconds: f32, theme: &Theme) {
    let count = if stereo { 2 } else { 1 };
    let stroke = Stroke::new(1.0, theme.border);
    for channel in 0..count {
        let y = rect.top() + (channel as f32 + 0.5) * rect.height() / count as f32;
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
        let name = if !stereo { "M" } else if channel == 0 { "L" } else { "R" };
        painter.text(egui::pos2(rect.left() - 3.0, y), Align2::RIGHT_CENTER, name, FontId::proportional(10.0), theme.muted_text);
    }
    let font = FontId::proportional(10.0);
    painter.text(rect.left_bottom() + egui::vec2(0.0, 3.0), Align2::LEFT_TOP, format!("−{:.0} ms", seconds * 1000.0), font.clone(), theme.muted_text);
    painter.text(rect.right_bottom() + egui::vec2(0.0, 3.0), Align2::RIGHT_TOP, "0 ms", font, theme.muted_text);
}

fn decay_time(painter: &egui::Painter, rect: Rect, step_seconds: f32, theme: &Theme) {
    let elapsed = step_seconds * (DECAY_SLICES - 1) as f32 * 1000.0;
    painter.text(rect.right_top() + egui::vec2(-4.0, 3.0), Align2::RIGHT_TOP,
        format!("0 → {elapsed:.0} ms"), FontId::proportional(10.0), theme.muted_text);
}

pub(super) fn gpu_axes(painter: &egui::Painter, rect: Rect, kind: PlotKind, pcm: &AudioPcm, theme: &Theme) {
    match kind {
        PlotKind::Waterfall => frequency_axis(painter, rect, theme),
        PlotKind::Spectrum => {
            level_grid(painter, rect, LEVEL_TICKS, theme);
            frequency_axis(painter, rect, theme);
        }
        PlotKind::Equalizer => {
            level_grid(painter, rect, EQ_TICKS, theme);
            frequency_axis(painter, rect, theme);
        }
        PlotKind::Waveform if pcm.samples.len() >= SPECTRUM_FFT_SIZE => {
            wave_axes(painter, rect, pcm.stereo, SPECTRUM_FFT_SIZE as f32 / pcm.rate as f32, theme);
        }
        PlotKind::Waveform => waiting(painter, rect, theme),
        PlotKind::Decay => {
            let front = Rect::from_min_max(rect.min + egui::vec2(decay_depth(rect), 0.0), rect.max);
            frequency_axis(painter, front, theme);
            if pcm.samples.len() >= SPECTRUM_FFT_SIZE {
                let step = pcm.samples.len().min(DECAY_FFT_SIZE) / (DECAY_SLICES + 2);
                decay_time(painter, rect, step as f32 / pcm.rate as f32, theme);
            } else {
                waiting(painter, rect, theme);
            }
        }
    }
}

pub(super) fn spectrum(ui: &mut egui::Ui, height: f32, theme: &Theme, scale: ColorScale, frame: Option<&SpectrumFrame>) {
    let (rect, response) = chart(ui, height, 40.0, theme);
    level_grid(ui.painter(), rect, LEVEL_TICKS, theme);
    frequency_axis(ui.painter(), rect, theme);
    if let Some(frame) = frame {
        let colors = scale.colors();
        let width = rect.width() / SPECTRUM_BANDS as f32;
        let gap = (width * 0.22).min(1.0);
        for (band, &level) in frame.bands.iter().enumerate() {
            let height = rect.height() * f32::from(level) / 255.0;
            let x = rect.left() + band as f32 * width;
            let bar = Rect::from_min_size(egui::pos2(x, rect.bottom() - height), egui::vec2(width - gap, height));
            ui.painter().rect_filled(bar, 0.0, colors[usize::from(level)]);
        }
    } else {
        waiting(ui.painter(), rect, theme);
    }
    response.on_hover_text("Log-frequency spectrum of the audio at the playback clock");
}

fn trace_colors(ui: &egui::Ui, scale: ColorScale) -> [Color32; 2] {
    let colors = scale.colors();
    trace_indices(ui.visuals().dark_mode).map(|index| colors[index])
}

fn add_strip(mesh: &mut egui::Mesh, previous: Option<u32>, top: Pos2, bottom: Pos2, colors: [Color32; 2]) -> u32 {
    let vertex = mesh.vertices.len() as u32;
    mesh.colored_vertex(top, colors[0]);
    mesh.colored_vertex(bottom, colors[1]);
    if let Some(previous) = previous {
        mesh.add_triangle(previous, previous + 1, vertex);
        mesh.add_triangle(vertex, previous + 1, vertex + 1);
    }
    vertex
}

pub(super) fn waveform(ui: &mut egui::Ui, height: f32, theme: &Theme, scale: ColorScale, wave: Option<&Waveform>) {
    let (rect, response) = chart(ui, height, 14.0, theme);
    let painter = ui.painter();
    if let Some(wave) = wave {
        let count = if wave.stereo { 2 } else { 1 };
        let colors = trace_colors(ui, scale);
        wave_axes(painter, rect, wave.stereo, wave.seconds, theme);
        for (channel, (ranges, color)) in wave.ranges.iter().zip(colors).take(count).enumerate() {
            let lane_height = rect.height() / count as f32;
            let center = rect.top() + (channel as f32 + 0.5) * lane_height;
            let mut mesh = egui::Mesh::default();
            let mut previous = None;
            for (index, &(low, high)) in ranges.iter().enumerate() {
                let x = rect.left() + rect.width() * index as f32 / (WAVEFORM_BINS - 1) as f32;
                let y = |v: f32| center - v.clamp(-1.0, 1.0) * lane_height * 0.46;
                let top = egui::pos2(x, y(high) - 0.5);
                let bottom = egui::pos2(x, y(low) + 0.5);
                previous = Some(add_strip(&mut mesh, previous, top, bottom, [color; 2]));
            }
            painter.add(egui::Shape::mesh(mesh));
        }
    } else {
        waiting(painter, rect, theme);
    }
    response.on_hover_text("Waveform amplitude from −1 to +1. The newest samples are at the right.");
}

pub(super) fn decay(ui: &mut egui::Ui, height: f32, theme: &Theme, scale: ColorScale, data: Option<&SpectralDecay>) {
    let (rect, response) = chart(ui, height, 14.0, theme);
    let painter = ui.painter();
    let depth = decay_depth(rect);
    let front = Rect::from_min_max(rect.min + egui::vec2(depth, 0.0), rect.max);
    frequency_axis(painter, front, theme);
    if let Some(data) = data {
        let colors = scale.colors();
        let trace_height = rect.height() * 0.55;
        for (slice_index, bands) in data.slices.iter().enumerate() {
            let fraction = slice_index as f32 / (DECAY_SLICES - 1) as f32;
            let base = rect.top() + trace_height + (rect.height() - trace_height) * fraction;
            let axis = rect.translate(egui::vec2(depth * fraction, 0.0));
            let mut mesh = egui::Mesh::default();
            let mut previous = None;
            for (band, &level) in bands.iter().enumerate() {
                let x = axis.left() + front.width() * band as f32 / (SPECTRUM_BANDS - 1) as f32;
                let y = base - f32::from(level) / 255.0 * trace_height;
                previous = Some(add_strip(
                    &mut mesh, previous, egui::pos2(x, y - 0.5), egui::pos2(x, base),
                    [colors[usize::from(level)], theme.background],
                ));
            }
            painter.add(egui::Shape::mesh(mesh));
        }
        decay_time(painter, rect, data.step_seconds, theme);
    } else {
        waiting(painter, rect, theme);
    }
    response.on_hover_text("Successive spectra omit more of the start of one playback window. The earliest cut is at the back.");
}

pub(super) const RESPONSE_POINTS: usize = 128;

pub(super) struct EqCurve {
    rate: u32,
    gains: [f32; EQ_FREQUENCIES.len()],
    db: [f32; RESPONSE_POINTS],
}

pub(super) fn equalizer(ui: &mut egui::Ui, height: f32, theme: &Theme, scale: ColorScale, player: &Player, effects: AudioEffects, cache: &mut Option<EqCurve>) {
    let (rect, response) = chart(ui, height, 40.0, theme);
    let rate = player.audio_output_rate().unwrap_or_default();
    if cache.as_ref().is_none_or(|curve| curve.rate != rate || curve.gains != effects.eq_db) {
        let frequencies: [f32; RESPONSE_POINTS] = std::array::from_fn(|index| {
            let fraction = index as f32 / (RESPONSE_POINTS - 1) as f32;
            SPECTRUM_MIN_HZ * (SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ).powf(fraction)
        });
        let mut db = [0.0; RESPONSE_POINTS];
        player.equalizer_response(effects, &frequencies, &mut db);
        *cache = Some(EqCurve { rate, gains: effects.eq_db, db });
    }
    level_grid(ui.painter(), rect, EQ_TICKS, theme);
    frequency_axis(ui.painter(), rect, theme);
    if let Some(curve) = cache {
        let points: Vec<_> = curve.db.iter().take_while(|db| db.is_finite()).enumerate().map(|(index, &db)| {
            let x = rect.left() + rect.width() * index as f32 / (RESPONSE_POINTS - 1) as f32;
            let y = rect.bottom() - (db.clamp(-24.0, 24.0) + 24.0) / 48.0 * rect.height();
            egui::pos2(x, y)
        }).collect();
        if points.len() >= 2 {
            ui.painter().add(egui::Shape::line(points, Stroke::new(2.0, trace_colors(ui, scale)[0])));
        }
    }
    response.on_hover_text("Combined gain of the five EQ bands at each frequency");
}
