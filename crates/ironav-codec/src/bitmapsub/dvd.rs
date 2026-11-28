//! DVD subpictures (VobSub), as `dvdsubdec.c` of FFmpeg reads them.
//!
//! A subpicture packet (SPU) is a size, the offset of its control sequences, the two interlaced
//! fields of run-length coded pixels (2 bits each, four colours) and the control sequences. A
//! sequence has a date (in 1024/90000 s after the packet's time) and commands: start, stop, the four
//! colours' indices into the 16-colour palette, their alpha (0 to 15), the display area and the
//! offsets of the two fields. The HD forms (4-byte offsets, 8-bit pixels with a 256-colour palette)
//! are read too. The 16 colours are the track's `.idx` `palette:` line when it has one; a DVD keeps
//! them in its IFO file, which IronAV does not read, so without one FFmpeg's guess is used: white,
//! with the opaque colours' brightness spread evenly from dark to full. A packet that holds only
//! transparent pixels shows nothing, and the picture is cropped to its visible pixels. A menu
//! (forced) subpicture is left as it is.

use super::{yuv_to_rgb, SubRect, SubtitleBitmap, MAX_DIM};
use std::sync::Arc;

/// How long an event with no stop time and no packet duration stays, in seconds.
const DEFAULT_HOLD: f64 = 5.0;

/// Decodes a stream of DVD subpictures, one whole SPU per packet.
pub struct DvdSubDecoder {
    tick: f64,
    canvas: (u32, u32),
    palette: Option<[u32; 16]>,
    // these persist from one packet to the next, as in FFmpeg
    colormap: [u8; 4],
    alpha: [u8; 256],
}

