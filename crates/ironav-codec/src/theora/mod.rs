//! Theora video decoder (Theora I, version 3.2 and later).
//!
//! A translation of FFmpeg's `vp3.c` (the Theora paths), `vp3data.h` and `vp3dsp.c`, to the same
//! results bit for bit: the three header packets (identification, comments, setup with its
//! quantisation tables, loop filter limits and 80 Huffman tables), coded block flags, macroblock
//! modes, motion vectors (the VLC and the fixed-length scheme), DCT tokens with EOB runs, DC
//! prediction, the integer inverse DCT, intra and inter reconstruction with half-pel motion, the
//! loop filter, 4:2:0, 4:2:2 and 4:4:4, the picture region, and the golden and previous frames.
//!
//! Pictures are decoded the way the format stores them, with the bottom row first; the output is
//! flipped and cropped to the picture region of the identification header.
//!
//! Not decoded: the pre-release bitstreams older than 3.2.0 (alpha), which differ in their headers.
//! Frames larger than 16.7 million coded pixels are refused, so a damaged header cannot ask for a
//! huge allocation.
//!
//! The packets come from Ogg (one packet per page entry) or Matroska (`V_THEORA`); the codec data of a
//! stream is the three header packets in Xiph lacing, as Matroska keeps them.

use ironav_core::{ColorInfo, Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamInfo, VideoFrame};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("Theora: {msg}"))
}

/// The most coded pixels (frame width times height, in whole macroblocks) that are decoded.
const MAX_PIXELS: u64 = 1 << 24;

const MODE_INTER_NO_MV: u8 = 0;
const MODE_INTRA: u8 = 1;
const MODE_INTER_PLUS_MV: u8 = 2;
const MODE_INTER_LAST_MV: u8 = 3;
const MODE_INTER_PRIOR_LAST: u8 = 4;
const MODE_USING_GOLDEN: u8 = 5;
const MODE_GOLDEN_MV: u8 = 6;
const MODE_INTER_FOURMV: u8 = 7;
/// Internal: a fragment copied from the previous frame.
const MODE_COPY: u8 = 8;

/// The longest run a single run-length code can say; Theora reads a bit instead of flipping after it.
const MAXIMUM_LONG_BIT_RUN: i32 = 4129;

