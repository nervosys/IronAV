//! Video analyzers, all working on luma averaged over 4x4 blocks: scene cuts, black and frozen
//! pictures, and thumbnails.

use crate::{round, Analyzer, Input, LowRes, Observation, Picture, Settings, Spans};
use ironav_codec::change::{change_score, SCENE_CUT};
use serde_json::{json, Value};

#[derive(Default)]
pub struct Scene {
    prev: Option<LowRes>,
    cuts: u64,
    all_cuts: u64,
    sum: f64,
    max: f64,
    n: u64,
}

impl Analyzer for Scene {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Video { low, t, .. } = input else { return };
        if let Some(prev) = &self.prev {
            let score = change_score(prev, low);
            out.push(Observation::new("change", "scene", *t, json!({ "score": round(score, 3) })));
            if score > SCENE_CUT {
                self.cuts += 1;
                out.push(Observation::new("scene_cut", "scene", *t, json!({ "score": round(score, 2) })));
            }
            self.sum += score;
            self.max = self.max.max(score);
            self.n += 1;
        }
        self.prev = Some((*low).clone());
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        let v = json!({ "cuts": self.cuts, "mean_change": round(self.sum / self.n.max(1) as f64, 3), "max_change": round(self.max, 3) });
        self.all_cuts += self.cuts;
        (self.cuts, self.sum, self.max, self.n) = (0, 0.0, 0.0, 0);
        Some(v)
    }

    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        self.digest(0.0);
        Some(json!({ "cuts": self.all_cuts }))
    }
}

/// A block average at or below this (8-bit levels) is black: 10 % of the way from video black (16)
/// to white (235), as FFmpeg's blackdetect judges it.
const BLACK_LUMA: i32 = 38;
/// The share of the picture that must be black.
const BLACK_SHARE: f64 = 0.98;

pub struct Black {
    spans: Spans,
    last_end: f64,
}

impl Black {
    pub fn new() -> Self {
        Black { spans: Spans::new("black", "black_start", "black_end", 0.5), last_end: 0.0 }
    }
}

impl Analyzer for Black {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Video { low, t, dt, .. } = input else { return };
        let black = low.data.iter().filter(|&&v| v <= BLACK_LUMA).count() as f64 >= BLACK_SHARE * low.data.len().max(1) as f64;
        self.spans.update(*t, *dt, black, out);
        self.last_end = t + dt;
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        Some(json!({ "black_seconds": round(self.spans.take_window(), 3) }))
    }

    fn finish(&mut self, t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        self.spans.close(t.max(self.last_end), out);
        Some(self.spans.summary())
    }
}

/// Pictures whose block averages differ from the last picture's by less than this (8-bit levels,
/// on average) have not changed.
const FREEZE_MAD: f64 = 0.3;

pub struct Freeze {
    spans: Spans,
    prev: Option<LowRes>,
    last_end: f64,
}

impl Freeze {
    pub fn new() -> Self {
        Freeze { spans: Spans::new("freeze", "freeze_start", "freeze_end", 1.0), prev: None, last_end: 0.0 }
    }
}

impl Analyzer for Freeze {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Video { low, t, dt, .. } = input else { return };
        let frozen = self.prev.as_ref().is_some_and(|p| p.data.len() == low.data.len() && low.mean_abs_diff(p) < FREEZE_MAD);
        // a frozen picture has been frozen since the one before it was shown
        let (start, len) = if frozen && self.spans_idle() { (t - dt, 2.0 * dt) } else { (*t, *dt) };
        self.spans.update(start, len, frozen, out);
        self.prev = Some((*low).clone());
        self.last_end = t + dt;
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        Some(json!({ "frozen_seconds": round(self.spans.take_window(), 3) }))
    }

    fn finish(&mut self, t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        self.spans.close(t.max(self.last_end), out);
        Some(self.spans.summary())
    }
}

impl Freeze {
    fn spans_idle(&self) -> bool {
        !self.spans.is_open()
    }
}

/// A 64-bit difference hash: each bit says whether a cell of a 9x8 grid is brighter than its right neighbour.
pub fn dhash(low: &LowRes) -> u64 {
    if low.w < 9 || low.h < 8 {
        return 0;
    }
    let cell = |cx: usize, cy: usize| {
        let (x0, x1) = (cx * low.w / 9, (cx + 1) * low.w / 9);
        let (y0, y1) = (cy * low.h / 8, (cy + 1) * low.h / 8);
        let mut sum = 0i64;
        for y in y0..y1 {
            sum += low.data[y * low.w + x0..y * low.w + x1].iter().map(|&v| v as i64).sum::<i64>();
        }
        sum as f64 / ((x1 - x0) * (y1 - y0)).max(1) as f64
    };
    let mut h = 0u64;
    for cy in 0..8 {
        for cx in 0..8 {
            h = h << 1 | (cell(cx, cy) > cell(cx + 1, cy)) as u64;
        }
    }
    h
}

/// Thumbnails come at most this often, however much the picture changes.
const THUMB_MIN_GAP: f64 = 2.0;
/// Hashes this many bits apart show different content.
const THUMB_NEW_CONTENT: u32 = 16;