impl DvdSubDecoder {
    /// A decoder for a stream whose ticks are `tick` seconds long. `extradata` is the track's `.idx`
    /// text (`size: 720x480`, `palette: 000000, ...`), or empty; the canvas is 720x480 without a size.
    pub fn new(extradata: &[u8], tick: f64) -> Self {
        let mut d = DvdSubDecoder { tick, canvas: (720, 480), palette: None, colormap: [0; 4], alpha: [0; 256] };
        for line in String::from_utf8_lossy(extradata).lines() {
            let line = line.trim();
            if let Some(p) = line.strip_prefix("palette:") {
                let mut colors = [0u32; 16];
                for (c, v) in colors.iter_mut().zip(p.split([',', ' ', '\t']).filter(|s| !s.is_empty())) {
                    *c = u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0) & 0xFF_FFFF;
                }
                d.palette = Some(colors);
            } else if let Some(s) = line.strip_prefix("size:") {
                let s = s.trim();
                if let Some((w, h)) = s.split_once(['x', 'X']) {
                    if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                        if (1..=MAX_DIM).contains(&w) && (1..=MAX_DIM).contains(&h) {
                            d.canvas = (w, h);
                        }
                    }
                }
            }
        }
        d
    }

    /// One SPU at `pts` ticks, shown for `duration` ticks unless its own stop time says otherwise (0
    /// if unknown). Returns the event, if the packet shows anything.
    pub fn push(&mut self, pts: i64, duration: i64, data: &[u8]) -> Vec<SubtitleBitmap> {
        let Some(spu) = self.parse(data) else { return Vec::new() };
        let base = pts as f64 * self.tick;
        let start = base + spu.start.unwrap_or(0.0);
        let end = match spu.end {
            Some(e) => base + e,
            None if duration > 0 => base + duration as f64 * self.tick,
            None => start + DEFAULT_HOLD,
        };
        vec![SubtitleBitmap { start, end: end.max(start), canvas_w: self.canvas.0, canvas_h: self.canvas.1, rects: vec![spu.rect] }]
    }

    /// The 4 colours of a 2-bit subpicture as RGBA, from the palette or FFmpeg's guess.
    fn colors(&self) -> [[u8; 4]; 4] {
        let (map, alpha) = (self.colormap, self.alpha);
        let a = |i: usize| (alpha[i] as u32 * 17).min(255) as u8;
        let mut out = [[0u8; 4]; 4];
        if let Some(p) = &self.palette {
            for i in 0..4 {
                let c = p[map[i] as usize];
                out[i] = [(c >> 16) as u8, (c >> 8) as u8, c as u8, a(i)];
            }
            return out;
        }
        const LEVELS: [[u8; 4]; 4] = [[0xFF, 0, 0, 0], [0x00, 0xFF, 0, 0], [0x00, 0x80, 0xFF, 0], [0x00, 0x55, 0xAA, 0xFF]];
        let mut seen = [false; 16];
        let mut opaque = 0;
        for i in 0..4 {
            if alpha[i] != 0 && !seen[map[i] as usize] {
                seen[map[i] as usize] = true;
                opaque += 1;
            }
        }
        if opaque == 0 {
            return out;
        }
        let mut given: [Option<usize>; 16] = [None; 16];
        let mut j = 0;
        for i in 0..4 {
            if alpha[i] == 0 {
                continue;
            }
            match given[map[i] as usize] {
                None => {
                    let level = LEVELS[opaque - 1][j];
                    out[i] = [level, level, level, a(i)];
                    given[map[i] as usize] = Some(i);
                    j += 1;
                }
                Some(first) => out[i] = [out[first][0], out[first][1], out[first][2], a(i)],
            }
        }
        out
    }

    fn parse(&mut self, buf: &[u8]) -> Option<Spu> {
        if buf.len() < 10 {
            return None;
        }
        let be16 = |at: usize| buf.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
        let be32 = |at: usize| buf.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
        // an SPU whose first two bytes are 0 is the HD form, with 4-byte offsets
        let big = be16(0)? == 0;
        let off = |at: usize| if big { be32(at) } else { be16(at) };
        let offset_size = if big { 4 } else { 2 };
        let mut cmd_pos = off(if big { 6 } else { 2 })?;
        if cmd_pos > buf.len().checked_sub(2 + offset_size)? {
            return None;
        }
        let (mut start, mut end, mut is_menu, mut is_8bit) = (None, None, false, false);
        let mut yuv_palette: Option<&[u8]> = None;
        let mut found: Option<(SubRect, bool)> = None;
        while cmd_pos > 0 && cmd_pos < buf.len() - 2 - offset_size {
            let date = be16(cmd_pos)?;
            let next_cmd_pos = off(cmd_pos + 2)?;
            let mut pos = cmd_pos + 2 + offset_size;
            let (mut offset1, mut offset2) = (None, None);
            let (mut x1, mut y1, mut x2, mut y2) = (0i32, 0i32, 0i32, 0i32);
            while pos < buf.len() {
                let cmd = buf[pos];
                pos += 1;
                let left = buf.len() - pos;
                match cmd {
                    0x00 => is_menu = true,
                    0x01 => start = Some(((date << 10) / 90) as f64 / 1000.0),
                    0x02 => end = Some(((date << 10) / 90) as f64 / 1000.0),
                    0x03 => {
                        if left < 2 {
                            return None;
                        }
                        self.colormap = [buf[pos + 1] & 15, buf[pos + 1] >> 4, buf[pos] & 15, buf[pos] >> 4];
                        pos += 2;
                    }
                    0x04 => {
                        if left < 2 {
                            return None;
                        }
                        self.alpha[..4].copy_from_slice(&[buf[pos + 1] & 15, buf[pos + 1] >> 4, buf[pos] & 15, buf[pos] >> 4]);
                        pos += 2;
                    }
                    0x05 | 0x85 => {
                        if left < 6 {
                            return None;
                        }
                        let b = &buf[pos..pos + 6];
                        x1 = ((b[0] as i32) << 4) | (b[1] as i32 >> 4);
                        x2 = (((b[1] & 15) as i32) << 8) | b[2] as i32;
                        y1 = ((b[3] as i32) << 4) | (b[4] as i32 >> 4);
                        y2 = (((b[4] & 15) as i32) << 8) | b[5] as i32;
                        is_8bit |= cmd & 0x80 != 0;
                        pos += 6;
                    }
                    0x06 => {
                        if left < 4 {
                            return None;
                        }
                        (offset1, offset2) = (be16(pos), be16(pos + 2));
                        pos += 4;
                    }
                    0x86 => {
                        if left < 8 {
                            return None;
                        }
                        (offset1, offset2) = (be32(pos), be32(pos + 4));
                        pos += 8;
                    }
                    0x83 => {
                        if left < 768 {
                            return None;
                        }
                        yuv_palette = Some(&buf[pos..pos + 768]);
                        pos += 768;
                    }
                    0x84 => {
                        if left < 256 {
                            return None;
                        }
                        for i in 0..256 {
                            self.alpha[i] = 0xFF - buf[pos + i];
                        }
                        pos += 256;
                    }
                    _ => break,
                }
            }
            if offset1.is_some_and(|o| o >= buf.len()) || offset2.is_some_and(|o| o >= buf.len()) {
                return None;
            }
            if let (Some(o1), Some(o2)) = (offset1, offset2) {
                let w = (x2 - x1 + 1).max(0) as usize;
                let h = (y2 - y1 + 1).max(0) as usize;
                if w > 0 && h > 1 {
                    if w > MAX_DIM as usize || h > MAX_DIM as usize {
                        return None;
                    }
                    let mut pixels = vec![0u8; w * h];
                    decode_field(&mut pixels, 0, w, h.div_ceil(2), buf, o1, is_8bit)?;
                    decode_field(&mut pixels, 1, w, h / 2, buf, o2, is_8bit)?;
                    let palette: Vec<[u8; 4]> = if is_8bit {
                        let yuv = yuv_palette?;
                        (0..256)
                            .map(|i| {
                                let [r, g, b] = yuv_to_rgb(yuv[i * 3], yuv[i * 3 + 2], yuv[i * 3 + 1], false);
                                [r, g, b, self.alpha[i]]
                            })
                            .collect()
                    } else {
                        self.colors().to_vec()
                    };
                    let rect = SubRect { x: x1, y: y1, w: w as u32, h: h as u32, forced: is_menu, pixels, palette: Arc::from(palette) };
                    found = Some((rect, is_menu));
                }
            }
            if next_cmd_pos <= cmd_pos {
                break;
            }
            cmd_pos = next_cmd_pos;
        }
        let (mut rect, menu) = found?;
        if !menu {
            rect = crop_to_visible(rect)?;
        }
        Some(Spu { start, end, rect })
    }
}

