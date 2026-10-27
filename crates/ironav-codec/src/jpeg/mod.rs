//! JPEG (ITU T.81, Huffman, 8-bit): a decoder for gray, 4:2:0, 4:2:2 and 4:4:4 images, sequential
//! or progressive, with restart intervals, and a baseline encoder for the same layouts. Arithmetic-coded, 12-bit and lossless files are refused by name.

mod tables;

use ironav_core::{Decoder, Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};
use tables::*;

/// Natural (row-major) index of the k-th coefficient in zigzag order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31,
    39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const STD_LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78,
    87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const STD_CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

fn unsupported(msg: String) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, msg)
}

// ---- transforms (the accurate integer forms of the IJG library) -------------------------------

const CONST_BITS: i32 = 13;
const PASS1_BITS: i32 = 2;
const F_0_298: i32 = 2446;
const F_0_390: i32 = 3196;
const F_0_541: i32 = 4433;
const F_0_765: i32 = 6270;
const F_0_899: i32 = 7373;
const F_1_175: i32 = 9633;
const F_1_501: i32 = 12299;
const F_1_847: i32 = 15137;
const F_1_961: i32 = 16069;
const F_2_053: i32 = 16819;
const F_2_562: i32 = 20995;
const F_3_072: i32 = 25172;

fn descale(x: i32, n: i32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

/// Inverse DCT of dequantised coefficients (natural order) to 8x8 samples, level-shifted and clamped.
fn idct(c: &[i32; 64], out: &mut [u8; 64]) {
    let mut ws = [0i64; 64];
    for col in 0..8 {
        let g = |r: usize| c[r * 8 + col].clamp(-4096, 4095) as i64;
        let z1 = (g(2) + g(6)) * (F_0_541 as i64);
        let t2 = z1 + g(6) * -(F_1_847 as i64);
        let t3 = z1 + g(2) * (F_0_765 as i64);
        let t0 = (g(0) + g(4)) << CONST_BITS as i64;
        let t1 = (g(0) - g(4)) << CONST_BITS as i64;
        let (t10, t13, t11, t12) = (t0 + t3, t0 - t3, t1 + t2, t1 - t2);
        let (mut a0, mut a1, mut a2, mut a3) = (g(7), g(5), g(3), g(1));
        let (z1, z2, z3, z4) = (a0 + a3, a1 + a2, a0 + a2, a1 + a3);
        let z5 = (z3 + z4) * (F_1_175 as i64);
        a0 *= (F_0_298 as i64);
        a1 *= (F_2_053 as i64);
        a2 *= (F_3_072 as i64);
        a3 *= (F_1_501 as i64);
        let (z1, z2, z3, z4) = (z1 * -(F_0_899 as i64), z2 * -(F_2_562 as i64), z3 * -(F_1_961 as i64) + z5, z4 * -(F_0_390 as i64) + z5);
        a0 += z1 + z3;
        a1 += z2 + z4;
        a2 += z2 + z3;
        a3 += z1 + z4;
        let s = CONST_BITS - PASS1_BITS;
        let descale = |x: i64, n: i32| (x + (1i64 << (n - 1))) >> n;
        ws[col] = descale(t10 + a3, s);
        ws[56 + col] = descale(t10 - a3, s);
        ws[8 + col] = descale(t11 + a2, s);
        ws[48 + col] = descale(t11 - a2, s);
        ws[16 + col] = descale(t12 + a1, s);
        ws[40 + col] = descale(t12 - a1, s);
        ws[24 + col] = descale(t13 + a0, s);
        ws[32 + col] = descale(t13 - a0, s);
    }
    for row in 0..8 {
        let g = |c: usize| ws[row * 8 + c];
        let z1 = (g(2) + g(6)) * (F_0_541 as i64);
        let t2 = z1 + g(6) * -(F_1_847 as i64);
        let t3 = z1 + g(2) * (F_0_765 as i64);
        let t0 = (g(0) + g(4)) << CONST_BITS as i64;
        let t1 = (g(0) - g(4)) << CONST_BITS as i64;
        let (t10, t13, t11, t12) = (t0 + t3, t0 - t3, t1 + t2, t1 - t2);
        let (mut a0, mut a1, mut a2, mut a3) = (g(7), g(5), g(3), g(1));
        let (z1, z2, z3, z4) = (a0 + a3, a1 + a2, a0 + a2, a1 + a3);
        let z5 = (z3 + z4) * (F_1_175 as i64);
        a0 *= (F_0_298 as i64);
        a1 *= (F_2_053 as i64);
        a2 *= (F_3_072 as i64);
        a3 *= (F_1_501 as i64);
        let (z1, z2, z3, z4) = (z1 * -(F_0_899 as i64), z2 * -(F_2_562 as i64), z3 * -(F_1_961 as i64) + z5, z4 * -(F_0_390 as i64) + z5);
        a0 += z1 + z3;
        a1 += z2 + z4;
        a2 += z2 + z3;
        a3 += z1 + z4;
        let s = CONST_BITS + PASS1_BITS + 3;
        let px = |v: i64| (((v + (1i64 << (s - 1))) >> s) + 128).clamp(0, 255) as u8;
        let o = &mut out[row * 8..row * 8 + 8];
        o[0] = px(t10 + a3);
        o[7] = px(t10 - a3);
        o[1] = px(t11 + a2);
        o[6] = px(t11 - a2);
        o[2] = px(t12 + a1);
        o[5] = px(t12 - a1);
        o[3] = px(t13 + a0);
        o[4] = px(t13 - a0);
    }
}

/// Forward DCT of level-shifted samples. The result is scaled up by 8 relative to the true DCT.
fn fdct(block: &mut [i32; 64]) {
    for row in 0..8 {
        let d = |i: usize| block[row * 8 + i];
        let (t0, t7, t1, t6, t2, t5, t3, t4) = (d(0) + d(7), d(0) - d(7), d(1) + d(6), d(1) - d(6), d(2) + d(5), d(2) - d(5), d(3) + d(4), d(3) - d(4));
        let (t10, t13, t11, t12) = (t0 + t3, t0 - t3, t1 + t2, t1 - t2);
        let mut o = [0i32; 8];
        o[0] = (t10 + t11) << PASS1_BITS;
        o[4] = (t10 - t11) << PASS1_BITS;
        let z1 = (t12 + t13) * F_0_541;
        o[2] = descale(z1 + t13 * F_0_765, CONST_BITS - PASS1_BITS);
        o[6] = descale(z1 + t12 * -F_1_847, CONST_BITS - PASS1_BITS);
        let (z1, z2, z3, z4) = (t4 + t7, t5 + t6, t4 + t6, t5 + t7);
        let z5 = (z3 + z4) * F_1_175;
        let (t4, t5, t6, t7) = (t4 * F_0_298, t5 * F_2_053, t6 * F_3_072, t7 * F_1_501);
        let (z1, z2, z3, z4) = (z1 * -F_0_899, z2 * -F_2_562, z3 * -F_1_961 + z5, z4 * -F_0_390 + z5);
        o[7] = descale(t4 + z1 + z3, CONST_BITS - PASS1_BITS);
        o[5] = descale(t5 + z2 + z4, CONST_BITS - PASS1_BITS);
        o[3] = descale(t6 + z2 + z3, CONST_BITS - PASS1_BITS);
        o[1] = descale(t7 + z1 + z4, CONST_BITS - PASS1_BITS);
        block[row * 8..row * 8 + 8].copy_from_slice(&o);
    }
    for col in 0..8 {
        let d = |i: usize| block[i * 8 + col];
        let (t0, t7, t1, t6, t2, t5, t3, t4) = (d(0) + d(7), d(0) - d(7), d(1) + d(6), d(1) - d(6), d(2) + d(5), d(2) - d(5), d(3) + d(4), d(3) - d(4));
        let (t10, t13, t11, t12) = (t0 + t3, t0 - t3, t1 + t2, t1 - t2);
        let mut o = [0i32; 8];
        o[0] = descale(t10 + t11, PASS1_BITS);
        o[4] = descale(t10 - t11, PASS1_BITS);
        let z1 = (t12 + t13) * F_0_541;
        o[2] = descale(z1 + t13 * F_0_765, CONST_BITS + PASS1_BITS);
        o[6] = descale(z1 + t12 * -F_1_847, CONST_BITS + PASS1_BITS);
        let (z1, z2, z3, z4) = (t4 + t7, t5 + t6, t4 + t6, t5 + t7);
        let z5 = (z3 + z4) * F_1_175;
        let (t4, t5, t6, t7) = (t4 * F_0_298, t5 * F_2_053, t6 * F_3_072, t7 * F_1_501);
        let (z1, z2, z3, z4) = (z1 * -F_0_899, z2 * -F_2_562, z3 * -F_1_961 + z5, z4 * -F_0_390 + z5);
        o[7] = descale(t4 + z1 + z3, CONST_BITS + PASS1_BITS);
        o[5] = descale(t5 + z2 + z4, CONST_BITS + PASS1_BITS);
        o[3] = descale(t6 + z2 + z3, CONST_BITS + PASS1_BITS);
        o[1] = descale(t7 + z1 + z4, CONST_BITS + PASS1_BITS);
        for (i, v) in o.iter().enumerate() {
            block[i * 8 + col] = *v;
        }
    }
}

// ---- Huffman tables ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Huff {
    maxcode: [i32; 18],
    valptr: [usize; 17],
    mincode: [i32; 17],
    vals: Vec<u8>,
}

