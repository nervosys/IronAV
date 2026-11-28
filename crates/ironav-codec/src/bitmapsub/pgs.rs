//! Blu-ray PGS (Presentation Graphic Stream) subtitles, as `pgssubdec.c` of FFmpeg reads them.
//!
//! A display set is a run of segments: PCS (0x16, the composition: which objects go where, and
//! whether this starts a new epoch), WDS (0x17, windows, not needed), PDS (0x14, a palette of
//! Y'CbCr and alpha entries), ODS (0x15, a run-length coded object, possibly split over several
//! segments) and END (0x80). A display set with no objects clears the screen, which is also where
//! the event before it ends. A palette is converted with BT.601 for a canvas up to 576 lines high
//! and BT.709 above, as FFmpeg does. The composition's cropping is applied, which FFmpeg does not do.

use super::{yuv_to_rgb, Be, SubRect, SubtitleBitmap, MAX_DIM};
use std::sync::Arc;

const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const END: u8 = 0x80;

/// Palettes and objects an epoch holds, at most (the limits of the format).
const MAX_PALETTES: usize = 8;
const MAX_OBJECTS: usize = 64;
/// Objects a composition shows.
const MAX_REFS: usize = 2;
/// How long the last event stays when the stream ends without clearing it, in seconds.
const LAST_HOLD: f64 = 10.0;

struct Object {
    id: u16,
    w: usize,
    h: usize,
    rle: Vec<u8>,
    /// Bytes of the RLE data still to come in later segments.
    remaining: usize,
}

struct ObjectRef {
    id: u16,
    forced: bool,
    x: i32,
    y: i32,
    crop: Option<[usize; 4]>,
}

struct Composition {
    pts: i64,
    palette: u8,
    refs: Vec<ObjectRef>,
}

/// Decodes a PGS stream: feed it packets (one or more segments each) in order.
pub struct PgsDecoder {
    tick: f64,
    canvas: (u32, u32),
    objects: Vec<Object>,
    palettes: Vec<(u8, Vec<[u8; 4]>)>,
    composition: Option<Composition>,
    /// The event on screen, its end not known until the next display set.
    open: Option<SubtitleBitmap>,
}

impl PgsDecoder {
    /// A decoder for a stream whose ticks are `tick` seconds long.
    pub fn new(tick: f64) -> Self {
        PgsDecoder { tick, canvas: (0, 0), objects: Vec::new(), palettes: Vec::new(), composition: None, open: None }
    }

    /// One packet at `pts` ticks. Returns the events that ended with it (the one on screen before it).
    pub fn push(&mut self, pts: i64, data: &[u8]) -> Vec<SubtitleBitmap> {
        let mut done = Vec::new();
        let mut at = Be(data);
        while let (Some(kind), Some(len)) = (at.u8(), at.u16()) {
            let len = len as usize;
            if kind != END && len > at.len() {
                break;
            }
            let body = &at.0[..len.min(at.len())];
            match kind {
                PDS => self.palette(body),
                ODS => self.object(body),
                PCS => self.presentation(pts, body),
                WDS => {}
                END => self.display(&mut done),
                _ => {}
            }
            if at.skip(len).is_none() {
                break;
            }
        }
        done
    }

    /// The end of the stream: the event still on screen, held for a few seconds.
    pub fn finish(&mut self) -> Vec<SubtitleBitmap> {
        let last = self.open.take().map(|mut e| {
            e.end = e.start + LAST_HOLD;
            e
        });
        last.into_iter().collect()
    }

    fn flush(&mut self) {
        self.objects.clear();
        self.palettes.clear();
    }

    fn palette(&mut self, body: &[u8]) {
        let mut b = Be(body);
        let (Some(id), Some(_version)) = (b.u8(), b.u8()) else { return };
        let bt709 = self.canvas.1 == 0 || self.canvas.1 > 576;
        let slot = match self.palettes.iter().position(|p| p.0 == id) {
            Some(i) => i,
            None if self.palettes.len() < MAX_PALETTES => {
                self.palettes.push((id, vec![[0; 4]; 256]));
                self.palettes.len() - 1
            }
            None => return,
        };
        for &[index, y, cr, cb, alpha] in b.0.as_chunks::<5>().0 {
            let [r, g, bl] = yuv_to_rgb(y, cb, cr, bt709);
            self.palettes[slot].1[index as usize] = [r, g, bl, alpha];
        }
    }

    fn object(&mut self, body: &[u8]) {
        let mut b = Be(body);
        let (Some(id), Some(_version), Some(sequence)) = (b.u16(), b.u8(), b.u8()) else { return };
        if sequence & 0x80 == 0 {
            // more of the RLE data of an object that was started
            if let Some(o) = self.objects.iter_mut().find(|o| o.id == id) {
                if b.len() <= o.remaining {
                    o.rle.extend_from_slice(b.0);
                    o.remaining -= b.len();
                }
            }
            return;
        }
        let (Some(total), Some(w), Some(h)) = (b.u24(), b.u16(), b.u16()) else { return };
        let Some(rle_len) = (total as usize).checked_sub(4) else { return };
        if b.len() == 0 || b.len() > rle_len {
            return;
        }
        let (w, h) = (w as u32, h as u32);
        if w == 0 || h == 0 || w > self.canvas.0 || h > self.canvas.1 || w > MAX_DIM || h > MAX_DIM {
            return;
        }
        let object = Object { id, w: w as usize, h: h as usize, rle: b.0.to_vec(), remaining: rle_len - b.len() };
        if let Some(i) = self.objects.iter().position(|o| o.id == id) {
            self.objects[i] = object;
        } else if self.objects.len() < MAX_OBJECTS {
            self.objects.push(object);
        }
    }