struct Spu {
    start: Option<f64>,
    end: Option<f64>,
    rect: SubRect,
}

/// Cuts the picture to the box of its non-transparent pixels, or `None` if there are none.
fn crop_to_visible(r: SubRect) -> Option<SubRect> {
    let (w, h) = (r.w as usize, r.h as usize);
    let visible = |i: u8| r.palette.get(i as usize).is_some_and(|c| c[3] != 0);
    let (mut x1, mut x2, mut y1, mut y2) = (usize::MAX, 0, usize::MAX, 0);
    for y in 0..h {
        for x in 0..w {
            if visible(r.pixels[y * w + x]) {
                (x1, x2, y1, y2) = (x1.min(x), x2.max(x), y1.min(y), y2.max(y));
            }
        }
    }
    if x1 == usize::MAX {
        return None;
    }
    let (nw, nh) = (x2 - x1 + 1, y2 - y1 + 1);
    let mut pixels = Vec::with_capacity(nw * nh);
    for y in y1..=y2 {
        pixels.extend_from_slice(&r.pixels[y * w + x1..y * w + x1 + nw]);
    }
    Some(SubRect { x: r.x + x1 as i32, y: r.y + y1 as i32, w: nw as u32, h: nh as u32, pixels, ..r })
}

/// MSB-first bits; reading past the end gives zeros and moves the position on, so it can be seen.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            let bit = self.data.get(self.pos / 8).map_or(0, |b| (b >> (7 - self.pos % 8)) & 1);
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        v
    }
    fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

/// A run: its length (`usize::MAX` for the rest of the line) and colour.
fn run_2bit(b: &mut Bits) -> (usize, u8) {
    let (mut v, mut t) = (0u32, 1u32);
    while v < t && t <= 0x40 {
        v = (v << 4) | b.get(4);
        t <<= 2;
    }
    let color = (v & 3) as u8;
    if v < 4 {
        (usize::MAX, color)
    } else {
        ((v >> 2) as usize, color)
    }
}

fn run_8bit(b: &mut Bits) -> (usize, u8) {
    let has_run = b.get(1) != 0;
    let wide = b.get(1) != 0;
    let color = b.get(if wide { 8 } else { 2 }) as u8;
    if !has_run {
        return (1, color);
    }
    if b.get(1) != 0 {
        match b.get(7) {
            0 => (usize::MAX, color),
            n => (n as usize + 9, color),
        }
    } else {
        (b.get(3) as usize + 2, color)
    }
}

/// Decodes one field (`lines` lines, every second line of the bitmap from line `first`) from `start`.
/// A run that crosses the end of its line is an error; nothing is written past the bitmap.
fn decode_field(bitmap: &mut [u8], first: usize, w: usize, lines: usize, buf: &[u8], start: usize, is_8bit: bool) -> Option<()> {
    if start >= buf.len() || w == 0 || lines == 0 {
        return None;
    }
    let mut bits = Bits { data: &buf[start..], pos: 0 };
    let bit_len = bits.data.len() * 8;
    let (mut x, mut y) = (0usize, 0usize);
    loop {
        if bits.pos > bit_len {
            return None;
        }
        let (len, color) = if is_8bit { run_8bit(&mut bits) } else { run_2bit(&mut bits) };
        if len != usize::MAX && len > w - x {
            return None;
        }
        let len = len.min(w - x);
        let row = (2 * y + first) * w;
        bitmap.get_mut(row + x..row + x + len)?.fill(color);
        x += len;
        if x >= w {
            y += 1;
            if y >= lines {
                return Some(());
            }
            x = 0;
            bits.align();
        }
    }
}
