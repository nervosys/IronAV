//! Deinterlacing: turns pictures whose two fields were captured at different moments into progressive
//! ones, so that moving edges do not show as combs.
//!
//! The `bwdif` method follows the published BWDIF description (a descendant of YADIF). The lines of
//! the field being kept pass through, and each missing line is predicted from the same line of the
//! fields of that parity just before and after it (the temporal prediction). How far the prediction may
//! be trusted comes from the temporal differences around it: where they are zero (nothing moved) the
//! prediction is the answer, so static areas come out as the source; where they are not, a spatial
//! interpolation of the kept field (a four-tap cubic, with a high-pass correction from the temporal
//! neighbours where the field's own lines differ strongly) is used, clipped to the range the
//! differences allow around the prediction. The `linear` method just averages the lines above and
//! below, for speed. Both work on 8-bit and 10 or 12-bit planar YUV and on gray, and on several
//! threads for large pictures.
//!
//! [`Deinterlacer`] holds the pictures the temporal prediction looks at; the `deinterlace` filter wraps
//! it. A player can use [`Deinterlacer`] on a stream, or [`deinterlace_frame`] for one picture on its own.

use crate::pool;
use ironav_core::{Error, FieldOrder, Filter, Frame, PixelFormat, Rational, Result, StreamParams, VideoFrame};

/// How a missing line is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeintMethod {
    /// Motion-adaptive, from the previous, current and next fields (the default).
    #[default]
    Bwdif,
    /// The mean of the lines above and below, in the kept field only.
    Linear,
}

/// How many pictures come out for each one that goes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeintMode {
    /// One per picture, from its first field.
    #[default]
    Frame,
    /// One per field, so twice as many, at twice the rate.
    Field,
}

/// Which field of a picture is the first in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeintParity {
    /// What the decoder says for each picture; top first when it does not say.
    #[default]
    Auto,
    TopFirst,
    BottomFirst,
}

/// Which pictures are deinterlaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeintSelect {
    #[default]
    All,
    /// Only those the decoder flags as interlaced; the rest pass unchanged.
    Interlaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeintOptions {
    pub method: DeintMethod,
    pub mode: DeintMode,
    pub parity: DeintParity,
    pub select: DeintSelect,
}

/// The line-finding kernels. `Cubic` is the spatial part of `Bwdif` alone, for a picture seen on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kernel {
    Bwdif,
    Linear,
    Cubic,
}

struct Held {
    frame: VideoFrame,
    order: FieldOrder,
}

/// The state of a deinterlacer: the pictures before and after the one being worked on.
///
/// Each picture is held back until the next arrives (the temporal prediction looks one picture
/// ahead), so [`Deinterlacer::push`] returns the output for the previous picture, and
/// [`Deinterlacer::flush`] the last one at the end of the stream.
///
/// In field mode the timestamps come out in units of half the input's: a picture stamped `pts` gives
/// pictures stamped `2 * pts` and `2 * pts + dt` (`dt` is the distance to the next picture), so the
/// second field's time is exact whatever the time base. [`Deinterlacer::time_scale`] is that factor
/// (2 in field mode, 1 otherwise); whoever writes the stream divides the time base by it.
pub struct Deinterlacer {
    opts: DeintOptions,
    prev: Option<Held>,
    cur: Option<Held>,
    last_dt: i64,
}

impl Deinterlacer {
    pub fn new(opts: DeintOptions) -> Self {
        Deinterlacer { opts, prev: None, cur: None, last_dt: 0 }
    }

    /// The factor the timestamps of the pictures coming out are scaled by.
    pub fn time_scale(&self) -> i64 {
        if self.opts.mode == DeintMode::Field { 2 } else { 1 }
    }

    /// Takes the next picture, with the field order its decoder reported (`FieldOrder::Unknown` if none).
    /// Returns the output for the picture before it (nothing for the first).
    pub fn push(&mut self, frame: VideoFrame, order: FieldOrder) -> Result<Vec<VideoFrame>> {
        validate(&frame)?;
        let held = Held { frame, order };
        if self.cur.is_none() {
            self.cur = Some(held);
            return Ok(Vec::new());
        }
        let out = self.emit(Some(&held.frame));
        self.prev = self.cur.take();
        self.cur = Some(held);
        Ok(out)
    }