impl Huff {
    fn new(bits: &[u8; 16], vals: &[u8]) -> Result<Self> {
        let total: usize = bits.iter().map(|&b| b as usize).sum();
        if total > vals.len() || total > 256 {
            return Err(Error::invalid_input("JPEG Huffman table is inconsistent"));
        }
        let mut h = Huff { vals: vals[..total].to_vec(), ..Default::default() };
        let (mut code, mut k) = (0i32, 0usize);
        for len in 1..=16 {
            h.valptr[len] = k;
            h.mincode[len] = code;
            code += bits[len - 1] as i32;
            k += bits[len - 1] as usize;
            h.maxcode[len] = if bits[len - 1] > 0 { code - 1 } else { -1 };
            code <<= 1;
        }
        h.maxcode[17] = i32::MAX;
        Ok(h)
    }
}

/// Reads the entropy-coded segments of a scan, undoing 0xFF00 byte stuffing.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u32,
    n: u32,
    marker: bool,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8], pos: usize) -> Self {
        Self { d, pos, acc: 0, n: 0, marker: false }
    }

    fn fill(&mut self) {
        while self.n <= 24 {
            let b = if self.marker || self.pos >= self.d.len() {
                self.marker = true;
                0
            } else {
                let b = self.d[self.pos];
                if b == 0xFF {
                    if self.d.get(self.pos + 1) == Some(&0) {
                        self.pos += 2;
                        0xFF
                    } else {
                        self.marker = true;
                        0
                    }
                } else {
                    self.pos += 1;
                    b
                }
            };
            self.acc |= (b as u32) << (24 - self.n);
            self.n += 8;
        }
    }

    fn bits(&mut self, k: u32) -> i32 {
        if k == 0 {
            return 0;
        }
        self.fill();
        let v = self.acc >> (32 - k);
        self.acc <<= k;
        self.n -= k;
        v as i32
    }

    fn symbol(&mut self, h: &Huff) -> Result<u8> {
        let mut code = 0i32;
        for len in 1..=16usize {
            code = (code << 1) | self.bits(1);
            if code <= h.maxcode[len] {
                let i = h.valptr[len] + (code - h.mincode[len]) as usize;
                return h.vals.get(i).copied().ok_or_else(|| Error::invalid_input("JPEG Huffman code is outside its table"));
            }
        }
        Err(Error::invalid_input("JPEG entropy data holds an invalid Huffman code"))
    }

    /// Skips to just after the next restart marker, as at the end of a restart interval.
    fn restart(&mut self) -> Result<()> {
        self.acc = 0;
        self.n = 0;
        self.marker = false;
        let mut p = self.pos;
        while p + 1 < self.d.len() {
            if self.d[p] == 0xFF && (0xD0..=0xD7).contains(&self.d[p + 1]) {
                self.pos = p + 2;
                return Ok(());
            }
            if self.d[p] == 0xFF && self.d[p + 1] != 0 && self.d[p + 1] != 0xFF {
                break;
            }
            p += 1;
        }
        Err(Error::invalid_input("JPEG restart marker is missing"))
    }
}

