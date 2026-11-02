//! Apple ProRes: intra-only 4:2:2 at 10 bits (Proxy, LT, Standard and HQ) and 4:4:4 at 12 bits
//! (4444 and 4444 XQ), progressive frames, with the encoder in `enc`.
//!
//! The decoder and the tables follow FFmpeg's `proresdec.c`, `proresdsp.c`, `proresdata.c` and
//! `simple_idct_template.c`, and the encoder its `proresenc_kostya.c`, so that a stream either
//! side writes decodes the same in the other. Copyright (c) 2010-2011 Maxim Poliakovski and Elvis
//! Presley (decoder), 2001 Michael Niedermayer (IDCT), 2011 Anatoliy Wasserman and 2012 Konstantin
//! Shishkov (encoder); LGPL-2.1-or-later. Interlaced pictures are rejected; the alpha plane of a
//! 4444 with alpha is skipped.

mod enc;
mod idct;

pub use enc::ProResEncoder;

use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};

pub use crate::options::Profile;

const FRAME_ID: &[u8; 4] = b"icpf";
const FIRST_DC_CB: u8 = 0xB8;
const DC_CB: [u8; 7] = [0x04, 0x28, 0x28, 0x4D, 0x4D, 0x70, 0x70];
const RUN_CB: [u8; 16] = [0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29, 0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4C];
const LEVEL_CB: [u8; 10] = [0x04, 0x0A, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4C];
/// Raster index of each coefficient in scan order.
const SCAN: [u8; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28, 29, 22, 15, 23, 30, 31, 32, 33,
    40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58, 59, 52, 45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const LUMA_BLOCKS: [(usize, usize); 4] = [(0, 0), (8, 0), (0, 8), (8, 8)];
const CHROMA_BLOCKS_422: [(usize, usize); 2] = [(0, 0), (0, 8)];
const CHROMA_BLOCKS_444: [(usize, usize); 4] = [(0, 0), (0, 8), (8, 0), (8, 8)];

/// The blocks of a macroblock in plane `plane` (0 luma), as offsets of their corners in samples.
pub(crate) fn block_offsets(plane: usize, c444: bool) -> &'static [(usize, usize)] {
    match (plane, c444) {
        (0, _) => &LUMA_BLOCKS,
        (_, false) => &CHROMA_BLOCKS_422,
        _ => &CHROMA_BLOCKS_444,
    }
}

/// Samples across a macroblock in plane `plane`.
pub(crate) fn mb_width(plane: usize, c444: bool) -> usize {
    if plane == 0 || c444 {
        16
    } else {
        8
    }
}

/// The slices of a picture of `mbw` by `mbh` macroblocks, as (x, y, width in macroblocks): 8
/// macroblocks wide, halved where a row's end is nearer.
pub(crate) fn slices(mbw: usize, mbh: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for y in 0..mbh {
        let mut x = 0;
        while x < mbw {
            let mut n = 8;
            while mbw - x < n {
                n >>= 1;
            }
            out.push((x, y, n));
            x += n;
        }
    }
    out
}

fn bad(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("ProRes: {msg}"))
}

fn be16(d: &[u8], at: usize) -> Result<usize> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| bad("truncated header"))
}

fn be32(d: &[u8], at: usize) -> Result<usize> {
    d.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| bad("truncated header"))
}

/// The bits of a slice's plane, most significant first; reads past the end see zeros.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    /// The next `n` bits (1 to 32) without consuming them.
    fn show(&self, n: u32) -> u32 {
        let (byte, sh) = (self.pos >> 3, self.pos & 7);
        let mut w = 0u64;
        for i in 0..5 {
            w = (w << 8) | *self.d.get(byte + i).unwrap_or(&0) as u64;
        }
        ((w >> (40 - sh as u32 - n)) & ((1u64 << n) - 1)) as u32
    }

    fn left(&self) -> i64 {
        self.d.len() as i64 * 8 - self.pos as i64
    }

    fn bit(&mut self) -> u32 {
        let b = self.show(1);
        self.pos += 1;
        b
    }
}

/// One Rice or exp-Golomb codeword with the codebook `cb` (FFmpeg's DECODE_CODEWORD).
fn codeword(br: &mut Bits, cb: u8) -> Result<u32> {
    let buf = br.show(32);
    let switch = (cb & 3) as i64;
    let rice = (cb >> 5) as u32;
    let exp = ((cb >> 2) & 7) as i64;
    let q = if buf == 0 { 31 } else { buf.leading_zeros() };
    if q as i64 > switch {
        let bits = exp - switch + 2 * q as i64;
        if !(1..=31).contains(&bits) {
            return Err(bad("a codeword runs past its slice"));
        }
        let bits = bits as u32;
        let val = (buf >> (32 - bits)).wrapping_sub(1 << exp).wrapping_add(((switch + 1) as u32) << rice);
        br.pos += bits as usize;
        Ok(val)
    } else if rice != 0 {
        let bits = q + 1 + rice;
        br.pos += bits as usize;
        Ok((q << rice) + ((buf >> (32 - bits)) & ((1 << rice) - 1)))
    } else {
        br.pos += q as usize + 1;
        Ok(q)
    }
}

