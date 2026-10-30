//! Voice activity: a cheap speech detector, from energy above an adaptive noise floor, zero
//! crossings and syllabic modulation. It is a gate for a transcription model, not a classifier:
//! music with a voice-like rhythm can pass it, while a steady tone, hum or hiss does not.

use crate::{round, Analyzer, Input, Observation};
use serde_json::{json, Value};
use std::collections::VecDeque;

/// Analysis frame, in seconds.
const FRAME: f64 = 0.02;
/// A frame this many dB above the noise floor may be speech...
const ABOVE_FLOOR: f64 = 10.0;
/// ...and must also be at least this loud (dBFS).
const MIN_DB: f64 = -55.0;
/// Where the noise floor starts, how fast it rises (dB per frame: 1 dB/s) while nothing quieter
/// comes, and how low it goes: digital silence must not leave it where it takes a minute to recover.
const START_FLOOR: f64 = -50.0;
const FLOOR_RISE: f64 = 0.02;
const LOWEST_FLOOR: f64 = -75.0;
/// Zero crossings per sample above this are hiss or noise, not voiced speech.
const MAX_ZCR: f64 = 0.35;
/// Speech comes in syllables: loud frames over the last second vary by at least this many dB
/// (standard deviation), and at least `MIN_LOUD` of them are needed to tell.
const MIN_MODULATION: f64 = 3.0;
const MIN_LOUD: usize = 10;
/// Speech starts when `START_ACTIVE` of the last `START_WINDOW` frames are active, and ends after
/// `HANGOVER` inactive frames in a row.
const START_WINDOW: usize = 15;
const START_ACTIVE: usize = 10;
const HANGOVER: usize = 25;

pub struct VoiceActivity {
    /// Mono samples not yet in a frame, and the time of the first.
    acc: Vec<f64>,
    acc_t: f64,
    rate: u32,
    floor: f64,
    /// Times and energies of the loud frames in the last second.
    loud: VecDeque<(f64, f64)>,
    /// Start times and verdicts of the last `START_WINDOW` frames.
    recent: VecDeque<(f64, bool)>,
    /// The start of the speech under way, the end of its last active frame, and inactive frames since.
    start: Option<f64>,
    last_active: f64,
    quiet: usize,
    window: f64,
    total: f64,
    count: u64,
}

impl Default for VoiceActivity {
    fn default() -> Self {
        VoiceActivity {
            acc: Vec::new(),
            acc_t: 0.0,
            rate: 0,
            floor: START_FLOOR,
            loud: VecDeque::new(),
            recent: VecDeque::new(),
            start: None,
            last_active: 0.0,
            quiet: 0,
            window: 0.0,
            total: 0.0,
            count: 0,
        }
    }
}

impl VoiceActivity {
    /// Whether speech is under way.
    pub fn speaking(&self) -> bool {
        self.start.is_some()
    }

    fn frame(&mut self, t: f64, x: &[f64], out: &mut Vec<Observation>) {
        let dur = x.len() as f64 / self.rate as f64;
        let e = 10.0 * (x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64 + 1e-12).log10();
        let zcr = x.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f64 / x.len() as f64;
        self.floor = if e < self.floor { e.max(LOWEST_FLOOR) } else { self.floor + FLOOR_RISE };
        let is_loud = e > self.floor + ABOVE_FLOOR && e > MIN_DB;
        if is_loud {
            self.loud.push_back((t, e));
        }
        while self.loud.front().is_some_and(|&(lt, _)| lt < t - 1.0) {
            self.loud.pop_front();
        }
        let modulated = self.loud.len() >= MIN_LOUD && {
            let n = self.loud.len() as f64;
            let mean = self.loud.iter().map(|l| l.1).sum::<f64>() / n;
            (self.loud.iter().map(|l| (l.1 - mean).powi(2)).sum::<f64>() / n).sqrt() >= MIN_MODULATION
        };
        let active = is_loud && zcr < MAX_ZCR && modulated;
        self.recent.push_back((t, active));
        if self.recent.len() > START_WINDOW {
            self.recent.pop_front();
        }
        match self.start {
            None => {
                if self.recent.iter().filter(|r| r.1).count() >= START_ACTIVE {
                    let first = self.recent.iter().find(|r| r.1).map_or(t, |r| r.0);
                    self.start = Some(first);
                    self.last_active = t + dur;
                    self.quiet = 0;
                    self.count += 1;
                    self.window += t + dur - first;
                    self.total += t + dur - first;
                    out.push(Observation::new("speech_start", "voice_activity", first, Value::Object(Default::default())));
                }
            }
            Some(_) => {
                self.window += dur;
                self.total += dur;
                if active {
                    self.last_active = t + dur;
                    self.quiet = 0;
                } else {
                    self.quiet += 1;
                    if self.quiet >= HANGOVER {
                        // the hangover was not speech
                        let tail = self.quiet as f64 * dur;
                        self.window = (self.window - tail).max(0.0);
                        self.total = (self.total - tail).max(0.0);
                        self.end(out);
                    }
                }
            }
        }
    }