fn extend(v: i32, t: u32) -> i32 {
    if t == 0 {
        0
    } else if v < 1 << (t - 1) {
        v - (1 << t) + 1
    } else {
        v
    }
}

// ---- decoder ----------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    td: usize,
    ta: usize,
    pred: i32,
    stride: usize,
    plane: Vec<u8>,
}

struct Frame0 {
    width: usize,
    height: usize,
    comps: Vec<Comp>,
    progressive: bool,
}

impl Frame0 {
    fn hmax(&self) -> usize {
        self.comps.iter().map(|c| c.h).max().unwrap_or(1)
    }
    fn vmax(&self) -> usize {
        self.comps.iter().map(|c| c.v).max().unwrap_or(1)
    }
    fn pix_fmt(&self) -> Result<PixelFormat> {
        let s: Vec<(usize, usize)> = self.comps.iter().map(|c| (c.h, c.v)).collect();
        let (hm, vm) = (self.hmax(), self.vmax());
        // luma carries the highest factors; the two chroma components match each other and divide them
        let ok = s.len() == 1 || (s[0] == (hm, vm) && s[1] == s[2] && hm % s[1].0 == 0 && vm % s[1].1 == 0);
        match (s.len(), if s.len() == 3 { (hm / s[1].0.max(1), vm / s[1].1.max(1)) } else { (1, 1) }) {
            (1, _) => Ok(PixelFormat::Gray8),
            (3, (2, 2)) if ok => Ok(PixelFormat::Yuv420p),
            (3, (2, 1)) if ok => Ok(PixelFormat::Yuv422p),
            (3, (1, 1)) if ok => Ok(PixelFormat::Yuv444p),
            _ => Err(unsupported(format!("JPEG sampling factors {s:?} are not supported (gray, 4:2:0, 4:2:2 and 4:4:4 are)"))),
        }
    }
}

fn be16(d: &[u8], at: usize) -> Result<usize> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| Error::invalid_input("JPEG ends inside a marker segment"))
}

fn read_dqt(seg: &[u8], qt: &mut [[u16; 64]; 4]) -> Result<()> {
    let mut p = 0;
    while p < seg.len() {
        let (prec, id) = (seg[p] >> 4, (seg[p] & 15) as usize);
        p += 1;
        if id > 3 {
            return Err(Error::invalid_input("JPEG quantisation table id is out of range"));
        }
        for k in 0..64 {
            let v = if prec == 0 { *seg.get(p).ok_or_else(|| Error::invalid_input("JPEG DQT is truncated"))? as u16 } else { be16(seg, p)? as u16 };
            p += if prec == 0 { 1 } else { 2 };
            qt[id][k] = v;
        }
    }
    Ok(())
}

fn read_dht(seg: &[u8], dc: &mut [Option<Huff>; 4], ac: &mut [Option<Huff>; 4]) -> Result<()> {
    let mut p = 0;
    while p + 17 <= seg.len() {
        let (class, id) = (seg[p] >> 4, (seg[p] & 15) as usize);
        let mut bits = [0u8; 16];
        bits.copy_from_slice(&seg[p + 1..p + 17]);
        let n: usize = bits.iter().map(|&b| b as usize).sum();
        let vals = seg.get(p + 17..p + 17 + n).ok_or_else(|| Error::invalid_input("JPEG DHT is truncated"))?;
        if id > 3 {
            return Err(Error::invalid_input("JPEG Huffman table id is out of range"));
        }
        let h = Huff::new(&bits, vals)?;
        if class == 0 {
            dc[id] = Some(h);
        } else {
            ac[id] = Some(h);
        }
        p += 17 + n;
    }
    Ok(())
}

/// Reads the dimensions and layout of a JPEG without decoding it.
pub fn probe(data: &[u8]) -> Result<(u32, u32, PixelFormat)> {
    let f = parse_headers(data)?.0;
    Ok((f.width as u32, f.height as u32, f.pix_fmt()?))
}

/// Parses markers up to the first scan. Returns the frame, quantisation tables, Huffman tables,
/// restart interval and the offset of the first scan's header.
type Headers = (Frame0, [[u16; 64]; 4], [Option<Huff>; 4], [Option<Huff>; 4], usize, usize);

