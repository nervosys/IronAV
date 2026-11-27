//! A bounded history of post-effects, pre-volume spectra. The audio worker produces rows and the
//! frontend retrieves only rows the playback clock has reached.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

pub const SPECTRUM_BANDS: usize = 64;
pub const SPECTRUM_MIN_HZ: f32 = 30.0;
pub const SPECTRUM_MAX_HZ: f32 = 16_000.0;
pub const SPECTRUM_HISTORY: usize = 176;
pub const WAVEFORM_BINS: usize = 128;
pub const DECAY_SLICES: usize = 7;
pub const DECAY_REFRESH_HZ: u32 = 60;
pub const SPECTRUM_FFT_SIZE: usize = 2048;
pub const DECAY_FFT_SIZE: usize = 8192;
pub const DECAY_STEP: usize = DECAY_FFT_SIZE / (DECAY_SLICES + 2);
/// Maximum number of spectrum rows a renderer can recover from one sample update.
pub const SPECTRUM_BATCH: usize = 8;
const FFT_SIZE: usize = SPECTRUM_FFT_SIZE;
const DECAY_SIZE: usize = DECAY_FFT_SIZE;

fn spectrum_hop(rate: u32) -> usize {
    (rate as usize / 25).max(FFT_SIZE / 4)
}

fn pcm_window_size(hop: usize) -> usize {
    // CSD can end between two history rows. Keep room for the fractional hop.
    (FFT_SIZE + SPECTRUM_BATCH * hop).max(DECAY_SIZE)
}

fn aligned_end(played: u64, hop: usize) -> u64 {
    let fft = FFT_SIZE as u64;
    let hop = hop.max(1) as u64;
    if played < fft { 0 } else { fft + (played - fft) / hop * hop }
}

/// An overlapping PCM window ending at or before the playback clock.
#[derive(Clone, Default)]
pub struct AudioPcm {
    pub revision: u64,
    pub end_frame: u64,
    pub rate: u32,
    pub stereo: bool,
    pub hop: usize,
    pub samples: Vec<[f32; 2]>,
}