    fn end(&mut self, out: &mut Vec<Observation>) {
        if let Some(start) = self.start.take() {
            out.push(Observation::new("speech_end", "voice_activity", start, json!({ "duration": round(self.last_active - start, 3) })).until(self.last_active));
        }
        self.quiet = 0;
    }
}

impl Analyzer for VoiceActivity {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Audio { frame, t } = input else { return };
        let ch = frame.channels.max(1) as usize;
        if frame.sample_rate == 0 {
            return;
        }
        if frame.sample_rate != self.rate {
            self.rate = frame.sample_rate;
            self.acc.clear();
        }
        if self.acc.is_empty() {
            self.acc_t = *t;
        }
        self.acc.extend(frame.samples.chunks(ch).map(|c| c.iter().sum::<f64>() / ch as f64));
        let len = ((self.rate as f64 * FRAME) as usize).max(1);
        let mut used = 0;
        while self.acc.len() - used >= len {
            let x = self.acc[used..used + len].to_vec();
            let ft = self.acc_t;
            self.frame(ft, &x, out);
            used += len;
            self.acc_t += len as f64 / self.rate as f64;
        }
        self.acc.drain(..used);
    }

    fn digest(&mut self, seconds: f64) -> Option<Value> {
        let w = std::mem::take(&mut self.window);
        Some(json!({ "speech_seconds": round(w, 3), "speech_fraction": if seconds > 0.0 { round((w / seconds).min(1.0), 3) } else { 0.0 } }))
    }

    fn finish(&mut self, _t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        self.end(out);
        Some(json!({ "spans": self.count, "seconds": round(self.total, 3) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::AudioFrame;
    use std::f64::consts::PI;

    fn run(samples: &[f64], rate: u32) -> Vec<(String, f64, Option<f64>)> {
        let mut v = VoiceActivity::default();
        let mut out = Vec::new();
        for (k, c) in samples.chunks(1024).enumerate() {
            let f = AudioFrame { sample_rate: rate, channels: 1, pts: 0, samples: c.to_vec() };
            v.observe(&Input::Audio { frame: &f, t: (k * 1024) as f64 / rate as f64 }, &mut out);
        }
        v.finish(0.0, &mut out);
        out.iter().map(|o| (o.kind.to_string(), o.t, o.t_end)).collect()
    }

    /// A voiced sound: harmonics of 120 Hz under a 4 Hz syllable envelope, from `a` to `b` seconds.
    pub(crate) fn voice(rate: u32, secs: f64, a: f64, b: f64) -> Vec<f64> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let noise = 0.001 * ((i * 7919 % 1000) as f64 / 500.0 - 1.0);
                if t < a || t >= b {
                    return noise;
                }
                let env = 0.5 - 0.5 * (2.0 * PI * 4.0 * (t - a)).cos();
                let h: f64 = (1..8).map(|k| (2.0 * PI * 120.0 * k as f64 * t).sin() / k as f64).sum();
                noise + 0.2 * env * h
            })
            .collect()
    }

    #[test]
    fn speech_is_found_and_a_steady_tone_is_not() {
        let got = run(&voice(16000, 6.0, 1.0, 4.0), 16000);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].0, "speech_start");
        assert!((got[0].1 - 1.0).abs() < 0.3, "{got:?}");
        let end = got[1].2.unwrap();
        assert!((end - 4.0).abs() < 0.3, "{got:?}");
        // a 1 kHz tone between silences
        let rate = 48000;
        let tone: Vec<f64> = (0..rate * 6).map(|i| if (rate..rate * 4).contains(&i) { 0.1 * (2.0 * PI * 1000.0 * i as f64 / rate as f64).sin() } else { 0.0 }).collect();
        assert_eq!(run(&tone, rate as u32), []);
        // white noise
        let mut x = 1u32;
        let hiss: Vec<f64> = (0..48000 * 3)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                0.2 * (x as f64 / u32::MAX as f64 - 0.5)
            })
            .collect();
        assert_eq!(run(&hiss, 48000), []);
    }
}