/// The six preset schemes for macroblock modes (the seventh is three bits per mode).
const MODE_ALPHABET: [[u8; 8]; 6] = [
    [MODE_INTER_LAST_MV, MODE_INTER_PRIOR_LAST, MODE_INTER_PLUS_MV, MODE_INTER_NO_MV, MODE_INTRA, MODE_USING_GOLDEN, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
    [MODE_INTER_LAST_MV, MODE_INTER_PRIOR_LAST, MODE_INTER_NO_MV, MODE_INTER_PLUS_MV, MODE_INTRA, MODE_USING_GOLDEN, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
    [MODE_INTER_LAST_MV, MODE_INTER_PLUS_MV, MODE_INTER_PRIOR_LAST, MODE_INTER_NO_MV, MODE_INTRA, MODE_USING_GOLDEN, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
    [MODE_INTER_LAST_MV, MODE_INTER_PLUS_MV, MODE_INTER_NO_MV, MODE_INTER_PRIOR_LAST, MODE_INTRA, MODE_USING_GOLDEN, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
    [MODE_INTER_NO_MV, MODE_INTER_LAST_MV, MODE_INTER_PRIOR_LAST, MODE_INTER_PLUS_MV, MODE_INTRA, MODE_USING_GOLDEN, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
    [MODE_INTER_NO_MV, MODE_USING_GOLDEN, MODE_INTER_LAST_MV, MODE_INTER_PRIOR_LAST, MODE_INTER_PLUS_MV, MODE_INTRA, MODE_GOLDEN_MV, MODE_INTER_FOURMV],
];

/// The order of the 16 fragments of a superblock (a Hilbert curve), as (x, y).
const HILBERT: [(usize, usize); 16] =
    [(0, 0), (1, 0), (1, 1), (0, 1), (0, 2), (0, 3), (1, 3), (1, 2), (2, 2), (2, 3), (3, 3), (3, 2), (3, 1), (2, 1), (2, 0), (3, 0)];

#[rustfmt::skip]
const ZIGZAG: [u8; 64] = [
    0,   1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// Code lengths of the superblock run lengths 1 to 34 (34 is "34 plus 12 more bits").
#[rustfmt::skip]
const SUPERBLOCK_RUN_LENS: [u8; 34] = [
    1, 3, 3, 4, 4, 6, 6, 6, 6, 8, 8, 8, 8, 8, 8, 8, 8, 10, 10,
    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 6,
];
/// Code lengths of the fragment run lengths 1 to 30.
#[rustfmt::skip]
const FRAGMENT_RUN_LENS: [u8; 30] = [
    2, 2, 3, 3, 4, 4, 6, 6, 6, 6, 7, 7, 7, 7, 9, 9, 9, 9, 9,
    9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
];
/// Code lengths of the 8 macroblock mode ranks.
const MODE_LENS: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 7];
/// Motion vector codes in code order: (value plus 31, length).
#[rustfmt::skip]
const MV_CODES: [(u8, u8); 63] = [
    (3, 31), (3, 32), (3, 30), (4, 33), (4, 29), (4, 34), (4, 28), (6, 35), (6, 27), (6, 36), (6, 26), (6, 37), (6, 25),
    (6, 38), (6, 24), (7, 39), (7, 23), (7, 40), (7, 22), (7, 41), (7, 21), (7, 42), (7, 20), (7, 43), (7, 19), (7, 44),
    (7, 18), (7, 45), (7, 17), (7, 46), (7, 16), (8, 47), (8, 15), (8, 48), (8, 14), (8, 49), (8, 13), (8, 50), (8, 12),
    (8, 51), (8, 11), (8, 52), (8, 10), (8, 53), (8, 9), (8, 54), (8, 8), (8, 55), (8, 7), (8, 56), (8, 6), (8, 57),
    (8, 5), (8, 58), (8, 4), (8, 59), (8, 3), (8, 60), (8, 2), (8, 61), (8, 1), (8, 62), (8, 0),
];

/// What a token (7 and up) means: the least magnitude of its coefficient, the extra bits that give it
/// (the first of them its sign when there are two or more, or one for the constants of 13 and up), the
/// least zero run before the coefficient, and the extra bits of that run. Tokens 0 to 6 are EOB runs.
fn token_parts(token: usize) -> (i32, u32, i32, u32) {
    match token {
        7 => (0, 0, 0, 3),
        8 => (0, 0, 0, 6),
        9..=12 => (1 + ((token as i32 - 9) >> 1), 0, 0, 0),
        13..=16 => (token as i32 - 10, 1, 0, 0),
        17 => (7, 2, 0, 0),
        18 => (9, 3, 0, 0),
        19 => (13, 4, 0, 0),
        20 => (21, 5, 0, 0),
        21 => (37, 6, 0, 0),
        22 => (69, 10, 0, 0),
        23..=27 => (1, 1, token as i32 - 22, 0),
        28 => (1, 1, 6, 2),
        29 => (1, 1, 10, 3),
        30 => (2, 2, 1, 0),
        _ => (2, 2, 2, 1),
    }
}

/// Tokens 0 to 6 are EOB runs: their base length and extra bits.
const EOB_BASE: [i64; 7] = [1, 2, 3, 4, 8, 16, 0];
const EOB_BITS: [u32; 7] = [0, 0, 0, 2, 3, 4, 12];

const IDCT_C1S7: i32 = 64277;
const IDCT_C2S6: i32 = 60547;
const IDCT_C3S5: i32 = 54491;
const IDCT_C4S4: i32 = 46341;
const IDCT_C5S3: i32 = 36410;
const IDCT_C6S2: i32 = 25080;
const IDCT_C7S1: i32 = 12785;

/// Reads bits, most significant first. Past the end it reads zeros; `left` goes negative.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }

    fn left(&self) -> isize {
        self.d.len() as isize * 8 - self.pos as isize
    }

    /// The next `n` bits (at most 32) without consuming them.
    fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let v = match self.d.get(byte..byte + 8) {
            Some(s) => u64::from_be_bytes(s.try_into().unwrap()),
            None => (0..8).fold(0u64, |a, i| (a << 8) | self.d.get(byte + i).copied().unwrap_or(0) as u64),
        };
        ((v << (self.pos & 7)) >> (64 - n)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }

    fn bit(&mut self) -> u32 {
        self.get(1)
    }
}

/// A prefix code of at most 34 symbols with codes up to 32 bits: a table for the short codes and a
/// scan for the long ones.
struct Vlc {
    /// Indexed by the next `FAST` bits: `(length << 8) | symbol`, or 0 for a longer code.
    fast: Vec<u16>,
    /// The codes longer than `FAST` bits: (code left-aligned in 32 bits, length, symbol).
    long: Vec<(u32, u8, u8)>,
    /// The symbol of a code of no bits (a table with one entry).
    zero: Option<u8>,
}

const FAST: u32 = 10;

impl Vlc {
    /// Codes are handed out in the order given, each the next free code of its length, which for the
    /// leaves of a tree read depth first is the path to the leaf. Entries are (length, symbol).
    fn new(entries: &[(u8, u8)]) -> Result<Vlc> {
        let mut v = Vlc { fast: vec![0; 1 << FAST], long: Vec::new(), zero: None };
        let mut code = 0u64;
        for &(len, sym) in entries {
            if len == 0 {
                if entries.len() != 1 {
                    return Err(bad("a Huffman table has an empty code beside others"));
                }
                v.zero = Some(sym);
                continue;
            }
            if len > 32 || code >> 32 != 0 {
                return Err(bad("a Huffman table is not a prefix code"));
            }
            let lj = code as u32;
            if len as u32 <= FAST {
                let start = (lj >> (32 - FAST)) as usize;
                for e in &mut v.fast[start..start + (1 << (FAST - len as u32))] {
                    *e = ((len as u16) << 8) | sym as u16;
                }
            } else {
                v.long.push((lj, len, sym));
            }
            code += 1u64 << (32 - len);
        }
        Ok(v)
    }

    fn get(&self, b: &mut Bits) -> Option<u8> {
        if let Some(s) = self.zero {
            return Some(s);
        }
        let e = self.fast[b.peek(FAST) as usize];
        if e != 0 {
            b.skip((e >> 8) as u32);
            return Some(e as u8);
        }
        let p = b.peek(32);
        for &(code, len, sym) in &self.long {
            if (p ^ code) >> (32 - len as u32) == 0 {
                b.skip(len as u32);
                return Some(sym);
            }
        }
        None
    }
}

/// Split Xiph-laced codec data into the three header packets.
pub fn split_headers(extra: &[u8]) -> Result<[&[u8]; 3]> {
    if extra.first() != Some(&2) {
        return Err(bad("the codec data does not hold three laced headers"));
    }
    let mut at = 1;
    let mut size = [0usize; 2];
    for s in size.iter_mut() {
        loop {
            let b = *extra.get(at).ok_or_else(|| bad("the codec data is truncated"))?;
            at += 1;
            *s += b as usize;
            if b != 255 {
                break;
            }
        }
    }
    let end0 = at + size[0];
    let end1 = end0 + size[1];
    if end1 > extra.len() {
        return Err(bad("the codec data is truncated"));
    }
    Ok([&extra[at..end0], &extra[end0..end1], &extra[end1..]])
}

/// Join the three header packets into Xiph-laced codec data.
pub fn join_headers(h: [&[u8]; 3]) -> Vec<u8> {
    let mut v = vec![2u8];
    for p in &h[..2] {
        let mut n = p.len();
        while n >= 255 {
            v.push(255);
            n -= 255;
        }
        v.push(n as u8);
    }
    for p in h {
        v.extend_from_slice(p);
    }
    v
}

/// Whether `packet` is a key frame (a video packet whose frame type bit is 0).
pub fn is_key_frame(packet: &[u8]) -> bool {
    packet.first().is_some_and(|&b| b & 0xC0 == 0)
}

/// What the identification header says about the stream.
#[derive(Clone, Debug, PartialEq)]
pub struct TheoraInfo {
    /// The picture region, which is what is shown.
    pub width: u32,
    pub height: u32,
    /// The coded frame (whole macroblocks).
    pub coded_width: u32,
    pub coded_height: u32,
    /// The picture's offset from the left and from the bottom of the coded frame.
    pub offset_x: u32,
    pub offset_y: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// How many low bits of an Ogg granule position count frames since the last key frame.
    pub kfgshift: u8,
    pub pix_fmt: PixelFormat,
    /// The colour space code: 0 undefined, 1 Rec. 470M, 2 Rec. 470BG.
    pub colorspace: u8,
    /// The bitstream version, major, minor and revision in 8 bits each.
    pub version: u32,
}

impl TheoraInfo {
    /// Parse an identification header packet.
    pub fn parse(id: &[u8]) -> Result<TheoraInfo> {
        if id.len() < 42 || id[0] != 0x80 || &id[1..7] != b"theora" {
            return Err(bad("the identification header is missing or damaged"));
        }
        let mut b = Bits::new(&id[7..]);
        let version = b.get(24);
        if version >> 16 != 3 || (version >> 8) & 0xFF < 2 {
            return Err(Error::new(
                ErrorCode::UnsupportedCodec,
                format!("Theora: bitstream version {}.{}.{} is not supported (3.2 and later are)", version >> 16, (version >> 8) & 0xFF, version & 0xFF),
            ));
        }
        let (cw, ch) = (b.get(16) << 4, b.get(16) << 4);
        let (width, height) = (b.get(24), b.get(24));
        let (offset_x, offset_y) = (b.get(8), b.get(8));
        let (fps_num, fps_den) = (b.get(32), b.get(32));
        let _aspect = (b.get(24), b.get(24));
        let colorspace = b.get(8) as u8;
        let _bitrate = b.get(24);
        let _quality = b.get(6);
        let kfgshift = b.get(5) as u8;
        let pix_fmt = match b.get(2) {
            0 => PixelFormat::Yuv420p,
            2 => PixelFormat::Yuv422p,
            3 => PixelFormat::Yuv444p,
            _ => return Err(bad("the pixel format is reserved")),
        };
        if cw == 0 || ch == 0 || width == 0 || height == 0 || width + offset_x > cw || height + offset_y > ch || width + 512 < cw || height + 512 < ch {
            return Err(bad(format!("invalid frame size {width}x{height} at {offset_x},{offset_y} in {cw}x{ch}")));
        }
        if cw as u64 * ch as u64 > MAX_PIXELS {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("Theora: a {cw}x{ch} frame is larger than the {MAX_PIXELS} pixels that are decoded")));
        }
        Ok(TheoraInfo { width, height, coded_width: cw, coded_height: ch, offset_x, offset_y, fps_num, fps_den, kfgshift, pix_fmt, colorspace, version })
    }

    /// Parse the identification header out of Xiph-laced codec data.
    pub fn from_extradata(extra: &[u8]) -> Result<TheoraInfo> {
        Self::parse(split_headers(extra)?[0])
    }

    /// Chroma planes' horizontal and vertical shift.
    fn chroma_shift(&self) -> (usize, usize) {
        match self.pix_fmt {
            PixelFormat::Yuv420p => (1, 1),
            PixelFormat::Yuv422p => (1, 0),
            _ => (0, 0),
        }
    }
}

/// The tables of the setup header.
struct Setup {
    filter_limits: [u8; 64],
    ac_scale: [u32; 64],
    dc_scale: [u32; 64],
    base: Vec<[u8; 64]>,
    qr_count: [[usize; 3]; 2],
    qr_size: [[[u8; 64]; 3]; 2],
    qr_base: [[[u16; 64]; 3]; 2],
    /// The 80 token codes: 16 for DC, then 4 groups of 16 for AC.
    huff: Vec<Vlc>,
}

/// The number of bits of `v`.
fn ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

fn read_tree(b: &mut Bits, entries: &mut Vec<(u8, u8)>, length: u8) -> Result<()> {
    if b.bit() == 1 {
        if entries.len() >= 32 {
            return Err(bad("a Huffman tree has too many leaves"));
        }
        entries.push((length, b.get(5) as u8));
    } else {
        if length >= 31 {
            return Err(bad("a Huffman tree is too deep"));
        }
        read_tree(b, entries, length + 1)?;
        read_tree(b, entries, length + 1)?;
    }
    Ok(())
}

impl Setup {
    fn parse(setup: &[u8]) -> Result<Setup> {
        if setup.len() < 8 || setup[0] != 0x82 || &setup[1..7] != b"theora" {
            return Err(bad("the setup header is missing or damaged"));
        }
        let mut b = Bits::new(&setup[7..]);
        let mut s = Setup {
            filter_limits: [0; 64],
            ac_scale: [0; 64],
            dc_scale: [0; 64],
            base: Vec::new(),
            qr_count: [[0; 3]; 2],
            qr_size: [[[0; 64]; 3]; 2],
            qr_base: [[[0; 64]; 3]; 2],
            huff: Vec::new(),
        };
        let n = b.get(3);
        if n != 0 {
            for v in s.filter_limits.iter_mut() {
                *v = b.get(n) as u8;
            }
        }
        let n = b.get(4) + 1;
        for v in s.ac_scale.iter_mut() {
            *v = b.get(n);
        }
        let n = b.get(4) + 1;
        for v in s.dc_scale.iter_mut() {
            *v = b.get(n);
        }
        let matrices = b.get(9) as usize + 1;
        for _ in 0..matrices {
            let mut m = [0u8; 64];
            for v in m.iter_mut() {
                *v = b.get(8) as u8;
            }
            s.base.push(m);
        }
        if b.left() < 0 {
            return Err(bad("the setup header is truncated"));
        }
        for inter in 0..2 {
            for plane in 0..3 {
                let newqr = if inter > 0 || plane > 0 { b.bit() == 1 } else { true };
                if !newqr {
                    let (qtj, plj) = if inter == 1 && b.bit() == 1 { (0, plane) } else { ((3 * inter + plane - 1) / 3, (plane + 2) % 3) };
                    s.qr_count[inter][plane] = s.qr_count[qtj][plj];
                    s.qr_size[inter][plane] = s.qr_size[qtj][plj];
                    s.qr_base[inter][plane] = s.qr_base[qtj][plj];
                } else {
                    let (mut qri, mut qi) = (0usize, 0u32);
                    loop {
                        let i = b.get(ilog(matrices as u32 - 1)) as usize;
                        if i >= matrices {
                            return Err(bad("a quantiser range names a base matrix that does not exist"));
                        }
                        s.qr_base[inter][plane][qri] = i as u16;
                        if qi >= 63 {
                            break;
                        }
                        let size = b.get(ilog(63 - qi)) + 1;
                        s.qr_size[inter][plane][qri] = size as u8;
                        qri += 1;
                        qi += size;
                    }
                    if qi > 63 {
                        return Err(bad("the quantiser ranges run past 63"));
                    }
                    s.qr_count[inter][plane] = qri;
                }
            }
        }
        for _ in 0..80 {
            let mut entries = Vec::new();
            read_tree(&mut b, &mut entries, 0)?;
            s.huff.push(Vlc::new(&entries)?);
        }
        if b.left() < 0 {
            return Err(bad("the setup header is truncated"));
        }
        Ok(s)
    }
}

/// One decoded picture: the three planes in the order of decoding (bottom row first), as coded.
#[derive(Clone)]
struct Pic {
    planes: [Vec<u8>; 3],
}

/// Per-frame working state, kept to reuse its allocations.
#[derive(Default)]
struct Work {
    /// How each fragment is coded (a MODE_ value).
    coding: Vec<u8>,
    /// The DC coefficient of each fragment, before and after prediction.
    dc: Vec<i16>,
    /// Which quantiser of the frame each fragment uses.
    qpi: Vec<u8>,
    /// Superblock coding: 0 not coded, 1 partially, 2 fully.
    sb_coding: Vec<u8>,
    /// The mode of each macroblock.
    mb_coding: Vec<u8>,
    /// Motion vectors of the luma fragments, and of the chroma fragments.
    mv: [Vec<[i8; 2]>; 2],
    /// The coded fragments, the planes one after another, and where each plane starts.
    coded: Vec<u32>,
    plane_start: [usize; 4],
    /// The coefficients of each coded fragment (all but the DC), stored transposed as the IDCT reads them.
    coefs: Vec<i16>,
    /// The next coefficient each coded fragment takes a token for (64 when it is complete).
    next: Vec<u8>,
    /// Whether the fragment's first token was an end of block (it has no coefficients of its own).
    eob0: Vec<bool>,
}

impl Work {
    /// Size and clear the per-frame arrays.
    fn prepare(&mut self, frags: usize, sbs: usize, mbs: usize, luma_frags: usize, chroma_frags: usize) {
        self.coding.clear();
        self.coding.resize(frags, MODE_INTER_NO_MV);
        self.dc.clear();
        self.dc.resize(frags, 0);
        self.qpi.clear();
        self.qpi.resize(frags, 0);
        self.sb_coding.clear();
        self.sb_coding.resize(sbs, 0);
        self.mb_coding.clear();
        self.mb_coding.resize(mbs, MODE_COPY);
        self.mv[0].clear();
        self.mv[0].resize(luma_frags, [0; 2]);
        self.mv[1].clear();
        self.mv[1].resize(chroma_frags, [0; 2]);
    }
}

/// A Theora decoder.
pub struct TheoraDecoder {
    info: TheoraInfo,
    setup: Setup,
    /// Width and height of each plane in samples.
    pw: [usize; 3],
    ph: [usize; 3],
    /// Width and height in fragments of the luma and of the chroma planes.
    fw: [usize; 2],
    fh: [usize; 2],
    fstart: [usize; 3],
    frag_count: usize,
    shift: (usize, usize),
    /// Superblocks across and down in the luma plane and in a chroma plane.
    y_sb: (usize, usize),
    c_sb: (usize, usize),
    /// Macroblocks across and down.
    mb: (usize, usize),
    /// The fragments of each superblock (16 per superblock, in curve order; `u32::MAX` for none).
    sb_frags: Vec<u32>,
    sb_count: usize,
    mode_vlc: Vlc,
    sb_run_vlc: Vlc,
    frag_run_vlc: Vlc,
    mv_vlc: Vlc,
    last: Option<Pic>,
    golden: Option<Pic>,
    spare: Option<Pic>,
    work: Work,
    /// Dequantisers, per frame quantiser, inter flag and plane, in the order the IDCT reads them.
    qmat: [[[[i16; 64]; 3]; 2]; 3],
}

/// The transposed position of a coefficient index, which is how FFmpeg's IDCT reads its block.
fn transpose(i: usize) -> usize {
    (i >> 3) | ((i & 7) << 3)
}

fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn mul(a: i32, b: i32) -> i32 {
    ((a as u32).wrapping_mul(b as u32) as i32) >> 16
}

/// The inverse DCT of `block` (transposed storage), put or added to the 8x8 samples at `at`. The
/// block is cleared.
fn idct(block: &mut [i16; 64], out: &mut [u8], at: usize, stride: usize, put: bool) {
    for i in 0..8 {
        let ip = |k: usize| block[i + 8 * k] as i32;
        if (0..8).any(|k| ip(k) != 0) {
            let a = mul(IDCT_C1S7, ip(1)) + mul(IDCT_C7S1, ip(7));
            let b = mul(IDCT_C7S1, ip(1)) - mul(IDCT_C1S7, ip(7));
            let c = mul(IDCT_C3S5, ip(3)) + mul(IDCT_C5S3, ip(5));
            let d = mul(IDCT_C3S5, ip(5)) - mul(IDCT_C5S3, ip(3));
            let ad = mul(IDCT_C4S4, a - c);
            let bd = mul(IDCT_C4S4, b - d);
            let cd = a + c;
            let dd = b + d;
            let e = mul(IDCT_C4S4, ip(0) + ip(4));
            let f = mul(IDCT_C4S4, ip(0) - ip(4));
            let g = mul(IDCT_C2S6, ip(2)) + mul(IDCT_C6S2, ip(6));
            let h = mul(IDCT_C6S2, ip(2)) - mul(IDCT_C2S6, ip(6));
            let ed = e - g;
            let gd = e + g;
            let add = f + ad;
            let bdd = bd - h;
            let fd = f - ad;
            let hd = bd + h;
            block[i] = (gd + cd) as i16;
            block[i + 56] = (gd - cd) as i16;
            block[i + 8] = (add + hd) as i16;
            block[i + 16] = (add - hd) as i16;
            block[i + 24] = (ed + dd) as i16;
            block[i + 32] = (ed - dd) as i16;
            block[i + 40] = (fd + bdd) as i16;
            block[i + 48] = (fd - bdd) as i16;
        }
    }
    for i in 0..8 {
        let ip: [i32; 8] = std::array::from_fn(|k| block[8 * i + k] as i32);
        let col = at + i;
        if ip[1..].iter().any(|&v| v != 0) {
            let a = mul(IDCT_C1S7, ip[1]) + mul(IDCT_C7S1, ip[7]);
            let b = mul(IDCT_C7S1, ip[1]) - mul(IDCT_C1S7, ip[7]);
            let c = mul(IDCT_C3S5, ip[3]) + mul(IDCT_C5S3, ip[5]);
            let d = mul(IDCT_C3S5, ip[5]) - mul(IDCT_C5S3, ip[3]);
            let ad = mul(IDCT_C4S4, a - c);
            let bd = mul(IDCT_C4S4, b - d);
            let cd = a + c;
            let dd = b + d;
            let mut e = mul(IDCT_C4S4, ip[0] + ip[4]) + 8;
            let mut f = mul(IDCT_C4S4, ip[0] - ip[4]) + 8;
            if put {
                e += 16 * 128;
                f += 16 * 128;
            }
            let g = mul(IDCT_C2S6, ip[2]) + mul(IDCT_C6S2, ip[6]);
            let h = mul(IDCT_C6S2, ip[2]) - mul(IDCT_C2S6, ip[6]);
            let ed = e - g;
            let gd = e + g;
            let add = f + ad;
            let bdd = bd - h;
            let fd = f - ad;
            let hd = bd + h;
            let rows = [gd + cd, add + hd, add - hd, ed + dd, ed - dd, fd + bdd, fd - bdd, gd - cd];
            for (row, v) in rows.into_iter().enumerate() {
                let p = &mut out[col + row * stride];
                *p = if put { clip8(v >> 4) } else { clip8(*p as i32 + (v >> 4)) };
            }
        } else if put {
            let v = clip8(128 + ((IDCT_C4S4 * ip[0] + (8 << 16)) >> 20));
            for row in 0..8 {
                out[col + row * stride] = v;
            }
        } else if ip[0] != 0 {
            let v = (IDCT_C4S4 * ip[0] + (8 << 16)) >> 20;
            for row in 0..8 {
                let p = &mut out[col + row * stride];
                *p = clip8(*p as i32 + v);
            }
        }
    }
    block.fill(0);
}

/// Add a flat DC value to the 8x8 samples at `at`.
fn idct_dc_add(dc: i16, out: &mut [u8], at: usize, stride: usize) {
    let dc = (dc as i32 + 15) >> 5;
    for row in 0..8 {
        for p in &mut out[at + row * stride..at + row * stride + 8] {
            *p = clip8(*p as i32 + dc);
        }
    }
}

/// The loop filter's response to the sum of differences across an edge, for a limit; index 127 is zero.
fn bounding_values(limit: i32) -> [i32; 256] {
    let mut b = [0i32; 256];
    let at = |x: i32| (127 + x) as usize;
    for x in 0..limit {
        b[at(-x)] = -x;
        b[at(x)] = x;
    }
    let (mut x, mut value) = (limit, limit);
    while x < 128 && value != 0 {
        b[at(x)] = value;
        b[at(-x)] = -value;
        x += 1;
        value -= 1;
    }
    if value != 0 {
        b[at(128)] = value;
    }
    b
}

/// Filter across the horizontal edge above the row at `at`, along 8 columns.
fn loop_filter_v(p: &mut [u8], at: usize, stride: usize, bv: &[i32; 256]) {
    for i in at..at + 8 {
        let f = (p[i - 2 * stride] as i32 - p[i + stride] as i32) + (p[i] as i32 - p[i - stride] as i32) * 3;
        let f = bv[(((f + 4) >> 3) + 127) as usize];
        p[i - stride] = clip8(p[i - stride] as i32 + f);
        p[i] = clip8(p[i] as i32 - f);
    }
}

/// Filter across the vertical edge left of the column at `at`, along 8 rows.
fn loop_filter_h(p: &mut [u8], at: usize, stride: usize, bv: &[i32; 256]) {
    for r in 0..8 {
        let i = at + r * stride;
        let f = (p[i - 2] as i32 - p[i + 1] as i32) + (p[i] as i32 - p[i - 1] as i32) * 3;
        let f = bv[(((f + 4) >> 3) + 127) as usize];
        p[i - 1] = clip8(p[i - 1] as i32 + f);
        p[i] = clip8(p[i] as i32 - f);
    }
}

/// A block's place in a plane and the reference it predicts from.
struct Ref<'a> {
    src: &'a [u8],
    pw: usize,
    ph: usize,
    /// The whole-sample position of the block's top-left in the reference.
    sx: i32,
    sy: i32,
}

/// Predict an 8x8 block at `at` from a reference with a half-sample step in x and/or y. A half step in
/// both averages along the diagonal that the motion follows (`flip`: the other one). Samples outside
/// the reference are its nearest edge.
fn predict(dst: &mut [u8], at: usize, stride: usize, r: &Ref, hx: bool, hy: bool, flip: bool) {
    let mut tmp = [0u8; 81];
    let inside = r.sx >= 0 && r.sy >= 0 && r.sx as usize + 9 <= r.pw && r.sy as usize + 9 <= r.ph;
    if !inside {
        for row in 0..9 {
            let y = (r.sy + row as i32).clamp(0, r.ph as i32 - 1) as usize;
            for col in 0..9 {
                let x = (r.sx + col as i32).clamp(0, r.pw as i32 - 1) as usize;
                tmp[row * 9 + col] = r.src[y * r.pw + x];
            }
        }
    }
    let px = |row: usize, col: usize| -> i32 {
        if inside {
            r.src[(r.sy as usize + row) * r.pw + r.sx as usize + col] as i32
        } else {
            tmp[row * 9 + col] as i32
        }
    };
    for row in 0..8 {
        for col in 0..8 {
            let v = match (hx, hy) {
                (false, false) => px(row, col),
                (true, false) => (px(row, col) + px(row, col + 1)) >> 1,
                (false, true) => (px(row, col) + px(row + 1, col)) >> 1,
                (true, true) if flip => (px(row, col + 1) + px(row + 1, col)) >> 1,
                (true, true) => (px(row, col) + px(row + 1, col + 1)) >> 1,
            };
            dst[at + row * stride + col] = v as u8;
        }
    }
}

fn rshift(a: i32, b: u32) -> i32 {
    if a > 0 {
        (a + ((1 << b) >> 1)) >> b
    } else {
        (a + ((1 << b) >> 1) - 1) >> b
    }
}

impl TheoraDecoder {
    /// Build a decoder from a stream whose codec data is the three header packets in Xiph lacing.
    pub fn new(s: &StreamInfo) -> Result<TheoraDecoder> {
        let [id, _comments, setup] = split_headers(&s.extradata)?;
        Self::from_headers(id, setup)
    }

    /// Build a decoder from the identification and setup header packets.
    pub fn from_headers(id: &[u8], setup: &[u8]) -> Result<TheoraDecoder> {
        let info = TheoraInfo::parse(id)?;
        let setup = Setup::parse(setup)?;
        let shift = info.chroma_shift();
        let (w, h) = (info.coded_width as usize, info.coded_height as usize);
        let pw = [w, w >> shift.0, w >> shift.0];
        let ph = [h, h >> shift.1, h >> shift.1];
        let fw = [w / 8, (w / 8) >> shift.0];
        let fh = [h / 8, (h / 8) >> shift.1];
        let y_sb = (w.div_ceil(32), h.div_ceil(32));
        let c_sb = (pw[1].div_ceil(32), ph[1].div_ceil(32));
        let sb_count = y_sb.0 * y_sb.1 + 2 * c_sb.0 * c_sb.1;
        let fstart = [0, fw[0] * fh[0], fw[0] * fh[0] + fw[1] * fh[1]];
        let frag_count = fw[0] * fh[0] + 2 * fw[1] * fh[1];

        let mut sb_frags = Vec::with_capacity(sb_count * 16);
        for plane in 0..3 {
            let (sbw, sbh) = if plane == 0 { y_sb } else { c_sb };
            let (fwp, fhp) = (fw[(plane > 0) as usize], fh[(plane > 0) as usize]);
            for sb_y in 0..sbh {
                for sb_x in 0..sbw {
                    for &(hx, hy) in &HILBERT {
                        let (x, y) = (4 * sb_x + hx, 4 * sb_y + hy);
                        sb_frags.push(if x < fwp && y < fhp { (fstart[plane] + y * fwp + x) as u32 } else { u32::MAX });
                    }
                }
            }
        }
        let sb_run: Vec<(u8, u8)> = SUPERBLOCK_RUN_LENS.iter().enumerate().map(|(i, &l)| (l, i as u8 + 1)).collect();
        let frag_run: Vec<(u8, u8)> = FRAGMENT_RUN_LENS.iter().enumerate().map(|(i, &l)| (l, i as u8)).collect();
        let mode: Vec<(u8, u8)> = MODE_LENS.iter().enumerate().map(|(i, &l)| (l, i as u8)).collect();
        let mb = (w.div_ceil(16), h.div_ceil(16));
        Ok(TheoraDecoder {
            info,
            setup,
            pw,
            ph,
            fw,
            fh,
            fstart,
            frag_count,
            shift,
            y_sb,
            c_sb,
            mb,
            sb_frags,
            sb_count,
            mode_vlc: Vlc::new(&mode)?,
            sb_run_vlc: Vlc::new(&sb_run)?,
            frag_run_vlc: Vlc::new(&frag_run)?,
            mv_vlc: Vlc::new(&MV_CODES)?,
            last: None,
            golden: None,
            spare: None,
            work: Work::default(),
            qmat: [[[[0; 64]; 3]; 2]; 3],
        })
    }

    /// What the identification header says.
    pub fn info(&self) -> &TheoraInfo {
        &self.info
    }

    /// The dequantisers for the frame's quantisers (FFmpeg's `init_dequantizer`).
    fn init_dequant(&mut self, qps: &[usize]) {
        let s = &self.setup;
        for (qpi, &qp) in qps.iter().enumerate() {
            let ac = s.ac_scale[qp] as i32;
            for inter in 0..2 {
                for plane in 0..3 {
                    let dcs = s.dc_scale[qp] as i32;
                    let (mut sum, mut qri) = (0i32, 0usize);
                    while qri < s.qr_count[inter][plane] {
                        sum += s.qr_size[inter][plane][qri] as i32;
                        if qp as i32 <= sum {
                            break;
                        }
                        qri += 1;
                    }
                    let size = (s.qr_size[inter][plane][qri.min(63)] as i32).max(1);
                    let qistart = sum - size;
                    let bmi = &s.base[s.qr_base[inter][plane][qri.min(63)] as usize % s.base.len()];
                    let bmj = &s.base[s.qr_base[inter][plane][(qri + 1).min(63)] as usize % s.base.len()];
                    for i in 0..64 {
                        let coeff = (2 * (sum - qp as i32) * bmi[i] as i32 - 2 * (qistart - qp as i32) * bmj[i] as i32 + size) / (2 * size);
                        let qmin = 8 << (inter + (i == 0) as usize);
                        let qscale = if i == 0 { dcs } else { ac };
                        self.qmat[qpi][inter][plane][transpose(i)] = (qscale.wrapping_mul(coeff) / 100 * 4).clamp(qmin, 4096) as i16;
                    }
                    // all DC coefficients use the first quantiser's, so as not to disturb DC prediction
                    self.qmat[qpi][inter][plane][0] = self.qmat[0][inter][plane][0];
                }
            }
        }
    }

    fn new_pic(&mut self) -> Pic {
        match self.spare.take() {
            Some(p) => p,
            None => Pic { planes: [vec![0; self.pw[0] * self.ph[0]], vec![0; self.pw[1] * self.ph[1]], vec![0; self.pw[2] * self.ph[2]]] },
        }
    }

    /// Decode a frame packet (None when there is nothing to show yet).
    fn decode_frame(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
        let mut b = Bits::new(data);
        if b.bit() == 1 {
            return Ok(None); // a header packet in the middle of the stream
        }
        let key = b.bit() == 0;
        if !key && self.golden.is_none() {
            return Ok(None); // inter frames before the first key frame cannot be decoded
        }
        let mut qps = Vec::with_capacity(3);
        loop {
            qps.push(b.get(6) as usize);
            if qps.len() >= 3 || b.bit() == 0 {
                break;
            }
        }
        if key {
            if b.bit() != 0 {
                return Err(bad("unsupported key frame coding type"));
            }
            b.skip(2);
        }
        if b.left() < 0 {
            return Err(bad("the frame is truncated"));
        }
        self.init_dequant(&qps);
        self.work.prepare(self.frag_count, self.sb_count, self.mb.0 * self.mb.1, self.fw[0] * self.fh[0], self.fw[1] * self.fh[1]);
        self.unpack_superblocks(&mut b, key)?;
        self.unpack_modes(&mut b, key)?;
        self.unpack_vectors(&mut b, key)?;
        self.unpack_block_qpis(&mut b, qps.len())?;
        self.unpack_dct_coeffs(&mut b)?;

        let mut cur = self.new_pic();
        if let (false, Some(last)) = (key, &self.last) {
            for p in 0..3 {
                cur.planes[p].copy_from_slice(&last.planes[p]);
            }
        }
        self.reconstruct(&mut cur);
        let limit = self.setup.filter_limits[qps[0]] as i32;
        if limit > 0 {
            self.loop_filter(&mut cur, limit);
        }
        let out = self.output(&cur);
        if key {
            self.golden = Some(cur.clone());
        }
        self.spare = self.last.take();
        self.last = Some(cur);
        Ok(Some(out))
    }

    /// Which fragments are coded: from the superblock flags and run lengths.
    fn unpack_superblocks(&mut self, b: &mut Bits, key: bool) -> Result<()> {
        let sb_count = self.sb_count;
        let w = &mut self.work;
        let (y_count, c_count) = (self.y_sb.0 * self.y_sb.1, self.c_sb.0 * self.c_sb.1);
        let starts = [0, y_count, y_count + c_count, sb_count];
        let mut bit = 0u32;
        let mut current_run = 0i32;
        let mut partial = 0usize;
        if key {
            w.sb_coding.fill(2);
        } else {
            let mut cur = 0usize;
            bit = b.bit() ^ 1;
            while cur < sb_count && b.left() > 0 {
                if current_run == MAXIMUM_LONG_BIT_RUN {
                    bit = b.bit();
                } else {
                    bit ^= 1;
                }
                current_run = self.sb_run_vlc.get(b).ok_or_else(|| bad("a bad superblock run code"))? as i32;
                if current_run == 34 {
                    current_run += b.get(12) as i32;
                }
                if current_run as usize > sb_count - cur {
                    return Err(bad("a partially coded superblock run is too long"));
                }
                w.sb_coding[cur..cur + current_run as usize].fill(bit as u8);
                cur += current_run as usize;
                if bit != 0 {
                    partial += current_run as usize;
                }
            }
            if partial < sb_count {
                let mut decoded = 0usize;
                cur = 0;
                bit = b.bit() ^ 1;
                current_run = 0;
                while decoded < sb_count - partial && b.left() > 0 {
                    if current_run == MAXIMUM_LONG_BIT_RUN {
                        bit = b.bit();
                    } else {
                        bit ^= 1;
                    }
                    current_run = self.sb_run_vlc.get(b).ok_or_else(|| bad("a bad superblock run code"))? as i32;
                    if current_run == 34 {
                        current_run += b.get(12) as i32;
                    }
                    let mut j = 0;
                    while j < current_run {
                        if cur >= sb_count {
                            return Err(bad("a fully coded superblock run is too long"));
                        }
                        if w.sb_coding[cur] == 0 {
                            w.sb_coding[cur] = 2 * bit as u8;
                            j += 1;
                        }
                        cur += 1;
                    }
                    decoded += current_run as usize;
                }
            }
            if partial > 0 {
                // the fragment runs: the first run length read toggles the bit again
                current_run = 0;
                bit = b.bit() ^ 1;
            }
        }
        // the fragments, plane by plane, superblock by superblock
        w.coded.clear();
        w.coding.fill(MODE_COPY);
        for plane in 0..3 {
            w.plane_start[plane] = w.coded.len();
            for sb in starts[plane]..starts[plane + 1] {
                if !key && b.left() <= 0 {
                    break;
                }
                for j in 0..16 {
                    let f = self.sb_frags[sb * 16 + j];
                    if f == u32::MAX {
                        continue;
                    }
                    let mut coded = w.sb_coding[sb] != 0;
                    if !key && w.sb_coding[sb] == 1 {
                        if current_run == 0 {
                            bit ^= 1;
                            current_run = self.frag_run_vlc.get(b).ok_or_else(|| bad("a bad fragment run code"))? as i32;
                        } else {
                            current_run -= 1;
                        }
                        coded = bit != 0;
                    }
                    if coded {
                        w.coding[f as usize] = MODE_INTER_NO_MV;
                        w.coded.push(f);
                    }
                }
            }
        }
        w.plane_start[3] = w.coded.len();
        let n = w.coded.len();
        w.next.clear();
        w.next.resize(n, 0);
        w.eob0.clear();
        w.eob0.resize(n, false);
        Ok(())
    }

    /// The coding mode of each macroblock.
    fn unpack_modes(&mut self, b: &mut Bits, key: bool) -> Result<()> {
        let w = &mut self.work;
        if key {
            w.coding.fill(MODE_INTRA);
            return Ok(());
        }
        let scheme = b.get(3) as usize;
        let mut custom = [MODE_INTER_NO_MV; 8];
        if scheme == 0 {
            for i in 0..8u8 {
                custom[b.get(3) as usize] = i;
            }
        }
        let alphabet = match scheme {
            0 => custom,
            7 => [0; 8],
            _ => MODE_ALPHABET[scheme - 1],
        };
        let (fw0, fw1) = (self.fw[0], self.fw[1]);
        for sb_y in 0..self.y_sb.1 {
            for sb_x in 0..self.y_sb.0 {
                if b.left() <= 0 {
                    return Err(bad("the macroblock modes are truncated"));
                }
                for j in 0..4 {
                    let mb_x = 2 * sb_x + (j >> 1);
                    let mb_y = 2 * sb_y + (((j >> 1) + j) & 1);
                    if mb_x >= self.mb.0 || mb_y >= self.mb.1 {
                        continue;
                    }
                    let cur_mb = mb_y * self.mb.0 + mb_x;
                    let blocks = |k: usize| (2 * mb_y + (k >> 1)) * fw0 + 2 * mb_x + (k & 1);
                    // a macroblock with no luma fragment coded has no mode of its own
                    if (0..4).all(|k| w.coding[blocks(k)] == MODE_COPY) {
                        w.mb_coding[cur_mb] = MODE_INTER_NO_MV;
                        continue;
                    }
                    let mode = if scheme == 7 { b.get(3) as u8 } else { alphabet[self.mode_vlc.get(b).ok_or_else(|| bad("a bad mode code"))? as usize] };
                    w.mb_coding[cur_mb] = mode;
                    for k in 0..4 {
                        let f = blocks(k);
                        if w.coding[f] != MODE_COPY {
                            w.coding[f] = mode;
                        }
                    }
                    let mut set_chroma = |f: usize| {
                        for s in [self.fstart[1], self.fstart[2]] {
                            if w.coding[s + f] != MODE_COPY {
                                w.coding[s + f] = mode;
                            }
                        }
                    };
                    match self.shift {
                        (1, 1) => set_chroma(mb_y * fw1 + mb_x),
                        (1, 0) => {
                            for k in 0..2 {
                                set_chroma((2 * mb_y + k) * fw1 + mb_x);
                            }
                        }
                        _ => {
                            for k in 0..4 {
                                set_chroma((2 * mb_y + (k >> 1)) * fw1 + 2 * mb_x + (k & 1));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// One motion vector, in the scheme the frame chose.
    fn read_mv(&self, b: &mut Bits, fixed: bool) -> Result<(i32, i32)> {
        let one = |b: &mut Bits| -> Result<i32> {
            if fixed {
                let n = b.get(6) as i32;
                let m = n >> 1;
                Ok(if n & 1 == 1 { -m } else { m })
            } else {
                Ok(self.mv_vlc.get(b).ok_or_else(|| bad("a bad motion vector code"))? as i32 - 31)
            }
        };
        let x = one(b)?;
        let y = one(b)?;
        Ok((x, y))
    }

    /// The motion vectors of the macroblocks, as the vectors of their fragments.
    fn unpack_vectors(&mut self, b: &mut Bits, key: bool) -> Result<()> {
        if key {
            return Ok(());
        }
        let fixed = b.bit() == 1;
        let (mut last, mut prior) = ((0i32, 0i32), (0i32, 0i32));
        let (fw0, fw1) = (self.fw[0], self.fw[1]);
        for sb_y in 0..self.y_sb.1 {
            for sb_x in 0..self.y_sb.0 {
                if b.left() <= 0 {
                    return Err(bad("the motion vectors are truncated"));
                }
                for j in 0..4 {
                    let mb_x = 2 * sb_x + (j >> 1);
                    let mb_y = 2 * sb_y + (((j >> 1) + j) & 1);
                    if mb_x >= self.mb.0 || mb_y >= self.mb.1 {
                        continue;
                    }
                    let mode = self.work.mb_coding[mb_y * self.mb.0 + mb_x];
                    if mode == MODE_COPY {
                        continue;
                    }
                    let blocks = |k: usize| (2 * mb_y + (k >> 1)) * fw0 + 2 * mb_x + (k & 1);
                    let mut mv = [(0i32, 0i32); 4];
                    match mode {
                        MODE_GOLDEN_MV | MODE_INTER_PLUS_MV => {
                            mv[0] = self.read_mv(b, fixed)?;
                            if mode == MODE_INTER_PLUS_MV {
                                prior = last;
                                last = mv[0];
                            }
                        }
                        MODE_INTER_FOURMV => {
                            prior = last;
                            for (k, m) in mv.iter_mut().enumerate() {
                                if self.work.coding[blocks(k)] != MODE_COPY {
                                    *m = self.read_mv(b, fixed)?;
                                    last = *m;
                                }
                            }
                        }
                        MODE_INTER_LAST_MV => mv[0] = last,
                        MODE_INTER_PRIOR_LAST => {
                            mv[0] = prior;
                            prior = last;
                            last = mv[0];
                        }
                        _ => {}
                    }
                    let w = &mut self.work;
                    for k in 0..4 {
                        let m = if mode == MODE_INTER_FOURMV { mv[k] } else { mv[0] };
                        w.mv[0][blocks(k)] = [m.0 as i8, m.1 as i8];
                    }
                    match self.shift {
                        (1, 1) => {
                            let (mut x, mut y) = mv[0];
                            if mode == MODE_INTER_FOURMV {
                                x = rshift(mv[0].0 + mv[1].0 + mv[2].0 + mv[3].0, 2);
                                y = rshift(mv[0].1 + mv[1].1 + mv[2].1 + mv[3].1, 2);
                            }
                            x = (x >> 1) | (x & 1);
                            y = (y >> 1) | (y & 1);
                            w.mv[1][mb_y * fw1 + mb_x] = [x as i8, y as i8];
                        }
                        (1, 0) => {
                            let mut c = [mv[0], mv[0]];
                            if mode == MODE_INTER_FOURMV {
                                c[0] = (rshift(mv[0].0 + mv[1].0, 1), rshift(mv[0].1 + mv[1].1, 1));
                                c[1] = (rshift(mv[2].0 + mv[3].0, 1), rshift(mv[2].1 + mv[3].1, 1));
                            }
                            for (k, m) in c.iter().enumerate() {
                                let x = (m.0 >> 1) | (m.0 & 1);
                                w.mv[1][(2 * mb_y + k) * fw1 + mb_x] = [x as i8, m.1 as i8];
                            }
                        }
                        _ => {
                            for k in 0..4 {
                                let m = if mode == MODE_INTER_FOURMV { mv[k] } else { mv[0] };
                                w.mv[1][(2 * mb_y + (k >> 1)) * fw1 + 2 * mb_x + (k & 1)] = [m.0 as i8, m.1 as i8];
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Which of the frame's quantisers each coded fragment uses.
    fn unpack_block_qpis(&mut self, b: &mut Bits, nqps: usize) -> Result<()> {
        let w = &mut self.work;
        let total = w.coded.len();
        let mut num_blocks = total;
        let mut qpi = 0u8;
        while (qpi as usize) < nqps - 1 && num_blocks > 0 {
            let (mut i, mut decoded, mut at_qpi) = (0usize, 0usize, 0usize);
            let mut bit = b.bit() ^ 1;
            let mut run = 0i32;
            loop {
                if run == MAXIMUM_LONG_BIT_RUN {
                    bit = b.bit();
                } else {
                    bit ^= 1;
                }
                run = self.sb_run_vlc.get(b).ok_or_else(|| bad("a bad quantiser run code"))? as i32;
                if run == 34 {
                    run += b.get(12) as i32;
                }
                decoded += run as usize;
                if bit == 0 {
                    at_qpi += run as usize;
                }
                let mut j = 0;
                while j < run {
                    if i >= total {
                        return Err(bad("a quantiser run is too long"));
                    }
                    let f = w.coded[i] as usize;
                    if w.qpi[f] == qpi {
                        w.qpi[f] += bit as u8;
                        j += 1;
                    }
                    i += 1;
                }
                if !(decoded < num_blocks && b.left() > 0) {
                    break;
                }
            }
            num_blocks -= at_qpi;
            qpi += 1;
        }
        Ok(())
    }

    /// One token pass: the next token of each fragment in `alive` (the coded fragments of a plane that
    /// still take tokens) that takes one at `level`, with the Huffman table `vlc`. Returns the
    /// end-of-block run that is left over for the next pass.
    fn token_pass(&mut self, b: &mut Bits, vlc: usize, level: usize, mut eob_run: i64, alive: &mut Vec<u32>) -> Result<i64> {
        let w = &mut self.work;
        let huff = &self.setup.huff[vlc];
        let mut kept = 0usize;
        let mut out_of_bits = false;
        for ai in 0..alive.len() {
            let c = alive[ai] as usize;
            if w.next[c] as usize > level {
                // covered by a zero run
                alive[kept] = c as u32;
                kept += 1;
                continue;
            }
            if eob_run > 0 {
                eob_run -= 1;
                if level == 0 {
                    w.eob0[c] = true;
                }
                continue; // the fragment ends here
            }
            if out_of_bits || b.left() <= 0 {
                out_of_bits = true;
                continue;
            }
            let token = huff.get(b).ok_or_else(|| bad("a bad token code"))? as usize;
            if token <= 6 {
                let mut run = EOB_BASE[token];
                if EOB_BITS[token] > 0 {
                    run += b.get(EOB_BITS[token]) as i64;
                }
                if run == 0 {
                    run = i64::MAX;
                }
                eob_run = run - 1;
                if level == 0 {
                    w.eob0[c] = true;
                }
                continue;
            }
            let (base, bits, run_base, run_bits) = token_parts(token);
            let mut coeff = base;
            if bits > 0 {
                let v = b.get(bits);
                if token >= 13 {
                    coeff += (v & ((1 << (bits - 1)) - 1)) as i32;
                    if (v >> (bits - 1)) & 1 == 1 {
                        coeff = -coeff;
                    }
                }
            } else if token == 10 || token == 12 {
                coeff = -coeff;
            }
            let mut zero_run = run_base as usize;
            if run_bits > 0 {
                zero_run += b.get(run_bits) as usize;
            }
            if level == 0 && zero_run == 0 {
                let f = w.coded[c] as usize;
                w.dc[f] = coeff as i16;
            } else if level + zero_run < 64 {
                w.coefs[c * 64 + transpose(ZIGZAG[level + zero_run] as usize)] = coeff as i16;
            }
            w.next[c] = (level + zero_run + 1).min(64) as u8;
            if w.next[c] < 64 {
                alive[kept] = c as u32;
                kept += 1;
            }
        }
        alive.truncate(kept);
        Ok(eob_run)
    }

    /// The DCT coefficients of every coded fragment, then DC prediction.
    fn unpack_dct_coeffs(&mut self, b: &mut Bits) -> Result<()> {
        let n = self.work.coded.len();
        self.work.coefs.clear();
        self.work.coefs.resize(n * 64, 0);
        if b.left() < 16 {
            return Err(bad("the frame ends before its coefficients"));
        }
        let dc_y = b.get(4) as usize;
        let dc_c = b.get(4) as usize;
        let ps = self.work.plane_start;
        let mut lists: [Vec<u32>; 3] = std::array::from_fn(|p| (ps[p] as u32..ps[p + 1] as u32).collect());
        let mut eob_run = 0i64;
        for p in 0..3 {
            eob_run = self.token_pass(b, if p == 0 { dc_y } else { dc_c }, 0, eob_run, &mut lists[p])?;
        }
        for p in 0..3 {
            self.reverse_dc_prediction(p);
        }
        if b.left() < 8 {
            return Err(bad("the frame ends before its AC tables"));
        }
        let ac_y = b.get(4) as usize;
        let ac_c = b.get(4) as usize;
        for level in 1..64 {
            let group = match level {
                1..=5 => 16,
                6..=14 => 32,
                15..=27 => 48,
                _ => 64,
            };
            for p in 0..3 {
                eob_run = self.token_pass(b, group + if p == 0 { ac_y } else { ac_c }, level, eob_run, &mut lists[p])?;
            }
        }
        Ok(())
    }

    /// Add each coded fragment's prediction from its neighbours to its DC coefficient.
    fn reverse_dc_prediction(&mut self, plane: usize) {
        const PUL: usize = 8;
        const PU: usize = 4;
        const PUR: usize = 2;
        const PL: usize = 1;
        // up-left, up, up-right and left multipliers, by which neighbours predict
        const TRANSFORM: [[i32; 4]; 16] = [
            [0, 0, 0, 0],
            [0, 0, 0, 128],
            [0, 0, 128, 0],
            [0, 0, 53, 75],
            [0, 128, 0, 0],
            [0, 64, 0, 64],
            [0, 128, 0, 0],
            [0, 0, 53, 75],
            [128, 0, 0, 0],
            [0, 0, 0, 128],
            [64, 0, 64, 0],
            [0, 0, 53, 75],
            [0, 128, 0, 0],
            [-104, 116, 0, 116],
            [24, 80, 24, 0],
            [-104, 116, 0, 116],
        ];
        // the frame a mode predicts from: only blocks of the same kind predict each other
        const COMPATIBLE: [u8; 9] = [1, 0, 1, 1, 1, 2, 2, 1, 3];
        let (fw, fh) = (self.fw[(plane > 0) as usize], self.fh[(plane > 0) as usize]);
        let w = &mut self.work;
        let mut last_dc = [0i16; 3];
        let (mut vul, mut vu, mut vur, mut vl) = (0i32, 0i32, 0i32, 0i32);
        let mut i = self.fstart[plane];
        for y in 0..fh {
            for x in 0..fw {
                if w.coding[i] != MODE_COPY {
                    let kind = COMPATIBLE[w.coding[i] as usize];
                    let compat = |f: usize| COMPATIBLE[w.coding[f] as usize] == kind;
                    let mut transform = 0usize;
                    if x > 0 {
                        vl = w.dc[i - 1] as i32;
                        if compat(i - 1) {
                            transform |= PL;
                        }
                    }
                    if y > 0 {
                        vu = w.dc[i - fw] as i32;
                        if compat(i - fw) {
                            transform |= PU;
                        }
                        if x > 0 {
                            vul = w.dc[i - fw - 1] as i32;
                            if compat(i - fw - 1) {
                                transform |= PUL;
                            }
                        }
                        if x + 1 < fw {
                            vur = w.dc[i - fw + 1] as i32;
                            if compat(i - fw + 1) {
                                transform |= PUR;
                            }
                        }
                    }
                    let predicted = if transform == 0 {
                        last_dc[kind as usize] as i32
                    } else {
                        let t = &TRANSFORM[transform];
                        let mut p = (t[0] * vul + t[1] * vu + t[2] * vur + t[3] * vl) / 128;
                        if transform == 15 || transform == 13 {
                            if (p - vu).abs() > 128 {
                                p = vu;
                            } else if (p - vl).abs() > 128 {
                                p = vl;
                            } else if (p - vul).abs() > 128 {
                                p = vul;
                            }
                        }
                        p
                    };
                    w.dc[i] = w.dc[i].wrapping_add(predicted as i16);
                    last_dc[kind as usize] = w.dc[i];
                }
                i += 1;
            }
        }
    }

    /// Predict and transform every coded fragment into `cur`.
    fn reconstruct(&self, cur: &mut Pic) {
        let w = &self.work;
        let mut block = [0i16; 64];
        for plane in 0..3 {
            let (pw, ph) = (self.pw[plane], self.ph[plane]);
            let fw = self.fw[(plane > 0) as usize];
            for c in w.plane_start[plane]..w.plane_start[plane + 1] {
                let f = w.coded[c] as usize;
                let local = f - self.fstart[plane];
                let (x, y) = (local % fw, local / fw);
                let at = 8 * y * pw + 8 * x;
                let mode = w.coding[f];
                let intra = mode == MODE_INTRA;
                let inter = !intra as usize;
                let dq = &self.qmat[w.qpi[f] as usize][inter][plane];
                if !intra {
                    let golden = mode == MODE_USING_GOLDEN || mode == MODE_GOLDEN_MV;
                    if let Some(src) = if golden { self.golden.as_ref() } else { self.last.as_ref() } {
                        let (mut mx, mut my) = (0i32, 0i32);
                        if mode > MODE_INTRA && mode != MODE_USING_GOLDEN {
                            let m = w.mv[(plane > 0) as usize][local];
                            mx = m[0] as i32;
                            my = m[1] as i32;
                        }
                        let r = Ref { src: &src.planes[plane], pw, ph, sx: (mx >> 1) + 8 * x as i32, sy: (my >> 1) + 8 * y as i32 };
                        predict(&mut cur.planes[plane], at, pw, &r, mx & 1 == 1, my & 1 == 1, (mx ^ my) < 0);
                    }
                }
                for pos in 1..64 {
                    let v = w.coefs[c * 64 + pos];
                    if v != 0 {
                        block[pos] = (v as i32).wrapping_mul(dq[pos] as i32) as i16;
                    }
                }
                // the DC is the predicted one, with the first quantiser's step
                block[0] = (w.dc[f] as i32).wrapping_mul(self.qmat[0][inter][plane][0] as i32) as i16;
                if intra {
                    idct(&mut block, &mut cur.planes[plane], at, pw, true);
                } else if w.eob0[c] {
                    idct_dc_add(block[0], &mut cur.planes[plane], at, pw);
                    block[0] = 0;
                } else {
                    idct(&mut block, &mut cur.planes[plane], at, pw, false);
                }
            }
        }
    }

    /// Smooth the edges of the coded fragments (FFmpeg's `apply_loop_filter`), plane by plane.
    fn loop_filter(&self, cur: &mut Pic, limit: i32) {
        let bv = bounding_values(limit);
        for plane in 0..3 {
            let (fw, fh) = (self.fw[(plane > 0) as usize], self.fh[(plane > 0) as usize]);
            let stride = self.pw[plane];
            let first = self.fstart[plane];
            let p = &mut cur.planes[plane];
            let coding = &self.work.coding;
            for y in 0..fh {
                for x in 0..fw {
                    let f = first + y * fw + x;
                    if coding[f] == MODE_COPY {
                        continue;
                    }
                    let at = 8 * y * stride + 8 * x;
                    if x > 0 {
                        loop_filter_h(p, at, stride, &bv);
                    }
                    if y > 0 {
                        loop_filter_v(p, at, stride, &bv);
                    }
                    // the right and bottom edges, when the neighbour is not coded and will not filter them
                    if x + 1 < fw && coding[f + 1] == MODE_COPY {
                        loop_filter_h(p, at + 8, stride, &bv);
                    }
                    if y + 1 < fh && coding[f + fw] == MODE_COPY {
                        loop_filter_v(p, at + 8 * stride, stride, &bv);
                    }
                }
            }
        }
    }

    /// The picture region of a decoded frame, flipped to top-down, as a video frame.
    fn output(&self, pic: &Pic) -> VideoFrame {
        let i = &self.info;
        let (vw, vh) = (i.width as usize, i.height as usize);
        let crop_left = i.offset_x as usize;
        let crop_top = i.coded_height as usize - vh - i.offset_y as usize;
        let layout = i.pix_fmt.planes(vw, vh);
        let mut planes = Vec::with_capacity(3);
        for (p, &(w, h, _)) in layout.iter().enumerate() {
            let (sx, sy) = if p == 0 { (0, 0) } else { self.shift };
            let (x0, y0) = (crop_left >> sx, crop_top >> sy);
            let pw = self.pw[p];
            let mut out = Vec::with_capacity(w * h);
            for row in 0..h {
                // row `y0 + row` counted from the top is that far from the end of the decoded plane
                let src_row = self.ph[p].saturating_sub(1 + y0 + row);
                let start = src_row * pw + x0;
                match pic.planes[p].get(start..start + w) {
                    Some(s) => out.extend_from_slice(s),
                    None => out.resize(out.len() + w, 128),
                }
            }
            planes.push(out);
        }
        VideoFrame { width: vw as u32, height: vh as u32, pix_fmt: i.pix_fmt, pts: 0, planes }
    }
}

impl Decoder for TheoraDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        if packet.data.is_empty() {
            // an empty packet is a dropped frame: nothing new to show (the last picture stays up, as FFmpeg has it)
            return Ok(Vec::new());
        }
        Ok(match self.decode_frame(&packet.data)? {
            Some(mut v) => {
                v.pts = packet.pts;
                vec![Frame::Video(v)]
            }
            None => Vec::new(),
        })
    }

    fn reset(&mut self) -> bool {
        self.golden = None;
        self.spare = self.last.take().or(self.spare.take());
        true
    }

    fn color_info(&self) -> Option<ColorInfo> {
        match self.info.colorspace {
            1 => Some(ColorInfo { primaries: 4, transfer: 1, matrix: 5, full_range: false }),
            2 => Some(ColorInfo { primaries: 5, transfer: 1, matrix: 5, full_range: false }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