fn tosigned(x: i32) -> i32 {
    (x >> 1) ^ -(x & 1)
}

/// The DC coefficients of `blocks` (64 per block): the first, then differences.
fn decode_dcs(br: &mut Bits, blocks: &mut [i16]) -> Result<()> {
    let n = blocks.len() / 64;
    let mut prev = tosigned(codeword(br, FIRST_DC_CB)? as i32) as i16;
    blocks[0] = prev;
    let (mut code, mut sign) = (5i32, 0i32);
    for b in 1..n {
        let cb = if code < 0 { 6 } else { code.min(6) as usize };
        code = codeword(br, DC_CB[cb])? as i32;
        sign = if code != 0 { sign ^ -(code & 1) } else { 0 };
        prev = prev.wrapping_add((((code.wrapping_add(1)) >> 1) ^ sign).wrapping_sub(sign) as i16);
        blocks[b * 64] = prev;
    }
    Ok(())
}

/// The AC coefficients: runs and levels, interleaved over the blocks in scan order.
fn decode_acs(br: &mut Bits, blocks: &mut [i16]) -> Result<()> {
    let n = blocks.len() / 64;
    let log2 = n.ilog2();
    let (mut run, mut level) = (4u32, 2u32);
    let max = 64u32 << log2;
    let mask = n as u32 - 1;
    let mut pos = mask;
    loop {
        let left = br.left();
        if left <= 0 || (left < 32 && br.show(left as u32) == 0) {
            break;
        }
        run = codeword(br, RUN_CB[run.min(15) as usize])?;
        pos = pos.wrapping_add(run).wrapping_add(1);
        if pos >= max {
            return Err(bad("an AC run runs past its slice"));
        }
        level = codeword(br, LEVEL_CB[level.min(9) as usize])?.wrapping_add(1);
        let sign = if br.bit() == 1 { u32::MAX } else { 0 };
        let v = (level ^ sign).wrapping_sub(sign);
        blocks[((pos & mask) as usize) << 6 | SCAN[(pos >> log2) as usize] as usize] = v as i16;
    }
    Ok(())
}

fn decode_coeffs(d: &[u8], blocks: &mut [i16]) -> Result<()> {
    let mut br = Bits { d, pos: 0 };
    decode_dcs(&mut br, blocks)?;
    decode_acs(&mut br, blocks)
}

/// The frame header: the picture size, chroma format and quantisation matrices.
struct FrameHeader {
    width: usize,
    height: usize,
    c444: bool,
    /// Bytes of header, before the picture.
    size: usize,
    luma: [u8; 64],
    chroma: [u8; 64],
}

fn matrix(f: &[u8], at: usize) -> Result<[u8; 64]> {
    f.get(at..at + 64).map(|m| m.try_into().unwrap()).ok_or_else(|| bad("the quantisation matrices are truncated"))
}

impl FrameHeader {
    fn parse(f: &[u8]) -> Result<Self> {
        if f.len() < 20 {
            return Err(bad("the frame header is truncated"));
        }
        let size = be16(f, 0)?;
        if size > f.len() {
            return Err(bad("the frame header is larger than the frame"));
        }
        if be16(f, 2)? > 1 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "ProRes: unsupported bitstream version"));
        }
        let (width, height) = (be16(f, 8)?, be16(f, 10)?);
        if width == 0 || height == 0 {
            return Err(bad("the frame has no size"));
        }
        if (f[12] >> 2) & 3 != 0 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "ProRes: interlaced pictures are not supported").hint("only progressive ProRes is decoded"));
        }
        if f[17] & 0xF > 2 {
            return Err(bad("an invalid alpha mode"));
        }
        let flags = f[19];
        let mut at = 20;
        let luma = if flags & 2 != 0 {
            at += 64;
            matrix(f, 20)?
        } else {
            [4; 64]
        };
        let chroma = if flags & 1 != 0 { matrix(f, at)? } else { luma };
        Ok(Self { width, height, c444: f[12] & 0xC0 == 0xC0, size, luma, chroma })
    }
}

