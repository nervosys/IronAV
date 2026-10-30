//! Made-up sources for testing a pipeline without a camera or a microphone: `test://video` (Y4M)
//! and `test://audio` (WAV), made at the pace they play. Their [`SourceInfo`] says `synthetic`,
//! so nothing downstream can take them for real footage.
//!
//! The picture is a grey ramp with a white square crossing it once a second, and its colour turns
//! every 5 s (a scene cut). The frame number is written in its top row as 32 blocks of 8 pixels,
//! black for 0 and white for 1, most significant first, so a reader can tell which frame it has
//! and how late. The sound is a 440 Hz tone, on for half of each second. `size=WxH` (320x240),
//! `fps=N` (25), `rate=HZ` (16000) and `duration=S` (unending) change them.

use crate::capture::{parse_fps, parse_size, wav_header, y4m_header};
use crate::source::{Chunk, Source, SourceInfo};
use crate::url::SourceUrl;
use ironav_core::{Error, Rational, Result};
use std::time::{Duration, Instant};

enum Kind {
    Video { w: usize, h: usize, fps: f64 },
    Audio { rate: u32 },
}

pub(crate) struct Synthetic {
    info: SourceInfo,
    kind: Kind,
    start: Instant,
    /// Frames (or 10 ms blocks of sound) made so far, and how many to make.
    made: u64,
    total: Option<u64>,
    out: Vec<u8>,
    at: usize,
}

/// The frame number `k` from the top row of a picture this source made.
pub fn frame_number(luma: &[u8], width: usize) -> Option<u32> {
    if width < 256 || luma.len() < 256 {
        return None;
    }
    Some((0..32).fold(0u32, |n, b| n << 1 | (luma[b * 8 + 4] > 128) as u32))
}

pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<Synthetic> {
    let duration = u
        .option("duration")
        .map(|s| s.parse::<f64>().ok().filter(|d| *d > 0.0 && d.is_finite()).ok_or_else(|| Error::invalid_arg(format!("duration must be a positive number of seconds, got '{s}'"))))
        .transpose()?;
    let (kind, per_second) = match u.target.trim_end_matches('/') {
        "video" => {
            let (w, h) = u.option("size").map(parse_size).transpose()?.unwrap_or((320, 240));
            if w % 2 != 0 || h % 2 != 0 {
                return Err(Error::invalid_arg("a test picture's width and height must be even"));
            }
            let fps = u.option("fps").map(parse_fps).transpose()?.unwrap_or(25.0);
            (Kind::Video { w: w as usize, h: h as usize, fps }, fps)
        }
        "audio" => {
            let rate = u.option("rate").map(|s| s.parse::<u32>().ok().filter(|r| (8000..=192_000).contains(r)).ok_or_else(|| Error::invalid_arg(format!("rate must be 8000 to 192000, got '{s}'")))).transpose()?.unwrap_or(16_000);
            if rate % 100 != 0 {
                return Err(Error::invalid_arg("a test sound's rate must be a multiple of 100"));
            }
            (Kind::Audio { rate }, 100.0)
        }
        other => return Err(Error::invalid_arg(format!("test sources are test://video and test://audio, not '{other}'")).alternatives(["test://video", "test://audio"])),
    };
    let out = match &kind {
        Kind::Video { w, h, fps } => y4m_header(*w as u32, *h as u32, Rational::new((fps * 1000.0).round() as i64, 1000)),
        Kind::Audio { rate } => wav_header(*rate, 1),
    };
    Ok(Synthetic {
        info: SourceInfo { url: shown, kind: "test", live: true, synthetic: true },
        kind,
        start: Instant::now(),
        made: 0,
        total: duration.map(|d| (d * per_second).round() as u64),
        out,
        at: 0,
    })
}

impl Synthetic {
    /// The next frame, or 10 ms of sound, into `out`.
    fn make(&mut self) {
        let k = self.made;
        self.made += 1;
        match self.kind {
            Kind::Video { w, h, fps } => {
                let t = k as f64 / fps;
                self.out.extend_from_slice(b"FRAME\n");
                let start = self.out.len();
                self.out.resize(start + w * h * 3 / 2, 0);
                let (luma, chroma) = self.out[start..].split_at_mut(w * h);
                let side = (h / 4).max(2);
                let x0 = ((t.fract() * (w + side) as f64) as usize).saturating_sub(side);
                let y0 = (h - side) / 2;
                for y in 0..h {
                    for x in 0..w {
                        let inside = x >= x0 && x < x0 + side && y >= y0 && y < y0 + side;
                        luma[y * w + x] = if inside { 235 } else { (16 + x * 200 / w) as u8 };
                    }
                }
                for b in 0..32.min(w / 8) {
                    let v = if (k as u32 >> (31 - b)) & 1 == 1 { 235 } else { 16 };
                    for y in 0..8.min(h) {
                        luma[y * w + b * 8..y * w + b * 8 + 8].fill(v);
                    }
                }
                // a new colour every 5 s
                let (u, v) = [(90, 200), (200, 90), (160, 160), (90, 90)][(t / 5.0) as usize % 4];
                let (cu, cv) = chroma.split_at_mut(w * h / 4);
                cu.fill(u);
                cv.fill(v);
            }
            Kind::Audio { rate } => {
                let n = rate as u64 / 100;
                for i in 0..n {
                    let s = k * n + i;
                    let t = s as f64 / rate as f64;
                    let on = t.fract() < 0.5;
                    let v = if on { (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32767.0) as i16 } else { 0 };
                    self.out.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
    }

    fn per_second(&self) -> f64 {
        match self.kind {
            Kind::Video { fps, .. } => fps,
            Kind::Audio { .. } => 100.0,
        }
    }
}

impl Source for Synthetic {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        if self.at >= self.out.len() {
            self.out.clear();
            self.at = 0;
            if self.total.is_some_and(|t| self.made >= t) {
                return Ok(Chunk::End);
            }
            // each frame when it is due
            let due = self.start + Duration::from_secs_f64(self.made as f64 / self.per_second());
            let now = Instant::now();
            if due > now {
                std::thread::sleep((due - now).min(wait));
                if Instant::now() < due {
                    return Ok(Chunk::Idle);
                }
            }
            self.make();
        }
        let n = buf.len().min(self.out.len() - self.at);
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(Chunk::Data(n))
    }
}