    fn presentation(&mut self, pts: i64, body: &[u8]) {
        self.composition = None;
        let mut b = Be(body);
        let (Some(w), Some(h), Some(_rate), Some(_number), Some(state), Some(_update), Some(palette), Some(count)) =
            (b.u16(), b.u16(), b.u8(), b.u16(), b.u8(), b.u8(), b.u8(), b.u8())
        else {
            return;
        };
        let (w, h) = (w as u32, h as u32);
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return;
        }
        self.canvas = (w, h);
        if state >> 6 != 0 {
            self.flush();
        }
        let mut refs = Vec::new();
        for _ in 0..(count as usize).min(MAX_REFS) {
            let (Some(id), Some(_window), Some(flags), Some(x), Some(y)) = (b.u16(), b.u8(), b.u8(), b.u16(), b.u16()) else { break };
            let crop = if flags & 0x80 != 0 {
                let (Some(cx), Some(cy), Some(cw), Some(ch)) = (b.u16(), b.u16(), b.u16(), b.u16()) else { break };
                Some([cx as usize, cy as usize, cw as usize, ch as usize])
            } else {
                None
            };
            // an object placed outside the picture goes to the corner, as FFmpeg does
            let (x, y) = if x as u32 > w || y as u32 > h { (0, 0) } else { (x as i32, y as i32) };
            refs.push(ObjectRef { id, forced: flags & 0x40 != 0, x, y, crop });
        }
        self.composition = Some(Composition { pts, palette, refs });
    }

    /// The END segment: the composition takes effect.
    fn display(&mut self, done: &mut Vec<SubtitleBitmap>) {
        let Some(c) = self.composition.take() else { return };
        let (cw, ch) = self.canvas;
        let start = c.pts as f64 * self.tick;
        let mut rects = Vec::new();
        if !c.refs.is_empty() {
            let Some((_, entries)) = self.palettes.iter().find(|p| p.0 == c.palette) else { return };
            let palette: Arc<[[u8; 4]]> = entries.clone().into();
            for r in &c.refs {
                let Some(o) = self.objects.iter().find(|o| o.id == r.id) else { continue };
                let Some(mut pixels) = decode_rle(&o.rle, o.w, o.h) else { continue };
                let (mut x, mut y, mut w, mut h) = (r.x, r.y, o.w, o.h);
                if let Some([cx, cy, cw, chh]) = r.crop {
                    let (cw, chh) = (cw.min(o.w.saturating_sub(cx)), chh.min(o.h.saturating_sub(cy)));
                    if cw == 0 || chh == 0 {
                        continue;
                    }
                    let mut cropped = Vec::with_capacity(cw * chh);
                    for row in cy..cy + chh {
                        cropped.extend_from_slice(&pixels[row * o.w + cx..row * o.w + cx + cw]);
                    }
                    (pixels, w, h) = (cropped, cw, chh);
                    (x, y) = (x.saturating_add(cx as i32), y.saturating_add(cy as i32));
                }
                rects.push(SubRect { x, y, w: w as u32, h: h as u32, forced: r.forced, pixels, palette: palette.clone() });
            }
        }
        if let Some(mut previous) = self.open.take() {
            previous.end = start.max(previous.start);
            done.push(previous);
        }
        if !rects.is_empty() {
            self.open = Some(SubtitleBitmap { start, end: start, canvas_w: cw, canvas_h: ch, rects });
        }
    }
}

/// The run-length coding of PGS objects: a byte is one pixel; a zero byte starts a run (a flag byte
/// with the run's length in 6 bits, or 14 with 0x40, and a colour byte if 0x80 is set, colour 0
/// if not), and a run of length 0 ends the line. Nothing is written past the bitmap.
fn decode_rle(buf: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let total = w * h;
    let mut pixels = vec![0u8; total];
    let (mut count, mut lines, mut i) = (0usize, 0usize, 0usize);
    while i < buf.len() && lines < h {
        let mut color = buf[i];
        i += 1;
        let mut run = 1usize;
        if color == 0 {
            let flags = *buf.get(i)?;
            i += 1;
            run = (flags & 0x3F) as usize;
            if flags & 0x40 != 0 {
                run = (run << 8) + *buf.get(i)? as usize;
                i += 1;
            }
            color = if flags & 0x80 != 0 {
                i += 1;
                *buf.get(i - 1)?
            } else {
                0
            };
        }
        if run > 0 && count + run <= total {
            pixels[count..count + run].fill(color);
            count += run;
        } else if run == 0 {
            lines += 1;
        }
    }
    (count >= total).then_some(pixels)
}