pub struct Thumbnails {
    every: f64,
    width: u32,
    last: Option<(f64, u64)>,
    count: u64,
    window: u64,
}

impl Thumbnails {
    pub fn new(s: &Settings) -> Self {
        Thumbnails { every: s.thumbnail_seconds.max(THUMB_MIN_GAP), width: s.thumbnail_width.max(16), last: None, count: 0, window: 0 }
    }
}

impl Analyzer for Thumbnails {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Video { frame, low, t, .. } = input else { return };
        let hash = dhash(low);
        let due = match self.last {
            None => true,
            Some((lt, lh)) => t - lt >= self.every || (t - lt >= THUMB_MIN_GAP && (hash ^ lh).count_ones() > THUMB_NEW_CONTENT),
        };
        if !due || frame.width == 0 || frame.height == 0 {
            return;
        }
        self.last = Some((*t, hash));
        self.count += 1;
        self.window += 1;
        let w = (self.width.min(frame.width) & !1).max(2);
        let h = ((frame.height as u64 * w as u64 / frame.width as u64) as u32 & !1).max(2);
        let mut o = Observation::new("thumbnail", "thumbnails", *t, json!({ "hash": format!("{hash:016x}"), "width": w, "height": h }));
        o.picture = Some(Picture(ironav_codec::resize_frame(frame, w, h)));
        out.push(o);
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        Some(json!({ "thumbnails": std::mem::take(&mut self.window) }))
    }

    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        Some(json!({ "thumbnails": self.count }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{PixelFormat, VideoFrame};

    /// A 64x48 grey picture of level `v`, with a bright square at `sx` when given.
    fn picture(v: u8, square: Option<usize>) -> VideoFrame {
        let (w, h) = (64usize, 48usize);
        let mut y = vec![v; w * h];
        if let Some(sx) = square {
            for r in 8..24 {
                for c in sx..sx + 16 {
                    y[r * w + c] = 235;
                }
            }
        }
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![y, vec![128; w * h / 4], vec![128; w * h / 4]] }
    }

    fn run(a: &mut dyn Analyzer, pics: &[VideoFrame]) -> Vec<Observation> {
        let mut out = Vec::new();
        let dt = 0.04;
        for (i, p) in pics.iter().enumerate() {
            let low = LowRes::of_plane(&p.planes[0], p.width as usize, 1, 0, p.width as usize, p.height as usize);
            a.observe(&Input::Video { frame: p, low: &low, t: i as f64 * dt, dt }, &mut out);
        }
        a.finish(pics.len() as f64 * dt, &mut out);
        out
    }

    #[test]
    fn black_pictures_between_content_are_one_span() {
        let pics: Vec<_> = (0..75).map(|i| if (25..50).contains(&i) { picture(16, None) } else { picture(120, Some(8)) }).collect();
        let out = run(&mut Black::new(), &pics);
        let spans: Vec<_> = out.iter().map(|o| (o.kind, o.t, o.t_end)).collect();
        assert_eq!(spans, [("black_start", 1.0, None), ("black_end", 1.0, Some(2.0))]);
    }

    #[test]
    fn a_repeated_picture_is_a_freeze_and_moving_content_is_not() {
        // moving for 1 s, still for 2 s, moving again
        let pics: Vec<_> = (0..100).map(|i| picture(100, Some(if (25..75).contains(&i) { 20 } else { i % 40 }))).collect();
        let out = run(&mut Freeze::new(), &pics);
        let spans: Vec<_> = out.iter().map(|o| (o.kind, o.t, o.t_end)).collect();
        assert_eq!(spans, [("freeze_start", 1.0, None), ("freeze_end", 1.0, Some(3.0))], "{out:?}");
    }

    #[test]
    fn a_change_of_content_is_a_cut_and_motion_is_not() {
        let mut pics: Vec<_> = (0..10).map(|i| picture(60, Some(4 + i))).collect();
        pics.extend((0..10).map(|i| picture(200, Some(40 - i))));
        let out = run(&mut Scene::default(), &pics);
        let cuts: Vec<_> = out.iter().filter(|o| o.kind == "scene_cut").map(|o| o.t).collect();
        assert_eq!(cuts, [0.4]);
    }

    #[test]
    fn thumbnails_follow_new_content_but_not_too_often() {
        let s = Settings { thumbnail_seconds: 100.0, thumbnail_width: 32 };
        // the picture changes at 1 s, too soon after the first thumbnail, so the next comes at
        // 2 s; it changes back at 3 s, too soon after that one
        let ramp = |rising: bool| {
            let mut p = picture(0, None);
            for (i, v) in p.planes[0].iter_mut().enumerate() {
                let x = (i % 64) as u8;
                *v = 16 + 3 * if rising { x } else { 63 - x };
            }
            p
        };
        let pics: Vec<_> = (0..100).map(|i| ramp((25..75).contains(&i))).collect();
        let out = run(&mut Thumbnails::new(&s), &pics);
        let at: Vec<_> = out.iter().map(|o| o.t).collect();
        assert_eq!(at, [0.0, 2.0], "{out:?}");
        let p = &out[0].picture.as_ref().unwrap().0;
        assert_eq!((p.width, p.height), (32, 24));
    }
}