    /// The output for the last picture, at the end of the stream. The deinterlacer is empty again after.
    pub fn flush(&mut self) -> Result<Vec<VideoFrame>> {
        if self.cur.is_none() {
            return Ok(Vec::new());
        }
        let out = self.emit(None);
        self.prev = None;
        self.cur = None;
        Ok(out)
    }

    fn emit(&mut self, next: Option<&VideoFrame>) -> Vec<VideoFrame> {
        let cur = self.cur.as_ref().expect("emit needs a current picture");
        let f = &cur.frame;
        let dt = match next {
            Some(n) if n.pts > f.pts => n.pts - f.pts,
            _ if self.last_dt > 0 => self.last_dt,
            _ => 1,
        };
        let same = |o: &VideoFrame| o.width == f.width && o.height == f.height && o.pix_fmt == f.pix_fmt;
        let prev = self.prev.as_ref().map(|h| &h.frame).filter(|p| same(p)).unwrap_or(f);
        let nxt = next.filter(|n| same(n)).unwrap_or(f);
        let deint = self.opts.select == DeintSelect::All || cur.order.is_interlaced();
        let tff = match self.opts.parity {
            DeintParity::TopFirst => true,
            DeintParity::BottomFirst => false,
            DeintParity::Auto => cur.order != FieldOrder::BottomFirst,
        };
        let first = if tff { 0 } else { 1 };
        // a picture with no neighbours has no motion to see: interpolate within it
        let alone = self.prev.is_none() && next.is_none();
        let kernel = match self.opts.method {
            DeintMethod::Bwdif if alone => Kernel::Cubic,
            DeintMethod::Bwdif => Kernel::Bwdif,
            DeintMethod::Linear => Kernel::Linear,
        };
        let out = match self.opts.mode {
            // the first field's missing lines lie between the previous picture's second field and this one's second
            DeintMode::Frame if deint => vec![render(f, prev, nxt, (prev, f), first, kernel, f.pts)],
            DeintMode::Frame => vec![f.clone()],
            DeintMode::Field => {
                let (t0, t1) = (f.pts * 2, f.pts * 2 + dt);
                if deint {
                    // the second field's missing lines lie between this picture's first field and the next one's first
                    vec![render(f, prev, nxt, (prev, f), first, kernel, t0), render(f, prev, nxt, (f, nxt), 1 - first, kernel, t1)]
                } else {
                    vec![VideoFrame { pts: t0, ..f.clone() }, VideoFrame { pts: t1, ..f.clone() }]
                }
            }
        };
        self.last_dt = dt;
        out
    }
}

/// One picture on its own, deinterlaced without looking at its neighbours: the first field is kept and
/// the other interpolated from it with a four-tap cubic. For a player that has no neighbours to hand;
/// a [`Deinterlacer`] does better on a stream, since it uses what the other fields show. `order` is what
/// the decoder reported (the bottom field is kept first only when it says bottom first).
pub fn deinterlace_frame(frame: &VideoFrame, order: FieldOrder) -> Result<VideoFrame> {
    validate(frame)?;
    let first = if order == FieldOrder::BottomFirst { 1 } else { 0 };
    Ok(render(frame, frame, frame, (frame, frame), first, Kernel::Cubic, frame.pts))
}

fn validate(f: &VideoFrame) -> Result<()> {
    if f.pix_fmt == PixelFormat::Rgb24 {
        return Err(Error::invalid_arg("deinterlace takes planar YUV or gray, not rgb24")
            .hint("put `format:pix_fmt=yuv420p` before it")
            .alternatives(["yuv420p", "yuv422p", "yuv444p", "gray8"]));
    }
    let layout = f.pix_fmt.planes(f.width as usize, f.height as usize);
    if f.planes.len() != layout.len() || f.planes.iter().zip(&layout).any(|(p, &(w, h, b))| p.len() != w * h * b) {
        return Err(Error::invalid_input("a video frame's planes do not match its size and pixel format"));
    }
    Ok(())
}

/// A sample of 8 or 16 bits (little endian) in a plane's bytes.
trait Px {
    const BYTES: usize;
    fn rd(b: &[u8], x: usize) -> i32;
    fn wr(b: &mut [u8], x: usize, v: i32);
}

struct P8;
struct P16;