impl AudioPcm {
    /// Keep the waterfall's analysis grid independent of the faster CSD preview.
    pub fn spectrum_end(&self) -> u64 {
        aligned_end(self.end_frame, self.hop)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureMode {
    Off,
    Analysis,
    Pcm,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct PcmTiming {
    pub start: f64,
    pub playback_rate: f64,
    pub sample_rate: u32,
    pub channels: usize,
}

#[derive(Default)]
struct PcmHistory {
    beginning: u64,
    timing: PcmTiming,
    samples: VecDeque<[f32; 2]>,
}

fn stereo_sample(frame: &[f32]) -> [f32; 2] {
    let left = frame.iter().step_by(2).sum::<f32>() / frame.len().div_ceil(2) as f32;
    let right = if frame.len() > 1 { frame.iter().skip(1).step_by(2).sum::<f32>() / (frame.len() / 2) as f32 } else { left };
    [left, right]
}

#[derive(Clone, Copy)]
pub struct SpectrumFrame {
    pub sequence: u64,
    pub time: f64,
    /// Log-frequency bands, scaled from −90 to 0 dBFS into 0 to 255.
    pub bands: [u8; SPECTRUM_BANDS],
}

#[derive(Clone)]
pub struct Waveform {
    pub seconds: f32,
    pub stereo: bool,
    /// The minimum and maximum sample values in each time bin, before the master volume.
    pub ranges: [[(f32, f32); WAVEFORM_BINS]; 2],
}

#[derive(Clone)]
pub struct SpectralDecay {
    /// Time between successive cut points in the analyzed playback window.
    pub step_seconds: f32,
    /// Spectra of successively shorter tails of the same audio window.
    pub slices: [[u8; SPECTRUM_BANDS]; DECAY_SLICES],
}

#[derive(Default)]
pub struct AudioAnalysis {
    pub spectra: Vec<SpectrumFrame>,
    pub waveform: Option<Waveform>,
    pub decay: Option<SpectralDecay>,
}

pub(crate) struct Analysis {
    bands: [u8; SPECTRUM_BANDS],
    waveform: Waveform,
    decay: Option<SpectralDecay>,
}

struct Row {
    frame: SpectrumFrame,
    waveform: Waveform,
    decay: Option<SpectralDecay>,
}

struct History {
    gen: u64,
    sequence: u64,
    rows: VecDeque<Row>,
    revision: u64,
    pcm: PcmHistory,
}

impl History {
    fn clear(&mut self) {
        self.rows.clear();
        self.pcm.samples.clear();
        self.pcm.beginning = 0;
        self.pcm.timing = PcmTiming::default();
        self.revision += 1;
    }
}

pub(crate) struct Spectrum {
    mode: AtomicU8,
    history: Mutex<History>,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self {
            mode: AtomicU8::new(CaptureMode::Off as u8),
            history: Mutex::new(History {
                gen: 1, sequence: 0, rows: VecDeque::new(), revision: 0, pcm: PcmHistory::default(),
            }),
        }
    }
}

impl Spectrum {
    pub fn mode(&self) -> CaptureMode {
        match self.mode.load(Ordering::Relaxed) {
            1 => CaptureMode::Analysis,
            2 => CaptureMode::Pcm,
            _ => CaptureMode::Off,
        }
    }

    pub fn set_mode(&self, mode: CaptureMode) {
        let mut history = self.history.lock().unwrap();
        if self.mode.swap(mode as u8, Ordering::Relaxed) != mode as u8 {
            history.clear();
        }
    }

    pub fn reset(&self, gen: u64) {
        let mut history = self.history.lock().unwrap();
        history.gen = gen;
        history.clear();
    }

    pub fn push(&self, gen: u64, time: f64, analysis: Analysis) {
        // Visualization is expendable when a reader is collecting rows.
        if let Ok(mut history) = self.history.try_lock() {
            if history.gen != gen || self.mode() != CaptureMode::Analysis {
                return;
            }
            if history.rows.len() >= SPECTRUM_HISTORY {
                history.rows.pop_front();
            }
            history.sequence += 1;
            let sequence = history.sequence;
            history.rows.push_back(Row {
                frame: SpectrumFrame { sequence, time, bands: analysis.bands },
                waveform: analysis.waveform,
                decay: analysis.decay,
            });
        }
    }

    pub fn push_pcm(&self, gen: u64, offset: u64, timing: PcmTiming, samples: &[f32]) {
        let mut history = self.history.lock().unwrap();
        if history.gen != gen || self.mode() != CaptureMode::Pcm {
            return;
        }
        let pcm = &mut history.pcm;
        if pcm.samples.is_empty() || pcm.beginning + pcm.samples.len() as u64 != offset {
            pcm.samples.clear();
            pcm.beginning = offset;
        }
        pcm.timing = timing;
        pcm.samples.extend(samples.chunks_exact(timing.channels).map(stereo_sample));
        // Retain the full decoder queue plus enough samples behind the playback clock for an FFT.
        let capacity = 4 * timing.sample_rate as usize + pcm_window_size(spectrum_hop(timing.sample_rate));
        let excess = pcm.samples.len().saturating_sub(capacity);
        pcm.samples.drain(..excess);
        pcm.beginning += excess as u64;
    }

    pub fn collect_pcm(&self, time: f64, target: &mut Arc<AudioPcm>, fast_decay: bool) {
        let Ok(history) = self.history.try_lock() else { return };
        let pcm = &history.pcm;
        let timing = pcm.timing;
        let hop = spectrum_hop(timing.sample_rate);
        let played = ((time - timing.start) / timing.playback_rate * f64::from(timing.sample_rate)) as u64;
        let playable = played.min(pcm.beginning + pcm.samples.len() as u64);
        let update_hop = if fast_decay { (timing.sample_rate / DECAY_REFRESH_HZ).max(1) as usize } else { hop };
        let end = aligned_end(playable, update_hop);
        if target.revision == history.revision && target.end_frame == end {
            return;
        }
        let target = Arc::make_mut(target);
        target.revision = history.revision;
        target.end_frame = end;
        target.rate = timing.sample_rate;
        target.stereo = timing.channels > 1;
        target.hop = hop;
        target.samples.clear();
        let keep = pcm_window_size(hop);
        let start = end.saturating_sub(keep as u64).max(pcm.beginning);
        if start < end {
            let offset = (start - pcm.beginning) as usize;
            target.samples.extend(pcm.samples.range(offset..offset + (end - start) as usize).copied());
        }
    }

    pub fn collect(&self, time: f64, rows: &mut Vec<SpectrumFrame>) {
        if let Ok(history) = self.history.try_lock() {
            rows.clear();
            rows.extend(history.rows.iter().take_while(|row| row.frame.time <= time).map(|row| row.frame));
        }
    }

    pub fn collect_analysis(&self, time: f64, result: &mut AudioAnalysis) {
        if let Ok(history) = self.history.try_lock() {
            let played = history.rows.iter().take_while(|row| row.frame.time <= time);
            result.spectra.clear();
            result.spectra.extend(played.map(|row| row.frame));
            result.waveform = None;
            result.decay = None;
            for row in history.rows.iter().rev().skip(history.rows.len() - result.spectra.len()) {
                result.waveform.get_or_insert_with(|| row.waveform.clone());
                if let Some(decay) = &row.decay {
                    result.decay = Some(decay.clone());
                    break;
                }
            }
        }
    }
}

struct Transform {
    spectrum: Vec<(f32, f32)>,
    rotation: Vec<(f32, f32)>,
    ranges: [(usize, usize); SPECTRUM_BANDS],
}

impl Transform {
    fn new(rate: u32, size: usize) -> Self {
        let tau = std::f32::consts::TAU;
        let edge = |band| {
            let fraction = band as f32 / SPECTRUM_BANDS as f32;
            SPECTRUM_MIN_HZ * (SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ).powf(fraction)
                * size as f32 / rate as f32
        };
        Self {
            spectrum: vec![(0.0, 0.0); size],
            rotation: (0..size / 2).map(|i| {
                let (sin, cos) = (-tau * i as f32 / size as f32).sin_cos();
                (cos, sin)
            }).collect(),
            ranges: std::array::from_fn(|band| {
                let start = (edge(band).round() as usize).clamp(1, size / 2);
                let end = (edge(band + 1).round() as usize).max(start + 1).min(size / 2);
                (start, end)
            }),
        }
    }

    fn bands(&mut self, samples: impl Iterator<Item = (f32, f32)>, normalization: f32) -> [u8; SPECTRUM_BANDS] {
        let size = self.spectrum.len();
        for (i, sample) in samples.enumerate() {
            let reversed = i.reverse_bits() >> (usize::BITS - size.trailing_zeros());
            self.spectrum[reversed] = sample;
        }
        let mut width = 2;
        while width <= size {
            let half = width / 2;
            for block in self.spectrum.chunks_exact_mut(width) {
                let (even, odd) = block.split_at_mut(half);
                for (i, (a, b)) in even.iter_mut().zip(odd).enumerate() {
                    let (c, s) = self.rotation[i * size / width];
                    let (re, im) = (b.0 * c - b.1 * s, b.0 * s + b.1 * c);
                    *b = (a.0 - re, a.1 - im);
                    *a = (a.0 + re, a.1 + im);
                }
            }
            width *= 2;
        }
        self.ranges.map(|(start, end)| {
            // Packing left/right into one complex FFT preserves stereo energy, even in antiphase.
            let power = (start..end).map(|bin| {
                let pos = self.spectrum[bin];
                let neg = self.spectrum[size - bin];
                pos.0 * pos.0 + pos.1 * pos.1 + neg.0 * neg.0 + neg.1 * neg.1
            }).fold(0.0f32, f32::max) * normalization.powi(2);
            let db = 10.0 * power.max(1e-9).log10();
            ((db + 90.0) * (255.0 / 90.0)).round().clamp(0.0, 255.0) as u8
        })
    }
}

pub(crate) struct Analyzer {
    rate: u32,
    wave: Vec<(f32, f32)>,
    normal: Transform,
    decay: Transform,
    window: Vec<f32>,
    taper: Vec<f32>,
    cursor: usize,
    filled: usize,
    remaining: usize,
    hop: usize,
    until_decay: u8,
}

impl Analyzer {
    pub fn new(rate: u32) -> Self {
        let cosine = |angle: f32| 0.5 - 0.5 * angle.cos();
        let taper_len = (rate as usize / 500).clamp(8, DECAY_STEP / 2);
        Self {
            rate,
            wave: vec![(0.0, 0.0); DECAY_SIZE],
            normal: Transform::new(rate, FFT_SIZE),
            decay: Transform::new(rate, DECAY_SIZE),
            window: (0..FFT_SIZE).map(|i| cosine(std::f32::consts::TAU * i as f32 / FFT_SIZE as f32)).collect(),
            taper: (0..=taper_len).map(|i| cosine(std::f32::consts::PI * i as f32 / taper_len as f32)).collect(),
            cursor: 0,
            filled: 0,
            remaining: FFT_SIZE,
            hop: spectrum_hop(rate),
            until_decay: 0,
        }
    }

    pub fn add_frame(&mut self, frame: &[f32]) -> Option<Analysis> {
        let [left, right] = stereo_sample(frame);
        let stereo = frame.len() > 1;
        self.wave[self.cursor] = (left, right);
        self.cursor = (self.cursor + 1) % DECAY_SIZE;
        self.filled = (self.filled + 1).min(DECAY_SIZE);
        self.remaining -= 1;
        if self.remaining != 0 {
            return None;
        }
        self.remaining = self.hop;

        let recent = (0..FFT_SIZE).map(|i| self.wave[(self.cursor + DECAY_SIZE - FFT_SIZE + i) % DECAY_SIZE]);
        let bands = self.normal.bands(recent.clone().zip(&self.window).map(|((left, right), weight)| {
            (left * weight, right * weight)
        }), 2.0 / FFT_SIZE as f32);
        let mut waveform = Waveform {
            seconds: FFT_SIZE as f32 / self.rate as f32,
            stereo,
            ranges: [[(f32::INFINITY, f32::NEG_INFINITY); WAVEFORM_BINS]; 2],
        };
        for (i, (left, right)) in recent.enumerate() {
            let bin = i / (FFT_SIZE / WAVEFORM_BINS);
            for (ranges, sample) in waveform.ranges.iter_mut().zip([left, right]) {
                let (min, max) = &mut ranges[bin];
                *min = min.min(sample);
                *max = max.max(sample);
            }
        }

        let mut decay = None;
        if self.filled == DECAY_SIZE && self.until_decay == 0 {
            // The end stays fixed. Later slices omit progressively more of the start.
            // A short cosine taper smooths the cut and the end of the captured audio.
            let wave = &self.wave;
            let taper = &self.taper;
            let cursor = self.cursor;
            let transform = &mut self.decay;
            let slices = std::array::from_fn(|slice| {
                let cut = slice * DECAY_STEP;
                let samples = (0..DECAY_SIZE).map(|i| {
                    if i < cut {
                        return (0.0, 0.0);
                    }
                    let weight = taper[(i - cut).min(DECAY_SIZE - 1 - i).min(taper.len() - 1)];
                    let (left, right) = wave[(cursor + i) % DECAY_SIZE];
                    (left * weight, right * weight)
                });
                transform.bands(samples, 1.0 / DECAY_SIZE as f32)
            });
            decay = Some(SpectralDecay { step_seconds: DECAY_STEP as f32 / self.rate as f32, slices });
            self.until_decay = 5;
        }
        self.until_decay = self.until_decay.saturating_sub(1);
        Some(Analysis { bands, waveform, decay })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_windows_stop_at_the_playback_clock_and_invalidate_on_seek() {
        let capture = Spectrum::default();
        capture.set_mode(CaptureMode::Pcm);
        let timing = PcmTiming { start: 2.0, playback_rate: 2.0, sample_rate: 48_000, channels: 2 };
        let samples: Vec<f32> = (0..6000).flat_map(|index| [index as f32, -(index as f32)]).collect();
        capture.push_pcm(1, 0, timing, &samples);
        let mut pcm = Arc::default();
        capture.collect_pcm(2.1, &mut pcm, false);
        assert_eq!(pcm.end_frame, FFT_SIZE as u64);
        assert_eq!(pcm.samples.len(), FFT_SIZE);
        assert_eq!(*pcm.samples.last().unwrap(), [2047.0, -2047.0]);
        let previous = pcm.clone();
        capture.collect_pcm(2.11, &mut pcm, false);
        assert!(Arc::ptr_eq(&previous, &pcm));

        capture.reset(2);
        capture.push_pcm(1, 0, timing, &samples);
        capture.collect_pcm(2.1, &mut pcm, false);
        assert!(pcm.revision > previous.revision);
        assert!(pcm.samples.is_empty());
    }

    #[test]
    fn faster_decay_windows_leave_the_waterfall_grid_aligned() {
        let capture = Spectrum::default();
        capture.set_mode(CaptureMode::Pcm);
        let timing = PcmTiming { start: 0.0, playback_rate: 1.0, sample_rate: 48_000, channels: 1 };
        capture.push_pcm(1, 0, timing, &vec![0.0; 8192]);
        let mut pcm = Arc::default();
        capture.collect_pcm(0.05, &mut pcm, true);
        assert_eq!(pcm.end_frame, 2048);
        capture.collect_pcm(0.06, &mut pcm, true);
        assert_eq!(pcm.end_frame, 2848);
        assert_eq!(pcm.spectrum_end(), 2048);
        capture.collect_pcm(0.10, &mut pcm, true);
        assert_eq!(pcm.end_frame, 4448);
        assert_eq!(pcm.spectrum_end(), 3968);
    }

    #[test]
    fn an_antiphase_stereo_tone_lights_the_correct_frequency_band() {
        let mut analyzer = Analyzer::new(48_000);
        let mut bands = None;
        for i in 0..FFT_SIZE {
            let x = (std::f32::consts::TAU * i as f32 / 48.0).sin() * 0.5;
            bands = analyzer.add_frame(&[x, -x]).map(|sample| sample.bands);
        }
        let bands = bands.unwrap();
        let (position, level) = bands.iter().enumerate().max_by_key(|(_, level)| *level).unwrap();
        let edge = |band| SPECTRUM_MIN_HZ * (SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ).powf(band as f32 / SPECTRUM_BANDS as f32);
        assert!((edge(position)..edge(position + 1)).contains(&1000.0));
        assert!((232..246).contains(level), "{level}");
        assert!(bands[0] < 30);
    }

    #[test]
    fn waveform_preserves_both_channels_and_the_decay_omits_earlier_audio() {
        let mut analyzer = Analyzer::new(48_000);
        let mut received = None;
        for i in 0..DECAY_SIZE * 2 {
            let pulse = if (3000..5000).contains(&i) {
                (std::f32::consts::TAU * i as f32 / 48.0).sin() * 0.8
            } else {
                0.0
            };
            if let Some(sample) = analyzer.add_frame(&[pulse, -pulse * 0.5]) {
                if (3000..7000).contains(&i) {
                    let [(left_min, left_max), (right_min, right_max)] = sample.waveform.ranges.map(|ranges| {
                        ranges.into_iter().fold((0.0f32, 0.0f32), |(min, max), (lo, hi)| (min.min(lo), max.max(hi)))
                    });
                    assert!((left_min + 0.8).abs() < 0.02 && (left_max - 0.8).abs() < 0.02);
                    assert!((right_min + 0.4).abs() < 0.02 && (right_max - 0.4).abs() < 0.02);
                }
                if sample.decay.is_some() {
                    received = sample.decay;
                    break;
                }
            }
        }
        let decay = received.expect("a full decay window");
        assert!(decay.slices[0].iter().any(|&value| value > 180));
        assert!(decay.slices[DECAY_SLICES - 1].iter().all(|&value| value == 0));
    }
}