fn parse_headers(d: &[u8]) -> Result<Headers> {
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return Err(Error::invalid_input("not a JPEG: no SOI marker"));
    }
    let mut qt = [[0u16; 64]; 4];
    let mut dc: [Option<Huff>; 4] = Default::default();
    let mut ac: [Option<Huff>; 4] = Default::default();
    let mut restart = 0usize;
    let mut frame: Option<Frame0> = None;
    let mut pos = 2;
    loop {
        // find the next marker
        while pos < d.len() && d[pos] != 0xFF {
            pos += 1;
        }
        while pos < d.len() && d[pos] == 0xFF {
            pos += 1;
        }
        let m = *d.get(pos).ok_or_else(|| Error::invalid_input("JPEG ends before its first scan"))?;
        pos += 1;
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            continue;
        }
        if m == 0xD9 {
            return Err(Error::invalid_input("JPEG has no scan"));
        }
        let len = be16(d, pos)?;
        let seg = d.get(pos + 2..pos + len).ok_or_else(|| Error::invalid_input("JPEG marker segment is truncated"))?;
        match m {
            0xDB => read_dqt(seg, &mut qt)?,
            0xC4 => read_dht(seg, &mut dc, &mut ac)?,
            0xDD => restart = be16(seg, 0)?,
            0xC0..=0xC2 => {
                if seg.len() < 6 || seg[0] != 8 {
                    return Err(unsupported(format!("JPEG sample precision {} is not supported (8 bit is)", seg.first().copied().unwrap_or(0))));
                }
                let (h, w, n) = (be16(seg, 1)?, be16(seg, 3)?, seg[5] as usize);
                if w == 0 || h == 0 || !(n == 1 || n == 3) || seg.len() < 6 + 3 * n {
                    return Err(unsupported(format!("JPEG frame of {w}x{h} with {n} components is not supported (1 or 3 components, non-zero size)")));
                }
                if w * h > 1 << 28 {
                    return Err(unsupported(format!("JPEG frame of {w}x{h} is larger than the 268 megapixels this decoder accepts")));
                }
                let comps = (0..n)
                    .map(|i| {
                        let b = &seg[6 + 3 * i..9 + 3 * i];
                        Comp { id: b[0], h: (b[1] >> 4) as usize, v: (b[1] & 15) as usize, tq: (b[2] & 3) as usize, ..Default::default() }
                    })
                    .collect::<Vec<_>>();
                if comps.iter().any(|c| c.h == 0 || c.v == 0 || c.h > 4 || c.v > 4) {
                    return Err(Error::invalid_input("JPEG sampling factor is out of range"));
                }
                frame = Some(Frame0 { width: w, height: h, comps, progressive: m == 0xC2 });
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err(unsupported(format!("JPEG coding process SOF{} (lossless, hierarchical or arithmetic) is not supported", m - 0xC0))),
            0xDA => {
                let f = frame.ok_or_else(|| Error::invalid_input("JPEG scan comes before its frame header"))?;
                return Ok((f, qt, dc, ac, restart, pos));
            }
            _ => {}
        }
        pos += len;
    }
}

/// Crops the padded component planes to the picture.
fn finish(f: &Frame0, pix_fmt: PixelFormat, pts: i64) -> VideoFrame {
    let layout = pix_fmt.planes(f.width, f.height);
    let planes = f
        .comps
        .iter()
        .zip(&layout)
        .map(|(c, &(w, h, _))| {
            let mut out = Vec::with_capacity(w * h);
            for r in 0..h {
                out.extend_from_slice(&c.plane[r * c.stride..r * c.stride + w]);
            }
            out
        })
        .collect();
    VideoFrame { width: f.width as u32, height: f.height as u32, pix_fmt, pts, planes }
}

pub struct JpegDecoder;

impl JpegDecoder {
    pub fn new() -> Self {
        Self
    }