impl Px for P8 {
    const BYTES: usize = 1;
    #[inline(always)]
    fn rd(b: &[u8], x: usize) -> i32 {
        b[x] as i32
    }
    #[inline(always)]
    fn wr(b: &mut [u8], x: usize, v: i32) {
        b[x] = v as u8;
    }
}

impl Px for P16 {
    const BYTES: usize = 2;
    #[inline(always)]
    fn rd(b: &[u8], x: usize) -> i32 {
        u16::from_le_bytes([b[2 * x], b[2 * x + 1]]) as i32
    }
    #[inline(always)]
    fn wr(b: &mut [u8], x: usize, v: i32) {
        b[2 * x..2 * x + 2].copy_from_slice(&(v as u16).to_le_bytes());
    }
}

/// The planes one plane of output is made from.
struct Src<'a> {
    w: usize,
    h: usize,
    /// The parity of the lines kept from `cur` (0 = even lines).
    keep: usize,
    max: i32,
    kernel: Kernel,
    cur: &'a [u8],
    prev: &'a [u8],
    next: &'a [u8],
    prev2: &'a [u8],
    next2: &'a [u8],
}

/// Line `y` mirrored back into the plane (the mirror keeps its parity).
#[inline]
fn mirror(y: isize, h: usize) -> usize {
    let h = h as isize;
    let y = if y < 0 { -y } else { y };
    let y = if y >= h { 2 * (h - 1) - y } else { y };
    y.clamp(0, h - 1) as usize
}

/// Deinterlaces `cur`, keeping its lines of parity `keep`. The other lines are predicted from `around`,
/// the pictures that hold that parity's fields just before and after, and `prev` and `next` (the
/// neighbouring pictures, for the motion test against `cur`).
fn render(cur: &VideoFrame, prev: &VideoFrame, next: &VideoFrame, around: (&VideoFrame, &VideoFrame), keep: usize, kernel: Kernel, pts: i64) -> VideoFrame {
    let (w, h) = (cur.width as usize, cur.height as usize);
    let max = (1i32 << cur.pix_fmt.bit_depth()) - 1;
    let layout = cur.pix_fmt.planes(w, h);
    let mut planes = Vec::with_capacity(layout.len());
    for (i, &(pw, ph, bpp)) in layout.iter().enumerate() {
        let mut out = vec![0u8; pw * ph * bpp];
        let src = Src { w: pw, h: ph, keep, max, kernel, cur: &cur.planes[i], prev: &prev.planes[i], next: &next.planes[i], prev2: &around.0.planes[i], next2: &around.1.planes[i] };
        let stride = pw * bpp;
        // slices of whole line pairs, one task each; small planes stay on this thread
        let tasks = if pw * ph < 1 << 16 { 1 } else { pool::threads() * 2 };
        let per = (ph.div_ceil(tasks).max(16) + 1) & !1;
        if per >= ph || stride == 0 {
            fill(&src, bpp, 0, &mut out);
        } else {
            pool::each(out.chunks_mut(per * stride).collect(), |k, rows| fill(&src, bpp, k * per, rows));
        }
        planes.push(out);
    }
    VideoFrame { width: cur.width, height: cur.height, pix_fmt: cur.pix_fmt, pts, planes }
}

fn fill(s: &Src, bpp: usize, y0: usize, rows: &mut [u8]) {
    if bpp == 1 {
        fill_rows::<P8>(s, y0, rows)
    } else {
        fill_rows::<P16>(s, y0, rows)
    }
}