/// `m` times the quantiser `q`, in the 16-bit words the transform takes.
fn scaled(m: &[u8; 64], q: i32) -> [i16; 64] {
    std::array::from_fn(|i| (m[i] as i32 * q) as i16)
}

/// The blocks of a plane's slice, inverse transformed into the planes at macroblock `(mbx, mby)`.
#[allow(clippy::too_many_arguments)]
fn put_blocks(blocks: &mut [i16], offs: &[(usize, usize)], qm: &[i16; 64], twelve: bool, plane: &mut [u16], stride: usize, mb: (usize, usize), mbw_px: usize) {
    let per = offs.len();
    for k in 0..blocks.len() / (64 * per) {
        for (t, &(dx, dy)) in offs.iter().enumerate() {
            let b = (k * per + t) * 64;
            let block: &mut [i16; 64] = (&mut blocks[b..b + 64]).try_into().unwrap();
            let (x, y) = ((mb.0 + k) * mbw_px + dx, mb.1 * 16 + dy);
            idct::put(block, qm, twelve, &mut plane[y * stride + x..], stride);
        }
    }
}

fn decode_slice(d: &[u8], h: &FrameHeader, (x, y, n): (usize, usize, usize), planes: &mut [Vec<u16>; 3], stride: [usize; 3]) -> Result<()> {
    let hdr = (d[0] >> 3) as usize;
    if hdr < 6 || hdr > d.len() {
        return Err(bad("the slice header is truncated"));
    }
    let mut q = d[1].clamp(1, 224) as i32;
    if q > 128 {
        q = (q - 96) << 2;
    }
    let (ys, us) = (be16(d, 2)?, be16(d, 4)?);
    let vs = if hdr > 7 { be16(d, 6)? } else { d.len().checked_sub(hdr + ys + us).ok_or_else(|| bad("the slice planes are larger than the slice"))? };
    if hdr + ys + us + vs > d.len() {
        return Err(bad("the slice planes are larger than the slice"));
    }
    let body = &d[hdr..];
    let (yb, rest) = body.split_at(ys);
    let (ub, rest) = rest.split_at(us);
    let vb = &rest[..vs];
    let (ql, qc) = (scaled(&h.luma, q), scaled(&h.chroma, q));
    let mut blocks = vec![0i16; n * 4 * 64];
    decode_coeffs(yb, &mut blocks)?;
    put_blocks(&mut blocks, &LUMA_BLOCKS, &ql, h.c444, &mut planes[0], stride[0], (x, y), 16);
    let per = block_offsets(1, h.c444).len();
    for (p, data) in [(1, ub), (2, vb)] {
        let mut blocks = vec![0i16; n * per * 64];
        if !data.is_empty() {
            decode_coeffs(data, &mut blocks)?;
        }
        put_blocks(&mut blocks, block_offsets(p, h.c444), &qc, h.c444, &mut planes[p], stride[p], (x, y), mb_width(p, h.c444));
    }
    Ok(())
}

/// Decodes one ProRes frame (the packet, from its `icpf` atom on) to 10-bit 4:2:2 or 12-bit 4:4:4.
pub fn decode_frame(d: &[u8], pts: i64) -> Result<VideoFrame> {
    if d.len() < 28 || &d[4..8] != FRAME_ID {
        return Err(bad("the packet is not a frame (no 'icpf' atom)"));
    }
    let f = &d[8..];
    let h = FrameHeader::parse(f)?;
    let (mbw, mbh) = (h.width.div_ceil(16), h.height.div_ceil(16));
    let stride: [usize; 3] = std::array::from_fn(|p| mbw * mb_width(p, h.c444));
    let mut planes: [Vec<u16>; 3] = std::array::from_fn(|p| vec![0u16; stride[p] * mbh * 16]);
    let pic = &f[h.size..];
    if pic.len() < 8 || (pic[0] >> 3) < 8 || (pic[0] >> 3) as usize > pic.len() {
        return Err(bad("the picture header is truncated"));
    }
    let hdr = (pic[0] >> 3) as usize;
    if be32(pic, 1)? > pic.len() {
        return Err(bad("the picture is larger than the frame"));
    }
    let (lw, lh) = (pic[7] >> 4, pic[7] & 15);
    if lw > 3 || lh != 0 {
        return Err(bad("an unsupported slice size"));
    }
    let list = slices(mbw, mbh);
    let index = hdr;
    if pic.len() < index + 2 * list.len() {
        return Err(bad("the slice table is truncated"));
    }
    let mut at = index + 2 * list.len();
    for (i, &s) in list.iter().enumerate() {
        let len = be16(pic, index + 2 * i)?;
        let data = pic.get(at..at + len).filter(|_| len >= 6).ok_or_else(|| bad("a slice runs past the picture"))?;
        decode_slice(data, &h, s, &mut planes, stride)?;
        at += len;
    }
    let pix_fmt = if h.c444 { PixelFormat::Yuv444p12 } else { PixelFormat::Yuv422p10 };
    let out = pix_fmt
        .planes(h.width, h.height)
        .iter()
        .enumerate()
        .map(|(p, &(pw, ph, _))| {
            let mut v = Vec::with_capacity(pw * ph * 2);
            for row in 0..ph {
                for s in &planes[p][row * stride[p]..row * stride[p] + pw] {
                    v.extend_from_slice(&s.to_le_bytes());
                }
            }
            v
        })
        .collect();
    Ok(VideoFrame { width: h.width as u32, height: h.height as u32, pix_fmt, pts, planes: out })
}

