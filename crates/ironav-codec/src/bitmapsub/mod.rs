//! Bitmap subtitles: Blu-ray PGS (`hdmv_pgs_subtitle`) and DVD VobSub (`dvd_subtitle`), decoded to
//! paletted bitmaps with their display times. Nothing here reads text from the pictures (no OCR).
//!
//! A decoder is fed a stream's packets in order and hands back events: the pictures to show, from
//! when to when, in seconds on the stream's clock, on a canvas of the video's size. Colours follow
//! FFmpeg's decoders (`pgssubdec.c`, `dvdsubdec.c`). Malformed data is dropped, never a panic, and
//! sizes are capped at [`MAX_DIM`] on a side.

mod dvd;
mod pgs;
#[doc(hidden)]
pub mod testing;
#[cfg(test)]
mod tests;

pub use dvd::DvdSubDecoder;
pub use pgs::PgsDecoder;

use ironav_core::CodecId;
use std::sync::Arc;

/// The largest canvas or bitmap side taken.
pub const MAX_DIM: u32 = 4096;

/// One picture of a subtitle event: a paletted bitmap at a position on the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Shown even when only forced subtitles are wanted.
    pub forced: bool,
    /// `w * h` palette indices, row by row.
    pub pixels: Vec<u8>,
    /// Red, green, blue and alpha (255 is opaque, not premultiplied) of each index.
    pub palette: Arc<[[u8; 4]]>,
}

impl SubRect {
    /// The picture as straight (not premultiplied) RGBA bytes, `w * h * 4` of them.
    pub fn rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for &i in &self.pixels {
            out.extend_from_slice(&self.palette.get(i as usize).copied().unwrap_or([0; 4]));
        }
        out
    }
}

/// What to show from `start` to `end` (seconds): pictures placed on a `canvas_w` x `canvas_h` canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleBitmap {
    pub start: f64,
    pub end: f64,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub rects: Vec<SubRect>,
}

/// A decoder for either kind of bitmap subtitle.
pub enum BitmapSubDecoder {
    Pgs(PgsDecoder),
    Dvd(DvdSubDecoder),
}

impl BitmapSubDecoder {
    /// A decoder for `codec` (`HdmvPgs` or `DvdSub`), or `None` for any other. `extradata` is the
    /// track's private data (the `.idx` text for VobSub) and `tick` the stream's seconds per tick.
    pub fn new(codec: CodecId, extradata: &[u8], tick: f64) -> Option<Self> {
        match codec {
            CodecId::HdmvPgs => Some(Self::Pgs(PgsDecoder::new(tick))),
            CodecId::DvdSub => Some(Self::Dvd(DvdSubDecoder::new(extradata, tick))),
            _ => None,
        }
    }

    /// One packet: its time in ticks, its duration in ticks (0 if unknown) and its bytes. Returns the
    /// events that are complete.
    pub fn push(&mut self, pts: i64, duration: i64, data: &[u8]) -> Vec<SubtitleBitmap> {
        match self {
            Self::Pgs(d) => d.push(pts, data),
            Self::Dvd(d) => d.push(pts, duration, data),
        }
    }

    /// The end of the stream: the event still open, if any.
    pub fn finish(&mut self) -> Vec<SubtitleBitmap> {
        match self {
            Self::Pgs(d) => d.finish(),
            Self::Dvd(_) => Vec::new(),
        }
    }
}

/// Converts Y'CbCr (video range) to RGB with FFmpeg's fixed-point arithmetic.
pub(crate) fn yuv_to_rgb(y: u8, cb: u8, cr: u8, bt709: bool) -> [u8; 3] {
    fn fix(x: f64) -> i32 {
        (x * 1024.0 + 0.5) as i32
    }
    let (cb, cr) = (cb as i32 - 128, cr as i32 - 128);
    let k = 255.0 / 224.0;
    let (r_add, g_add, b_add) = if bt709 {
        (512 + fix(1.5747 * k) * cr, 512 - fix(0.1873 * k) * cb - fix(0.4682 * k) * cr, 512 + fix(1.8556 * k) * cb)
    } else {
        (fix(1.402 * k) * cr + 512, -fix(0.34414 * k) * cb - fix(0.71414 * k) * cr + 512, fix(1.772 * k) * cb + 512)
    };
    let y = (y as i32 - 16) * fix(255.0 / 219.0);
    let c = |v: i32| (v >> 10).clamp(0, 255) as u8;
    [c(y + r_add), c(y + g_add), c(y + b_add)]
}

/// A big-endian reader that never reads past the end.
pub(crate) struct Be<'a>(pub &'a [u8]);

impl Be<'_> {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    pub fn u24(&mut self) -> Option<u32> {
        Some(((self.u8()? as u32) << 16) | self.u16()? as u32)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.0 = self.0.get(n..)?;
        Some(())
    }
}