fn fill_rows<P: Px>(s: &Src, y0: usize, rows: &mut [u8]) {
    let stride = s.w * P::BYTES;
    if stride == 0 {
        return;
    }
    fn line(buf: &[u8], y: usize, stride: usize) -> &[u8] {
        &buf[y * stride..(y + 1) * stride]
    }
    for (i, out) in rows.chunks_exact_mut(stride).enumerate() {
        let y = y0 + i;
        if y % 2 == s.keep {
            out.copy_from_slice(line(s.cur, y, stride));
            continue;
        }
        macro_rules! at {
            ($buf:expr, $dy:expr) => {
                line($buf, mirror(y as isize + $dy, s.h), stride)
            };
        }
        let (c, e) = (at!(s.cur, -1), at!(s.cur, 1));
        match s.kernel {
            Kernel::Linear => {
                for x in 0..s.w {
                    P::wr(out, x, (P::rd(c, x) + P::rd(e, x) + 1) >> 1);
                }
            }
            Kernel::Cubic => {
                let (c3, e3) = (at!(s.cur, -3), at!(s.cur, 3));
                for x in 0..s.w {
                    let v = (5077 * (P::rd(c, x) + P::rd(e, x)) - 981 * (P::rd(c3, x) + P::rd(e3, x))) >> 13;
                    P::wr(out, x, v.clamp(0, s.max));
                }
            }
            Kernel::Bwdif => {
                let (c3, e3) = (at!(s.cur, -3), at!(s.cur, 3));
                let (pc, pe, nc, ne) = (at!(s.prev, -1), at!(s.prev, 1), at!(s.next, -1), at!(s.next, 1));
                let (p0, pm2, pp2, pm4, pp4) = (at!(s.prev2, 0), at!(s.prev2, -2), at!(s.prev2, 2), at!(s.prev2, -4), at!(s.prev2, 4));
                let (n0, nm2, np2, nm4, np4) = (at!(s.next2, 0), at!(s.next2, -2), at!(s.next2, 2), at!(s.next2, -4), at!(s.next2, 4));
                for x in 0..s.w {
                    let (cv, ev) = (P::rd(c, x), P::rd(e, x));
                    let (a0, b0) = (P::rd(p0, x), P::rd(n0, x));
                    let d = (a0 + b0) >> 1;
                    let td0 = (a0 - b0).abs();
                    let td1 = ((P::rd(pc, x) - cv).abs() + (P::rd(pe, x) - ev).abs()) >> 1;
                    let td2 = ((P::rd(nc, x) - cv).abs() + (P::rd(ne, x) - ev).abs()) >> 1;
                    let mut diff = (td0 >> 1).max(td1).max(td2);
                    if diff == 0 {
                        P::wr(out, x, d);
                        continue;
                    }
                    let b = (P::rd(pm2, x) + P::rd(nm2, x)) >> 1;
                    let f = (P::rd(pp2, x) + P::rd(np2, x)) >> 1;
                    let (dc, de) = (d - cv, d - ev);
                    let hi = de.max(dc).max((b - cv).min(f - ev));
                    let lo = de.min(dc).min((b - cv).max(f - ev));
                    diff = diff.max(lo).max(-hi);
                    let near = P::rd(c3, x) + P::rd(e3, x);
                    let interp = if (cv - ev).abs() > td0 {
                        let hf = 5570 * (a0 + b0) - 3801 * (P::rd(pm2, x) + P::rd(nm2, x) + P::rd(pp2, x) + P::rd(np2, x)) + 1016 * (P::rd(pm4, x) + P::rd(nm4, x) + P::rd(pp4, x) + P::rd(np4, x));
                        ((hf >> 2) + 4309 * (cv + ev) - 213 * near) >> 13
                    } else {
                        (5077 * (cv + ev) - 981 * near) >> 13
                    };
                    P::wr(out, x, interp.clamp(d - diff, d + diff).clamp(0, s.max));
                }
            }
        }
    }
}

/// The `deinterlace` filter.
pub(crate) struct DeinterlaceFilter {
    d: Deinterlacer,
    field: bool,
}

impl DeinterlaceFilter {
    pub(crate) fn new(opts: DeintOptions) -> Self {
        DeinterlaceFilter { d: Deinterlacer::new(opts), field: opts.mode == DeintMode::Field }
    }
}