    fn decode_image(&self, d: &[u8], pts: i64) -> Result<VideoFrame> {
        let (mut f, qt, dc, ac, mut restart, mut sos) = parse_headers(d)?;
        let pix_fmt = f.pix_fmt()?;
        if f.progressive {
            return self.decode_progressive(d, f, pix_fmt, pts);
        }
        let (hmax, vmax) = (f.hmax(), f.vmax());
        let (mcux, mcuy) = (f.width.div_ceil(8 * hmax), f.height.div_ceil(8 * vmax));
        for c in &mut f.comps {
            c.stride = mcux * c.h * 8;
            c.plane = vec![0; c.stride * mcuy * c.v * 8];
        }
        let mut done = vec![false; f.comps.len()];
        let mut dcs = dc;
        let mut acs = ac;
        let mut qts = qt;
        let mut pos = sos;
        loop {
            // the scan header at `pos` (just after the SOS marker)
            let len = be16(d, pos)?;
            let seg = d.get(pos + 2..pos + len).ok_or_else(|| Error::invalid_input("JPEG scan header is truncated"))?;
            let ns = *seg.first().ok_or_else(|| Error::invalid_input("JPEG scan header is empty"))? as usize;
            if seg.len() < 1 + 2 * ns + 3 || ns == 0 || ns > f.comps.len() {
                return Err(Error::invalid_input("JPEG scan header is inconsistent"));
            }
            let mut order = Vec::new();
            for i in 0..ns {
                let (cid, t) = (seg[1 + 2 * i], seg[2 + 2 * i]);
                let ci = f.comps.iter().position(|c| c.id == cid).ok_or_else(|| Error::invalid_input("JPEG scan names an unknown component"))?;
                f.comps[ci].td = (t >> 4) as usize & 3;
                f.comps[ci].ta = (t & 15) as usize & 3;
                order.push(ci);
                done[ci] = true;
            }
            let mut bits = Bits::new(d, pos + len);
            let single = ns == 1;
            let (bw, bh) = if single {
                let c = &f.comps[order[0]];
                let cw = (f.width * c.h).div_ceil(hmax);
                let ch = (f.height * c.v).div_ceil(vmax);
                (cw.div_ceil(8), ch.div_ceil(8))
            } else {
                (mcux, mcuy)
            };
            for c in &mut f.comps {
                c.pred = 0;
            }
            let mut since = 0usize;
            let mut coef = [0i32; 64];
            let mut px = [0u8; 64];
            for by in 0..bh {
                for bx in 0..bw {
                    if restart > 0 && since == restart {
                        bits.restart()?;
                        for c in &mut f.comps {
                            c.pred = 0;
                        }
                        since = 0;
                    }
                    since += 1;
                    for &ci in &order {
                        let (ch, cv) = if single { (1, 1) } else { (f.comps[ci].h, f.comps[ci].v) };
                        for v in 0..cv {
                            for h in 0..ch {
                                let (td, ta, tq) = (f.comps[ci].td, f.comps[ci].ta, f.comps[ci].tq);
                                let dct = dcs[td].as_ref().ok_or_else(|| Error::invalid_input("JPEG uses a DC Huffman table it never defined"))?;
                                let act = acs[ta].as_ref().ok_or_else(|| Error::invalid_input("JPEG uses an AC Huffman table it never defined"))?;
                                let q = &qts[tq];
                                coef.fill(0);
                                let t = bits.symbol(dct)? as u32;
                                if t > 11 {
                                    return Err(Error::invalid_input("JPEG DC category is out of range"));
                                }
                                let diff = extend(bits.bits(t), t);
                                f.comps[ci].pred = f.comps[ci].pred.wrapping_add(diff);
                                coef[0] = f.comps[ci].pred * q[0] as i32;
                                let mut k = 1;
                                while k < 64 {
                                    let rs = bits.symbol(act)?;
                                    let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
                                    if s == 0 {
                                        if r == 15 {
                                            k += 16;
                                            continue;
                                        }
                                        break;
                                    }
                                    k += r;
                                    if k > 63 {
                                        return Err(Error::invalid_input("JPEG coefficient run goes past the block"));
                                    }
                                    coef[ZIGZAG[k]] = extend(bits.bits(s), s) * q[k] as i32;
                                    k += 1;
                                }
                                idct(&coef, &mut px);
                                let c = &mut f.comps[ci];
                                let (x0, y0) = if single { (bx * 8, by * 8) } else { ((bx * c.h + h) * 8, (by * c.v + v) * 8) };
                                if x0 + 8 > c.stride || (y0 + 8) * c.stride > c.plane.len() {
                                    continue;
                                }
                                for r in 0..8 {
                                    c.plane[(y0 + r) * c.stride + x0..(y0 + r) * c.stride + x0 + 8].copy_from_slice(&px[r * 8..r * 8 + 8]);
                                }
                            }
                        }
                    }
                }
            }
            // after the scan: the next marker decides what follows
            sos = bits.pos;
            let mut p = sos;
            let mut next = None;
            while p + 1 < d.len() {
                if d[p] == 0xFF && d[p + 1] != 0 && d[p + 1] != 0xFF && !(0xD0..=0xD7).contains(&d[p + 1]) {
                    next = Some((d[p + 1], p + 2));
                    break;
                }
                p += 1;
            }
            // tables and a restart interval may be redefined between scans
            let mut cur = next;
            loop {
                match cur {
                    Some((0xDA, at)) => {
                        pos = at;
                        break;
                    }
                    Some((0xD9, _)) | None => {
                        pos = usize::MAX;
                        break;
                    }
                    Some((m, at)) => {
                        let len = be16(d, at)?;
                        let seg = d.get(at + 2..at + len).ok_or_else(|| Error::invalid_input("JPEG marker segment is truncated"))?;
                        match m {
                            0xDD => restart = be16(seg, 0)?,
                            0xDB | 0xC4 => {
                                // reuse the header parser on a synthetic stream holding just this segment
                                let mut tmp = vec![0xFF, 0xD8, 0xFF, m];
                                tmp.extend_from_slice(&d[at..at + len]);
                                tmp.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xFF, 0xDA, 0, 2]);
                                let (_, q2, d2, a2, _, _) = parse_headers(&tmp)?;
                                if m == 0xDB {
                                    let mut p2 = 0;
                                    while p2 < seg.len() {
                                        let id = (seg[p2] & 15) as usize;
                                        qts[id] = q2[id];
                                        p2 += 1 + if seg[p2] >> 4 == 0 { 64 } else { 128 };
                                    }
                                } else {
                                    for i in 0..4 {
                                        if d2[i].is_some() {
                                            dcs[i] = d2[i].clone();
                                        }
                                        if a2[i].is_some() {
                                            acs[i] = a2[i].clone();
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        let mut q = at + len;
                        let mut nxt = None;
                        while q + 1 < d.len() {
                            if d[q] == 0xFF && d[q + 1] != 0 && d[q + 1] != 0xFF {
                                nxt = Some((d[q + 1], q + 2));
                                break;
                            }
                            q += 1;
                        }
                        cur = nxt;
                    }
                }
            }
            if pos == usize::MAX {
                break;
            }
        }
        if done.iter().any(|d| !d) {
            return Err(Error::invalid_input("JPEG ends before every component was coded"));
        }
        Ok(finish(&f, pix_fmt, pts))
    }

    /// Progressive (SOF2) decoding: every scan adds bits to coefficient buffers kept in zigzag
    /// order, and the inverse DCT runs once at the end.
    fn decode_progressive(&self, d: &[u8], mut f: Frame0, pix_fmt: PixelFormat, pts: i64) -> Result<VideoFrame> {
        if f.width * f.height > 1 << 26 {
            return Err(unsupported(format!("progressive JPEG of {}x{} is larger than the 67 megapixels this decoder accepts (it keeps every coefficient in memory)", f.width, f.height)));
        }
        let (hmax, vmax) = (f.hmax(), f.vmax());
        let (mcux, mcuy) = (f.width.div_ceil(8 * hmax), f.height.div_ceil(8 * vmax));
        // blocks per row and column as stored (padded to whole MCUs)
        let store: Vec<(usize, usize)> = f.comps.iter().map(|c| (mcux * c.h, mcuy * c.v)).collect();
        let mut coefs: Vec<Vec<[i32; 64]>> = store.iter().map(|&(w, h)| vec![[0i32; 64]; w * h]).collect();
        let mut qts = [[0u16; 64]; 4];
        let mut dcs: [Option<Huff>; 4] = Default::default();
        let mut acs: [Option<Huff>; 4] = Default::default();
        let mut restart = 0usize;
        let mut scans = 0;
        let mut pos = 2;
        'markers: loop {
            while pos < d.len() && d[pos] != 0xFF {
                pos += 1;
            }
            while pos < d.len() && d[pos] == 0xFF {
                pos += 1;
            }
            let Some(&m) = d.get(pos) else { break };
            pos += 1;
            if m == 0 || m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
                continue;
            }
            if m == 0xD9 {
                break;
            }
            let len = be16(d, pos)?;
            let seg = d.get(pos + 2..pos + len).ok_or_else(|| Error::invalid_input("JPEG marker segment is truncated"))?;
            match m {
                0xDB => read_dqt(seg, &mut qts)?,
                0xC4 => read_dht(seg, &mut dcs, &mut acs)?,
                0xDD => restart = be16(seg, 0)?,
                0xDA => {
                    let ns = *seg.first().ok_or_else(|| Error::invalid_input("JPEG scan header is empty"))? as usize;
                    if ns == 0 || ns > f.comps.len() || seg.len() < 1 + 2 * ns + 3 {
                        return Err(Error::invalid_input("JPEG scan header is inconsistent"));
                    }
                    let mut order = Vec::new();
                    let mut tables = Vec::new();
                    for i in 0..ns {
                        let (cid, t) = (seg[1 + 2 * i], seg[2 + 2 * i]);
                        let ci = f.comps.iter().position(|c| c.id == cid).ok_or_else(|| Error::invalid_input("JPEG scan names an unknown component"))?;
                        order.push(ci);
                        tables.push(((t >> 4) as usize & 3, (t & 15) as usize & 3));
                    }
                    let (ss, se, a) = (seg[1 + 2 * ns] as usize, seg[2 + 2 * ns] as usize, seg[3 + 2 * ns]);
                    let (ah, al) = ((a >> 4) as u32, (a & 15) as u32);
                    if ss > se || se > 63 || al > 13 || (ss == 0 && se != 0) || (ss > 0 && ns != 1) {
                        return Err(Error::invalid_input("JPEG progressive scan parameters are inconsistent"));
                    }
                    let mut bits = Bits::new(d, pos + len);
                    let single = ns == 1;
                    let (bw, bh) = if single {
                        let c = &f.comps[order[0]];
                        ((f.width * c.h).div_ceil(hmax).div_ceil(8), (f.height * c.v).div_ceil(vmax).div_ceil(8))
                    } else {
                        (mcux, mcuy)
                    };
                    let mut pred = vec![0i32; f.comps.len()];
                    let mut eobrun = 0u32;
                    let mut since = 0usize;
                    for by in 0..bh {
                        for bx in 0..bw {
                            if restart > 0 && since == restart {
                                bits.restart()?;
                                pred.iter_mut().for_each(|p| *p = 0);
                                eobrun = 0;
                                since = 0;
                            }
                            since += 1;
                            for (oi, &ci) in order.iter().enumerate() {
                                let (ch, cv) = if single { (1, 1) } else { (f.comps[ci].h, f.comps[ci].v) };
                                for v in 0..cv {
                                    for h in 0..ch {
                                        let (x, y) = if single { (bx, by) } else { (bx * ch + h, by * cv + v) };
                                        if x >= store[ci].0 || y >= store[ci].1 {
                                            continue;
                                        }
                                        let blk = &mut coefs[ci][y * store[ci].0 + x];
                                        let (td, ta) = tables[oi];
                                        if ss == 0 {
                                            if ah == 0 {
                                                let dct = dcs[td].as_ref().ok_or_else(|| Error::invalid_input("JPEG uses a DC Huffman table it never defined"))?;
                                                let t = bits.symbol(dct)? as u32;
                                                if t > 11 {
                                                    return Err(Error::invalid_input("JPEG DC category is out of range"));
                                                }
                                                pred[ci] = pred[ci].wrapping_add(extend(bits.bits(t), t));
                                                blk[0] = pred[ci].wrapping_shl(al);
                                            } else if bits.bits(1) == 1 {
                                                blk[0] |= 1 << al;
                                            }
                                            continue;
                                        }
                                        let act = acs[ta].as_ref().ok_or_else(|| Error::invalid_input("JPEG uses an AC Huffman table it never defined"))?;
                                        if ah == 0 {
                                            if eobrun > 0 {
                                                eobrun -= 1;
                                                continue;
                                            }
                                            let mut k = ss;
                                            while k <= se {
                                                let rs = bits.symbol(act)?;
                                                let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
                                                if s != 0 {
                                                    k += r;
                                                    if k > se {
                                                        return Err(Error::invalid_input("JPEG coefficient run goes past the band"));
                                                    }
                                                    blk[k] = extend(bits.bits(s), s).wrapping_shl(al);
                                                    k += 1;
                                                } else if r == 15 {
                                                    k += 16;
                                                } else {
                                                    eobrun = (1u32 << r) + if r > 0 { bits.bits(r as u32) as u32 } else { 0 } - 1;
                                                    break;
                                                }
                                            }
                                        } else {
                                            let (p1, m1) = (1i32 << al, -1i32 << al);
                                            let mut k = ss;
                                            if eobrun == 0 {
                                                while k <= se {
                                                    let rs = bits.symbol(act)?;
                                                    let (mut r, s) = ((rs >> 4) as i32, rs & 15);
                                                    let mut newval = 0;
                                                    if s != 0 {
                                                        newval = if bits.bits(1) == 1 { p1 } else { m1 };
                                                    } else if r != 15 {
                                                        eobrun = (1u32 << r) + if r > 0 { bits.bits(r as u32) as u32 } else { 0 };
                                                        break;
                                                    }
                                                    // step over the nonzero coefficients (refining each) and `r` zeros
                                                    while k <= se {
                                                        let c = &mut blk[k];
                                                        if *c != 0 {
                                                            if bits.bits(1) == 1 && *c & p1 == 0 {
                                                                *c = c.wrapping_add(if *c >= 0 { p1 } else { m1 });
                                                            }
                                                        } else {
                                                            r -= 1;
                                                            if r < 0 {
                                                                break;
                                                            }
                                                        }
                                                        k += 1;
                                                    }
                                                    if newval != 0 && k <= se {
                                                        blk[k] = newval;
                                                    }
                                                    k += 1;
                                                }
                                            }
                                            if eobrun > 0 {
                                                while k <= se {
                                                    let c = &mut blk[k];
                                                    if *c != 0 && bits.bits(1) == 1 && *c & p1 == 0 {
                                                        *c = c.wrapping_add(if *c >= 0 { p1 } else { m1 });
                                                    }
                                                    k += 1;
                                                }
                                                eobrun -= 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    scans += 1;
                    pos = bits.pos;
                    continue 'markers;
                }
                _ => {}
            }
            pos += len;
        }
        if scans == 0 {
            return Err(Error::invalid_input("JPEG has no scan"));
        }
        let mut px = [0u8; 64];
        let mut nat = [0i32; 64];
        for (ci, c) in f.comps.iter_mut().enumerate() {
            let (bw, bh) = store[ci];
            c.stride = bw * 8;
            c.plane = vec![0; c.stride * bh * 8];
            let q = &qts[c.tq];
            for by in 0..bh {
                for bx in 0..bw {
                    let blk = &coefs[ci][by * bw + bx];
                    for k in 0..64 {
                        nat[ZIGZAG[k]] = blk[k].wrapping_mul(q[k] as i32);
                    }
                    idct(&nat, &mut px);
                    for r in 0..8 {
                        let o = (by * 8 + r) * c.stride + bx * 8;
                        c.plane[o..o + 8].copy_from_slice(&px[r * 8..r * 8 + 8]);
                    }
                }
            }
        }
        Ok(finish(&f, pix_fmt, pts))
    }
}

impl Default for JpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for JpegDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        Ok(vec![Frame::Video(self.decode_image(&packet.data, packet.pts)?)])
    }
}

// ---- encoder ----------------------------------------------------------------------------------

struct Writer {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Writer {
    fn put(&mut self, code: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (code & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let b = (self.acc >> (self.n - 8)) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
            self.n -= 8;
        }
    }

    fn finish(&mut self) {
        if self.n > 0 {
            self.put((1 << (8 - self.n)) - 1, 8 - self.n);
        }
    }
}

/// Code and length of every symbol, from a table's code counts and symbols.
fn codes(bits: &[u8; 16], vals: &[u8]) -> [(u32, u32); 256] {
    let mut t = [(0u32, 0u32); 256];
    let (mut code, mut k) = (0u32, 0usize);
    for len in 1..=16u32 {
        for _ in 0..bits[len as usize - 1] {
            t[vals[k] as usize] = (code, len);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    t
}

fn scaled(base: &[u16; 64], quality: u32) -> [u16; 64] {
    let q = quality.clamp(1, 100);
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for i in 0..64 {
        out[i] = ((base[i] as u32 * scale + 50) / 100).clamp(1, 255) as u16;
    }
    out
}

pub struct JpegEncoder {
    quality: u32,
}

impl JpegEncoder {
    pub fn new(quality: u32) -> Self {
        Self { quality }
    }

    pub fn default_quality() -> u32 {
        90
    }

    /// The JPEG quality (1 to 100) that stands for a 0 to 51 quantizer: 0 is 100, 5 is 90, 23 is 54.
    pub fn quality_for_qp(qp: u8) -> u32 {
        100u32.saturating_sub(qp as u32 * 2).max(1)
    }

    fn encode_image(&self, v: &VideoFrame) -> Result<Vec<u8>> {
        let (samp, n): (&[(u8, u8)], usize) = match v.pix_fmt {
            PixelFormat::Gray8 => (&[(1, 1)], 1),
            PixelFormat::Yuv420p => (&[(2, 2), (1, 1), (1, 1)], 3),
            PixelFormat::Yuv422p => (&[(2, 1), (1, 1), (1, 1)], 3),
            PixelFormat::Yuv444p => (&[(1, 1), (1, 1), (1, 1)], 3),
            other => return Err(unsupported(format!("the JPEG encoder takes gray, yuv420p, yuv422p or yuv444p frames, not {}", other.name()))),
        };
        let (w, h) = (v.width as usize, v.height as usize);
        if w == 0 || h == 0 || w > 65535 || h > 65535 {
            return Err(Error::invalid_input("JPEG images are 1 to 65535 pixels on each side"));
        }
        let (hmax, vmax) = (samp[0].0 as usize, samp[0].1 as usize);
        let qts = [scaled(&STD_LUMA_Q, self.quality), scaled(&STD_CHROMA_Q, self.quality)];
        let mut o = vec![0xFF, 0xD8];
        // JFIF header
        o.extend([0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0]);
        for (id, q) in qts.iter().enumerate().take(if n == 1 { 1 } else { 2 }) {
            o.extend([0xFF, 0xDB, 0, 67, id as u8]);
            o.extend(ZIGZAG.iter().map(|&z| q[z] as u8));
        }
        o.extend([0xFF, 0xC0, 0, (8 + 3 * n) as u8, 8]);
        o.extend((h as u16).to_be_bytes());
        o.extend((w as u16).to_be_bytes());
        o.push(n as u8);
        for (i, s) in samp.iter().enumerate() {
            o.extend([i as u8 + 1, (s.0 << 4) | s.1, if i == 0 { 0 } else { 1 }]);
        }
        let dht = |o: &mut Vec<u8>, tc_th: u8, bits: &[u8; 16], vals: &[u8]| {
            o.extend([0xFF, 0xC4]);
            o.extend(((19 + vals.len()) as u16).to_be_bytes());
            o.push(tc_th);
            o.extend(bits);
            o.extend(vals);
        };
        dht(&mut o, 0x00, &DC_LUMA_BITS, &DC_VALS);
        dht(&mut o, 0x10, &AC_LUMA_BITS, &AC_LUMA_VALS);
        if n == 3 {
            dht(&mut o, 0x01, &DC_CHROMA_BITS, &DC_VALS);
            dht(&mut o, 0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS);
        }
        o.extend([0xFF, 0xDA, 0, (6 + 2 * n) as u8, n as u8]);
        for i in 0..n {
            o.extend([i as u8 + 1, if i == 0 { 0x00 } else { 0x11 }]);
        }
        o.extend([0, 63, 0]);

        let dc_codes = [codes(&DC_LUMA_BITS, &DC_VALS), codes(&DC_CHROMA_BITS, &DC_VALS)];
        let ac_codes = [codes(&AC_LUMA_BITS, &AC_LUMA_VALS), codes(&AC_CHROMA_BITS, &AC_CHROMA_VALS)];
        let (mcux, mcuy) = (w.div_ceil(8 * hmax), h.div_ceil(8 * vmax));
        let layout = v.pix_fmt.planes(w, h);
        let mut wr = Writer { out: o, acc: 0, n: 0 };
        let mut pred = [0i32; 3];
        for my in 0..mcuy {
            for mx in 0..mcux {
                for ci in 0..n {
                    let (ch, cv) = (samp[ci].0 as usize, samp[ci].1 as usize);
                    let (pw, ph, _) = layout[ci];
                    let plane = &v.planes[ci];
                    let t = (ci > 0) as usize;
                    for by in 0..cv {
                        for bx in 0..ch {
                            let (x0, y0) = ((mx * ch + bx) * 8, (my * cv + by) * 8);
                            let mut blk = [0i32; 64];
                            for r in 0..8 {
                                for c in 0..8 {
                                    // pixels beyond the edge repeat the last row and column
                                    let (x, y) = ((x0 + c).min(pw - 1), (y0 + r).min(ph - 1));
                                    blk[r * 8 + c] = plane[y * pw + x] as i32 - 128;
                                }
                            }
                            fdct(&mut blk);
                            let mut q = [0i32; 64];
                            for k in 0..64 {
                                let z = ZIGZAG[k];
                                let d = qts[t][z] as i32 * 8;
                                let x = blk[z];
                                q[k] = if x >= 0 { (x + d / 2) / d } else { -((-x + d / 2) / d) };
                            }
                            let diff = q[0] - pred[ci];
                            pred[ci] = q[0];
                            let (cat, bitsv) = category(diff);
                            let (code, len) = dc_codes[t][cat as usize];
                            wr.put(code, len);
                            wr.put(bitsv, cat);
                            let mut run = 0;
                            for k in 1..64 {
                                if q[k] == 0 {
                                    run += 1;
                                    continue;
                                }
                                while run > 15 {
                                    let (code, len) = ac_codes[t][0xF0];
                                    wr.put(code, len);
                                    run -= 16;
                                }
                                let (cat, bitsv) = category(q[k]);
                                let (code, len) = ac_codes[t][(run << 4) as usize | cat as usize];
                                wr.put(code, len);
                                wr.put(bitsv, cat);
                                run = 0;
                            }
                            if run > 0 {
                                let (code, len) = ac_codes[t][0];
                                wr.put(code, len);
                            }
                        }
                    }
                }
            }
        }
        wr.finish();
        wr.out.extend([0xFF, 0xD9]);
        Ok(wr.out)
    }
}

/// Size category of a value and the bits that follow its Huffman code.
fn category(v: i32) -> (u32, u32) {
    let a = v.unsigned_abs();
    let cat = 32 - a.leading_zeros();
    let bits = if v < 0 { (v - 1) as u32 & ((1u32 << cat) - 1) } else { v as u32 };
    (cat, if cat == 0 { 0 } else { bits })
}

impl Encoder for JpegEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the JPEG encoder received an audio frame"));
        };
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data: self.encode_image(v)? }])
    }
}