/// Decodes ProRes packets (one frame each) to video frames.
pub struct ProResDecoder;

impl ProResDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProResDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for ProResDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        Ok(vec![Frame::Video(decode_frame(&packet.data, packet.pts)?)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{Encoder, Rational, StreamParams};

    fn psnr(a: &[u8], b: &[u8], peak: f64) -> f64 {
        let se: f64 = a.chunks(2).zip(b.chunks(2)).map(|(x, y)| {
            let (x, y) = (u16::from_le_bytes([x[0], x[1]]) as f64, u16::from_le_bytes([y[0], y[1]]) as f64);
            (x - y) * (x - y)
        }).sum();
        10.0 * (peak * peak * a.len() as f64 / 2.0 / se).log10()
    }

    fn picture(w: u32, h: u32, pix_fmt: PixelFormat) -> VideoFrame {
        let planes = pix_fmt
            .planes(w as usize, h as usize)
            .iter()
            .enumerate()
            .map(|(p, &(pw, ph, _))| {
                let mut v = Vec::new();
                for y in 0..ph {
                    for x in 0..pw {
                        let s = (512.0 + 380.0 * ((x as f64 + 3.0 * p as f64) * 0.19).sin() * ((y as f64) * 0.11).cos() + (x * 7 + y * 3) as f64 % 23.0) as u16;
                        v.extend_from_slice(&(s.min(1023) << if pix_fmt == PixelFormat::Yuv444p12 { 2 } else { 0 }).to_le_bytes());
                    }
                }
                v
            })
            .collect();
        VideoFrame { width: w, height: h, pix_fmt, pts: 0, planes }
    }

    #[test]
    fn a_422_frame_round_trips_above_45_db() {
        let src = picture(130, 74, PixelFormat::Yuv422p10);
        let params = StreamParams::Video { width: 130, height: 74, pix_fmt: PixelFormat::Yuv422p10, frame_rate: Rational::new(25, 1) };
        let mut enc = ProResEncoder::new(&params, Profile::Hq).unwrap();
        let pkt = enc.encode(&Frame::Video(src.clone())).unwrap().remove(0);
        assert_eq!(&pkt.data[4..8], b"icpf");
        let out = decode_frame(&pkt.data, 0).unwrap();
        assert_eq!((out.width, out.height, out.pix_fmt), (130, 74, PixelFormat::Yuv422p10));
        assert_eq!(out.planes.iter().map(Vec::len).collect::<Vec<_>>(), vec![130 * 74 * 2, 65 * 74 * 2, 65 * 74 * 2]);
        for p in 0..3 {
            assert!(psnr(&src.planes[p], &out.planes[p], 1023.0) > 45.0, "plane {p}");
        }
    }

    #[test]
    fn a_444_frame_decodes_at_twelve_bits() {
        let src = picture(64, 48, PixelFormat::Yuv444p12);
        let params = StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv444p12, frame_rate: Rational::new(25, 1) };
        let mut enc = ProResEncoder::new(&params, Profile::Fourfour).unwrap();
        let pkt = enc.encode(&Frame::Video(src.clone())).unwrap().remove(0);
        let out = decode_frame(&pkt.data, 0).unwrap();
        assert_eq!(out.pix_fmt, PixelFormat::Yuv444p12);
        assert!(psnr(&src.planes[0], &out.planes[0], 4095.0) > 45.0);
    }

    #[test]
    fn fourccs_round_trip() {
        for p in [Profile::Proxy, Profile::Lt, Profile::Standard, Profile::Hq, Profile::Fourfour, Profile::FourfourXq] {
            assert_eq!(Profile::from_fourcc(&p.fourcc()), Some(p));
        }
        assert_eq!(Profile::from_fourcc(b"avc1"), None);
    }
}