impl Filter for DeinterlaceFilter {
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        match input {
            StreamParams::Video { width, height, pix_fmt, frame_rate } if self.field => {
                StreamParams::Video { width: *width, height: *height, pix_fmt: *pix_fmt, frame_rate: Rational::new(frame_rate.num * 2, frame_rate.den) }
            }
            other => other.clone(),
        }
    }

    fn time_scale(&self) -> i64 {
        self.d.time_scale()
    }

    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        self.process_interlaced(frame, FieldOrder::Unknown)
    }

    fn process_interlaced(&mut self, frame: Frame, order: FieldOrder) -> Result<Vec<Frame>> {
        match frame {
            Frame::Video(v) => Ok(self.d.push(v, order)?.into_iter().map(Frame::Video).collect()),
            other => Ok(vec![other]),
        }
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(self.d.flush()?.into_iter().map(Frame::Video).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};

    const W: usize = 64;
    const H: usize = 48;

    /// The picture at `time` (in field periods): a smooth vertical gradient, a bar 10 wide that moves 3 columns
    /// a field, and, at the right, a static texture that has detail on every line.
    fn scene(time: usize, x: usize, y: usize) -> i32 {
        let bar = 4 + 3 * time;
        if x >= 50 {
            80 + ((x * 7 + y * 13) % 64) as i32
        } else if x >= bar && x < bar + 10 {
            230
        } else {
            60 + y as i32
        }
    }

    /// A picture whose lines of parity `first` are the scene at time `2 * t` and the others at `2 * t + 1`
    /// (a frame of interlaced video, first field first), or, with `progressive`, all at that time.
    fn frame(fmt: PixelFormat, t: usize, first: usize, progressive: Option<usize>) -> VideoFrame {
        let layout = fmt.planes(W, H);
        let depth = fmt.bit_depth() as u32;
        let planes = layout
            .iter()
            .map(|&(pw, ph, bpp)| {
                let mut p = Vec::with_capacity(pw * ph * bpp);
                for y in 0..ph {
                    let time = progressive.unwrap_or(if y % 2 == first { 2 * t } else { 2 * t + 1 });
                    for x in 0..pw {
                        let v = (scene(time, x * W / pw, y) as u32) << (depth - 8);
                        if bpp == 1 {
                            p.push(v as u8);
                        } else {
                            p.extend_from_slice(&(v as u16).to_le_bytes());
                        }
                    }
                }
                p
            })
            .collect();
        VideoFrame { width: W as u32, height: H as u32, pix_fmt: fmt, pts: t as i64 * 100, planes }
    }

    fn sample(p: &[u8], bpp: usize, i: usize) -> i64 {
        if bpp == 1 { p[i] as i64 } else { u16::from_le_bytes([p[2 * i], p[2 * i + 1]]) as i64 }
    }

    /// The mean absolute second difference between lines, over the columns left of the static texture: what combing adds.
    fn comb(f: &VideoFrame, plane: usize) -> f64 {
        let (pw, ph, bpp) = f.pix_fmt.planes(W, H)[plane];
        let cols = 48 * pw / W;
        let (mut sum, mut n) = (0.0, 0);
        for y in 1..ph - 1 {
            for x in 0..cols {
                let at = |yy: usize| sample(&f.planes[plane], bpp, yy * pw + x);
                sum += (2 * at(y) - at(y - 1) - at(y + 1)).abs() as f64;
                n += 1;
            }
        }
        sum / n as f64 / (1 << (f.pix_fmt.bit_depth() - 8)) as f64
    }

    /// The mean absolute difference from the scene at `time`, over all of plane 0 (in 8-bit steps).
    fn error_against_scene(f: &VideoFrame, time: usize) -> f64 {
        let depth = f.pix_fmt.bit_depth() as u32;
        let (pw, ph, bpp) = f.pix_fmt.planes(W, H)[0];
        let mut sum = 0.0;
        for y in 0..ph {
            for x in 0..pw {
                let want = (scene(time, x, y) as i64) << (depth - 8);
                sum += (sample(&f.planes[0], bpp, y * pw + x) - want).abs() as f64;
            }
        }
        sum / (pw * ph) as f64 / (1 << (depth - 8)) as f64
    }

    fn run(d: &mut Deinterlacer, frames: Vec<(VideoFrame, FieldOrder)>) -> Vec<VideoFrame> {
        let mut out = Vec::new();
        for (f, o) in frames {
            out.extend(d.push(f, o).unwrap());
        }
        out.extend(d.flush().unwrap());
        out
    }

    fn clip(fmt: PixelFormat, n: usize, first: usize) -> Vec<(VideoFrame, FieldOrder)> {
        let order = if first == 0 { FieldOrder::TopFirst } else { FieldOrder::BottomFirst };
        (0..n).map(|t| (frame(fmt, t, first, None), order)).collect()
    }

    #[test]
    fn frame_mode_removes_combing_and_keeps_the_static_part() {
        for method in [DeintMethod::Bwdif, DeintMethod::Linear] {
            let input = clip(PixelFormat::Yuv420p, 6, 0);
            let before: Vec<f64> = input.iter().map(|(f, _)| comb(f, 0)).collect();
            let copy: Vec<VideoFrame> = input.iter().map(|(f, _)| f.clone()).collect();
            let out = run(&mut Deinterlacer::new(DeintOptions { method, ..Default::default() }), input);
            assert_eq!(out.len(), 6);
            for (i, o) in out.iter().enumerate() {
                assert_eq!(o.pts, i as i64 * 100, "frame mode keeps the timestamps");
                assert_eq!((o.width, o.height, o.pix_fmt), (W as u32, H as u32, PixelFormat::Yuv420p));
                assert!(comb(o, 0) < before[i] * 0.35, "{method:?} frame {i}: comb {} against {}", comb(o, 0), before[i]);
                // the first field passes through untouched
                for y in (0..H).step_by(2) {
                    assert_eq!(o.planes[0][y * W..(y + 1) * W], copy[i].planes[0][y * W..(y + 1) * W]);
                }
            }
            // the static texture is the source, line for line
            if method == DeintMethod::Bwdif {
                for i in 1..5 {
                    for y in 0..H {
                        assert_eq!(out[i].planes[0][y * W + 50..(y + 1) * W], copy[i].planes[0][y * W + 50..(y + 1) * W], "frame {i} line {y}");
                    }
                }
            }
        }
    }

    #[test]
    fn field_mode_gives_a_picture_per_field_at_the_right_time() {
        for first in [0usize, 1] {
            let input = clip(PixelFormat::Yuv420p, 6, first);
            let woven_error: f64 = (1..5).map(|t| error_against_scene(&input[t].0, 2 * t)).sum::<f64>() / 4.0;
            let opts = DeintOptions { mode: DeintMode::Field, parity: if first == 0 { DeintParity::TopFirst } else { DeintParity::BottomFirst }, ..Default::default() };
            let mut d = Deinterlacer::new(opts);
            assert_eq!(d.time_scale(), 2);
            let out = run(&mut d, input);
            assert_eq!(out.len(), 12, "twice as many pictures");
            let pts: Vec<i64> = out.iter().map(|f| f.pts).collect();
            assert!(pts.windows(2).all(|w| w[0] < w[1]), "timestamps rise: {pts:?}");
            // 100 apart in the input is 200 apart now, and the second field halfway
            assert_eq!(pts[..4], [0, 100, 200, 300]);
            // every output is the scene at its field's time, which the woven picture is not
            let mut err = 0.0;
            for (k, o) in out.iter().enumerate().take(10).skip(2) {
                err += error_against_scene(o, k);
            }
            err /= 8.0;
            assert!(err < 2.5 && err < woven_error * 0.6, "parity {first}: mean error {err} (woven {woven_error})");
        }
    }

    #[test]
    fn the_order_the_decoder_reports_picks_the_first_field() {
        // a bottom-first clip with parity auto and the order reported is as good as with parity bff
        let input = clip(PixelFormat::Yuv420p, 6, 1);
        let opts = DeintOptions { mode: DeintMode::Field, ..Default::default() };
        let out = run(&mut Deinterlacer::new(opts), input);
        let err: f64 = (2..10).map(|k| error_against_scene(&out[k], k)).sum::<f64>() / 8.0;
        assert!(err < 2.5, "mean error {err}");
        // and with the wrong parity forced the pictures are visibly wrong
        let input = clip(PixelFormat::Yuv420p, 6, 1);
        let out = run(&mut Deinterlacer::new(DeintOptions { mode: DeintMode::Field, parity: DeintParity::TopFirst, ..Default::default() }), input);
        let wrong: f64 = (2..10).map(|k| error_against_scene(&out[k], k)).sum::<f64>() / 8.0;
        assert!(wrong > err * 1.5, "wrong parity {wrong} against {err}");
    }

    #[test]
    fn flush_emits_the_last_pictures_and_a_lone_picture_works() {
        let mut d = Deinterlacer::new(DeintOptions { mode: DeintMode::Field, ..Default::default() });
        assert!(d.push(frame(PixelFormat::Yuv420p, 0, 0, None), FieldOrder::TopFirst).unwrap().is_empty());
        assert_eq!(d.push(frame(PixelFormat::Yuv420p, 1, 0, None), FieldOrder::TopFirst).unwrap().len(), 2);
        let last = d.flush().unwrap();
        assert_eq!(last.len(), 2);
        assert_eq!(last[0].pts, 200);
        assert_eq!(last[1].pts, 300, "the last picture's second field is a picture length after its first");
        assert!(d.flush().unwrap().is_empty(), "nothing left");
        let mut d = Deinterlacer::new(DeintOptions::default());
        d.push(frame(PixelFormat::Yuv420p, 0, 0, None), FieldOrder::Unknown).unwrap();
        assert_eq!(d.flush().unwrap().len(), 1);
    }

    #[test]
    fn high_bit_depth_422_444_and_gray_work() {
        for fmt in [PixelFormat::Yuv420p10, PixelFormat::Yuv422p, PixelFormat::Yuv422p12, PixelFormat::Yuv444p, PixelFormat::Yuv440p, PixelFormat::Gray8] {
            let input = clip(fmt, 5, 0);
            let before = comb(&input[2].0, 0);
            let chroma_before = if input[2].0.planes.len() == 3 { comb(&input[2].0, 1) } else { 0.0 };
            let out = run(&mut Deinterlacer::new(DeintOptions::default()), input);
            assert_eq!(out.len(), 5);
            assert_eq!(out[2].planes.len(), fmt.planes(W, H).len());
            assert!(comb(&out[2], 0) < before * 0.35, "{fmt:?}: comb {} against {before}", comb(&out[2], 0));
            if out[2].planes.len() == 3 {
                assert!(comb(&out[2], 1) < chroma_before * 0.5 + 0.01, "{fmt:?} chroma");
            }
            // samples stay inside the depth
            if fmt.bit_depth() > 8 {
                let max = (1u32 << fmt.bit_depth()) - 1;
                assert!(out[2].planes.iter().all(|p| p.as_chunks::<2>().0.iter().all(|b| u16::from_le_bytes(*b) as u32 <= max)));
            }
        }
    }

    #[test]
    fn only_flagged_pictures_are_changed_when_asked() {
        let opts = DeintOptions { select: DeintSelect::Interlaced, ..Default::default() };
        let progressive = frame(PixelFormat::Yuv420p, 0, 0, Some(3));
        let interlaced = frame(PixelFormat::Yuv420p, 1, 0, None);
        let out = run(&mut Deinterlacer::new(opts), vec![(progressive.clone(), FieldOrder::Progressive), (interlaced.clone(), FieldOrder::TopFirst), (progressive.clone(), FieldOrder::Unknown)]);
        assert_eq!(out[0].planes, progressive.planes);
        assert_ne!(out[1].planes, interlaced.planes);
        assert_eq!(out[2].planes, progressive.planes);
        // in field mode an untouched picture is still given twice, so the rate stays even
        let opts = DeintOptions { select: DeintSelect::Interlaced, mode: DeintMode::Field, ..Default::default() };
        let out = run(&mut Deinterlacer::new(opts), vec![(progressive.clone(), FieldOrder::Progressive), (interlaced, FieldOrder::BottomFirst)]);
        assert_eq!(out.len(), 4);
        assert_eq!((out[0].planes == progressive.planes, out[1].planes == progressive.planes), (true, true));
        assert_eq!((out[0].pts, out[1].pts, out[2].pts), (0, 100, 200));
    }

    #[test]
    fn one_picture_on_its_own_loses_the_combing() {
        let f = frame(PixelFormat::Yuv420p, 2, 0, None);
        let out = deinterlace_frame(&f, FieldOrder::TopFirst).unwrap();
        assert!(comb(&out, 0) < comb(&f, 0) * 0.35);
        assert_eq!(out.pts, f.pts);
        // bottom first keeps the odd lines
        let out = deinterlace_frame(&f, FieldOrder::BottomFirst).unwrap();
        assert_eq!(out.planes[0][W..2 * W], f.planes[0][W..2 * W]);
        let rgb = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Rgb24, pts: 0, planes: vec![vec![0; 12]] };
        assert!(deinterlace_frame(&rgb, FieldOrder::Unknown).is_err());
    }

    #[test]
    fn tiny_and_odd_sizes_do_not_panic() {
        for (w, h) in [(1u32, 1u32), (2, 2), (3, 5), (8, 7), (16, 3)] {
            for mode in [DeintMode::Frame, DeintMode::Field] {
                let planes = PixelFormat::Yuv420p.planes(w as usize, h as usize).iter().map(|&(pw, ph, _)| (0..pw * ph).map(|i| (i * 37 % 251) as u8).collect()).collect::<Vec<Vec<u8>>>();
                let f = VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes };
                let mut d = Deinterlacer::new(DeintOptions { mode, ..Default::default() });
                let out = run(&mut d, vec![(f.clone(), FieldOrder::TopFirst), (VideoFrame { pts: 1, ..f }, FieldOrder::TopFirst)]);
                assert_eq!(out.len(), if mode == DeintMode::Frame { 2 } else { 4 });
            }
        }
    }

    #[test]
    fn large_planes_split_across_threads_come_out_the_same_as_whole() {
        let (w, h) = (640usize, 480usize);
        let mk = |t: usize| {
            let mut y = vec![0u8; w * h];
            for r in 0..h {
                for c in 0..w {
                    y[r * w + c] = ((c * 3 + r * 5 + (r % 2) * t * 17 + (c * r) % 7) % 256) as u8;
                }
            }
            VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Gray8, pts: t as i64, planes: vec![y] }
        };
        let (a, b, c) = (mk(0), mk(1), mk(2));
        let threaded = render(&b, &a, &c, (&a, &b), 0, Kernel::Bwdif, 0);
        let mut whole = vec![0u8; w * h];
        let src = Src { w, h, keep: 0, max: 255, kernel: Kernel::Bwdif, cur: &b.planes[0], prev: &a.planes[0], next: &c.planes[0], prev2: &a.planes[0], next2: &b.planes[0] };
        fill(&src, 1, 0, &mut whole);
        assert!(threaded.planes[0] == whole);
    }

    fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn the_filter_takes_names_or_numbers_and_doubles_the_rate_in_field_mode() {
        use crate::build_filter;
        let params = StreamParams::Video { width: 720, height: 576, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let by_name = build_filter("deinterlace", &args(&[("mode", "field".into()), ("parity", "bff".into()), ("deint", "interlaced".into()), ("method", "linear".into())])).unwrap();
        let by_number = build_filter("deinterlace", &args(&[("mode", 1.into()), ("parity", 2.into()), ("deint", 1.into()), ("method", 1.into())])).unwrap();
        assert_eq!(by_name.output_params(&params), by_number.output_params(&params));
        assert_eq!(by_name.time_scale(), 2);
        assert_eq!(by_name.output_params(&params), StreamParams::Video { width: 720, height: 576, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(50, 1) });
        let plain = build_filter("deinterlace", &Map::new()).unwrap();
        assert_eq!(plain.output_params(&params), params);
        assert_eq!(plain.time_scale(), 1);
        let e = build_filter("deinterlace", &args(&[("mode", "both".into())])).err().unwrap();
        assert_eq!(e.alternatives, vec!["frame", "field"]);
        assert!(build_filter("deinterlace", &args(&[("parity", 3.into())])).is_err());
        assert!(build_filter("deinterlace", &args(&[("parity", 0.5.into())])).is_err());
    }

    #[test]
    fn the_filter_flushes_and_passes_audio_and_the_decoder_order_through() {
        let mut f = DeinterlaceFilter::new(DeintOptions { select: DeintSelect::Interlaced, ..Default::default() });
        let a = Frame::Audio(ironav_core::AudioFrame { sample_rate: 8000, channels: 1, pts: 0, samples: vec![0.5] });
        assert_eq!(f.process(a).unwrap().len(), 1);
        let v = frame(PixelFormat::Yuv420p, 0, 0, None);
        assert!(f.process_interlaced(Frame::Video(v.clone()), FieldOrder::TopFirst).unwrap().is_empty());
        let Frame::Video(out) = f.flush().unwrap().remove(0) else { panic!() };
        assert_ne!(out.planes, v.planes, "flagged as interlaced, so changed");
        // without the flag (process, no order) a select=interlaced filter leaves the picture alone
        let mut f = DeinterlaceFilter::new(DeintOptions { select: DeintSelect::Interlaced, ..Default::default() });
        f.process(Frame::Video(v.clone())).unwrap();
        let Frame::Video(out) = f.flush().unwrap().remove(0) else { panic!() };
        assert_eq!(out.planes, v.planes);
    }
}
