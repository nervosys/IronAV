//! WavPack (version 4 and 5, `wvpk` blocks): lossless integer and float audio, and the lossy part
//! of hybrid files. Output is interleaved f64 in [-1, 1], scaled like the other lossless codecs here
//! (a 16-bit sample is divided by 2^15, a 24-bit one by 2^23 and a 32-bit one by 2^31); float files
//! give their f32 samples unchanged.
//!
//! Decoded: the block header, the metadata sub-blocks (decorrelation terms, weights and samples,
//! entropy variables, hybrid parameters, int32 and float information, extra bits, channel
//! information), the adaptive entropy coder with its median tracking and zero-run mode, every
//! decorrelation term (the history terms 1 to 8 and 17 and 18, and the cross-channel terms -1 to
//! -3), joint stereo, mono and false stereo, 8, 16, 24 and 32-bit integers and 32-bit float,
//! shifted and "int32" data, and several blocks per frame for more than two channels. The CRC of the
//! samples is checked, and a block that fails it is an error. Blocks are independent of each other
//! (each carries its own predictor state), so decoding can start at any frame.
//!
//! Hybrid (lossy) blocks are decoded without a correction file, as FFmpeg does: the lossy samples
//! are what comes out. A block whose error limits are ever nonzero is lossy, and its CRC is not
//! enforced (FFmpeg checks it only on request, and a lossy block's may not match what is decoded
//! without the correction data); a hybrid block whose limits are all zero is lossless and checked
//! like any other. The "wvx" extra
//! bits of a block, when it has them in its own sub-blocks, are read. WavPack's correction data
//! (the `.wvc` file) is not read. Not decoded: DSD audio (WavPack 5), which is refused.
//!
//! A packet is a frame: its blocks one after another, each with its 32-byte header, as FFmpeg's
//! `wv` demuxer gives them. Matroska's form (the sample count once, then for each block its flags,
//! CRC, and, with more than one block, its length, and the block without its header) is accepted
//! as well and rebuilt into that.
//!
//! The decoding follows FFmpeg's `libavcodec/wavpack.c`, `wavpack.h` and `wavpackdata.c`, which are
//! licensed under the LGPL-2.1 or later (Copyright (c) 2006, 2011 Konstantin Shishkov, 2020 David
//! Bryant).

mod tables;

use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamInfo, StreamParams};
use std::borrow::Cow;
use tables::{EXP2, LOG2};

const MAX_TERMS: usize = 16;
const MAX_TERM: usize = 8;
/// The size of a block's header.
pub const HEADER_SIZE: usize = 32;
/// The largest block the format allows (and FFmpeg's demuxer accepts).
pub const BLOCK_LIMIT: u32 = 1 << 20;
const MAX_SAMPLES: usize = 150_000;
/// FFmpeg's input padding, which its float extra-bits check counts as readable.
const PADDING_BITS: i64 = 8 * 64;

const WV_MONO: u32 = 0x4;
const WV_JOINT_STEREO: u32 = 0x10;
const WV_FLOAT_DATA: u32 = 0x80;
const WV_FALSE_STEREO: u32 = 0x4000_0000;
const WV_DSD_DATA: u32 = 0x8000_0000;
const WV_HYBRID_MODE: u32 = 0x8;
const WV_HYBRID_BITRATE: u32 = 0x200;
const WV_INITIAL_BLOCK: u32 = 0x800;
const WV_FINAL_BLOCK: u32 = 0x1000;
const WV_SINGLE_BLOCK: u32 = WV_INITIAL_BLOCK | WV_FINAL_BLOCK;

const WV_FLT_SHIFT_ONES: u8 = 0x01;
const WV_FLT_SHIFT_SAME: u8 = 0x02;
const WV_FLT_SHIFT_SENT: u8 = 0x04;
const WV_FLT_ZERO_SENT: u8 = 0x08;
const WV_FLT_ZERO_SIGN: u8 = 0x10;

const ID_IDF_MASK: u8 = 0x3F;
const ID_IDF_ODD: u8 = 0x40;
const ID_IDF_LONG: u8 = 0x80;
const ID_DECTERMS: u8 = 2;
const ID_DECWEIGHTS: u8 = 3;
const ID_DECSAMPLES: u8 = 4;
const ID_ENTROPY: u8 = 5;
const ID_HYBRID: u8 = 6;
const ID_FLOATINFO: u8 = 8;
const ID_INT32INFO: u8 = 9;
const ID_DATA: u8 = 10;
const ID_EXTRABITS: u8 = 12;
const ID_CHANINFO: u8 = 13;
const ID_DSD_DATA: u8 = 14;
const ID_SAMPLE_RATE: u8 = 0x27;

/// The sample rates a block's flags can name; the last (index 15) means a custom rate in a
/// sub-block.
pub const RATES: [u32; 16] = [6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000, 192000, 0];

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("WavPack: {msg}"))
}

fn le16(d: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([d[at], d[at + 1]]) as u32
}

fn le32(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// The 32-byte header that starts every block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// The length of the block after its first 8 bytes (ckSize): the rest of the header and the data.
    pub size: u32,
    pub version: u16,
    /// The samples in the whole stream, or 0xFFFFFFFF when unknown.
    pub total_samples: u32,
    /// The index of the first sample of this block.
    pub block_index: u32,
    /// The samples (per channel) in this block.
    pub samples: u32,
    pub flags: u32,
    pub crc: u32,
}

impl BlockHeader {
    /// Reads a header from the start of `d` if it is a plausible one: the `wvpk` marker, a size of
    /// 24 bytes to 1 MiB and a version from 4.02 to 4.10 (as FFmpeg's probe asks).
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < HEADER_SIZE || &d[..4] != b"wvpk" {
            return None;
        }
        let size = le32(d, 4);
        let version = le16(d, 8) as u16;
        if !(24..=BLOCK_LIMIT).contains(&size) || !(0x402..=0x410).contains(&version) {
            return None;
        }
        Some(Self { size, version, total_samples: le32(d, 12), block_index: le32(d, 16), samples: le32(d, 20), flags: le32(d, 24), crc: le32(d, 28) })
    }

    /// The length of the block's data after the 32-byte header.
    pub fn body_len(&self) -> usize {
        self.size as usize - 24
    }

    /// Whether this is the first block of a frame.
    pub fn initial(&self) -> bool {
        self.flags & WV_INITIAL_BLOCK != 0
    }

    /// Whether this is the last block of a frame.
    pub fn is_final(&self) -> bool {
        self.flags & WV_FINAL_BLOCK != 0
    }

    pub fn mono(&self) -> bool {
        self.flags & WV_MONO != 0
    }

    pub fn dsd(&self) -> bool {
        self.flags & WV_DSD_DATA != 0
    }

    /// The bits per sample of the original audio (8, 16, 24 or 32).
    pub fn bits(&self) -> u8 {
        (((self.flags & 3) + 1) << 3) as u8
    }

    /// The sample rate the flags name, or 0 for a custom rate.
    pub fn rate(&self) -> u32 {
        RATES[((self.flags >> 23) & 0xF) as usize]
    }
}

/// What a block's metadata says about the stream: the custom sample rate and the channel count and
/// mask of a frame of several blocks. Each is zero when the block does not say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamHints {
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_mask: u32,
    /// The block holds DSD data.
    pub dsd: bool,
}

/// Reads the sub-blocks of `body` (the data after a block's header) for the stream's parameters.
/// A damaged sub-block ends the search.
pub fn stream_hints(body: &[u8]) -> StreamHints {
    let mut h = StreamHints::default();
    let mut p = 0usize;
    while p + 2 <= body.len() {
        let id = body[p];
        let mut size = body[p + 1] as usize;
        p += 2;
        if id & ID_IDF_LONG != 0 {
            let Some(b) = body.get(p..p + 2) else { break };
            size |= (u16::from_le_bytes([b[0], b[1]]) as usize) << 8;
            p += 2;
        }
        let padded = size * 2;
        let size = if id & ID_IDF_ODD != 0 { (padded).saturating_sub(1) } else { padded };
        let Some(sub) = body.get(p..p + padded).map(|s| &s[..size.min(padded)]) else { break };
        match id & ID_IDF_MASK {
            ID_CHANINFO if sub.len() >= 2 => {
                let mut chan = sub[0] as u32;
                let le = |b: &[u8]| b.iter().rev().fold(0u32, |a, &x| (a << 8) | x as u32);
                match sub.len() - 2 {
                    n @ 0..=3 => h.channel_mask = le(&sub[1..2 + n]),
                    n @ 4..=5 => {
                        chan |= (sub[2] as u32 & 0xF) << 8;
                        chan += 1;
                        h.channel_mask = le(&sub[3..2 + n]);
                    }
                    _ => {}
                }
                h.channels = chan;
            }
            ID_DSD_DATA => h.dsd = true,
            ID_SAMPLE_RATE if sub.len() == 3 => h.sample_rate = le32(&[sub[0], sub[1], sub[2], 0], 0),
            _ => {}
        }
        p += padded;
    }
    h
}

/// The bits of a block's data, least significant first, as FFmpeg's little-endian reader gives
/// them: past the end every bit is zero (and `left` goes negative).
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The bits not yet read (negative after reading past the end).
    fn left(&self) -> i64 {
        (self.data.len() * 8) as i64 - self.pos as i64
    }

    /// 64 bits from the current position (zeros past the end), starting at bit `pos & 7` of the word.
    fn window(&self) -> u64 {
        let at = self.pos >> 3;
        let mut w = [0u8; 8];
        if at < self.data.len() {
            let n = (self.data.len() - at).min(8);
            w[..n].copy_from_slice(&self.data[at..at + n]);
        }
        u64::from_le_bytes(w) >> (self.pos & 7)
    }

    fn bit(&mut self) -> u32 {
        let b = (self.window() & 1) as u32;
        self.pos += 1;
        b
    }

    /// `n` bits, 0 to 32.
    fn bits(&mut self, n: u32) -> u32 {
        let v = (self.window() & ((1u64 << n) - 1)) as u32;
        self.pos += n as usize;
        v
    }

    /// The number of one bits before a zero, at most 33 (the 33 ones are read, not the zero after
    /// them): FFmpeg's `get_unary_0_33`.
    fn unary_0_33(&mut self) -> u32 {
        let ones = (!self.window()).trailing_zeros().min(33);
        self.pos += if ones >= 33 { 33 } else { ones as usize + 1 };
        ones
    }
}

/// A reader of a sub-block's bytes; past the end it gives zeros, as FFmpeg's byte stream reader does.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos = (self.pos + 1).min(self.data.len());
        v
    }

    fn u16(&mut self) -> u32 {
        let lo = self.u8() as u32;
        lo | (self.u8() as u32) << 8
    }
}

/// The exponential WavPack uses for logs stored in 16 bits.
fn wp_exp2(val: i16) -> i32 {
    let neg = val < 0;
    let v = if neg { val.wrapping_neg() } else { val };
    let res = EXP2[(v & 0xFF) as usize] as i32 | 0x100;
    let e = (v >> 8) as i32;
    if e as u32 > 31 {
        return i32::MIN;
    }
    let res = if e > 9 { res << (e - 9) } else { res >> (9 - e) };
    if neg {
        res.wrapping_neg()
    } else {
        res
    }
}

fn wp_log2(val: u32) -> i32 {
    if val == 0 {
        return 0;
    }
    if val == 1 {
        return 256;
    }
    let val = val.wrapping_add(val >> 9);
    let bits = if val == 0 { 1 } else { 32 - val.leading_zeros() as i32 };
    if bits < 9 {
        (bits << 8) + LOG2[((val << (9 - bits)) & 0xFF) as usize] as i32
    } else {
        (bits << 8) + LOG2[((val >> (bits - 9)) & 0xFF) as usize] as i32
    }
}

fn level_decay(a: i32) -> i32 {
    a.wrapping_add(0x80) >> 8
}

fn get_med(m: &[i32; 3], n: usize) -> i32 {
    (m[n] >> 4) + 1
}

fn dec_med(m: &mut [i32; 3], n: usize) {
    let d = 128i32 >> n;
    m[n] = m[n].wrapping_sub((m[n].wrapping_add(d).wrapping_sub(2) / d).wrapping_mul(2));
}

fn inc_med(m: &mut [i32; 3], n: usize) {
    let d = 128i32 >> n;
    m[n] = m[n].wrapping_add((m[n].wrapping_add(d) / d).wrapping_mul(5));
}

/// The bits after a unary prefix when the range `k` is not a power of two.
fn get_tail(gb: &mut Bits, k: u32) -> u32 {
    if k < 1 {
        return 0;
    }
    let p = 31 - k.leading_zeros();
    let e = (1i64 << (p + 1)) - k as i64 - 1;
    let mut res = gb.bits(p);
    if res as i64 >= e {
        res = res.wrapping_mul(2).wrapping_sub(e as u32).wrapping_add(gb.bit());
    }
    res
}

/// Moves a weight toward the sign of `samples` times `input`, within +-1024.
fn update_weight_clip(weight: &mut i32, delta: i32, samples: i32, input: i32) {
    if samples != 0 && input != 0 {
        if (samples ^ input) < 0 {
            *weight = weight.wrapping_sub(delta);
            if *weight < -1024 {
                *weight = -1024;
            }
        } else {
            *weight = weight.wrapping_add(delta);
            if *weight > 1024 {
                *weight = 1024;
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Decorr {
    delta: i32,
    value: i32,
    weight_a: i32,
    weight_b: i32,
    samples_a: [i32; MAX_TERM],
    samples_b: [i32; MAX_TERM],
}

#[derive(Clone, Copy, Default)]
struct Channel {
    median: [i32; 3],
    slow_level: i32,
    error_limit: i32,
    bitrate_acc: u32,
    bitrate_delta: u32,
}

/// How the samples of a block are held, which sets the arithmetic of the predictors.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 8 and 16-bit integers (FFmpeg's `s16p`: 32-bit wrapping predictors, and a size check)
    S16,
    /// 24 and 32-bit integers
    S32,
    Float,
}

/// One block being decoded.
struct Block<'a> {
    samples: usize,
    crc: u32,
    stereo: bool,
    stereo_in: bool,
    joint: bool,
    hybrid: bool,
    hybrid_bitrate: bool,
    post_shift: u32,
    hybrid_maxclip: i32,
    hybrid_minclip: i32,
    terms: usize,
    decorr: [Decorr; MAX_TERMS],
    zero: bool,
    one: bool,
    zeroes: i32,
    extra_bits: u32,
    and: u32,
    or: u32,
    shift: u32,
    float_flag: u8,
    float_shift: u32,
    float_max_exp: i32,
    ch: [Channel; 2],
    gb: Bits<'a>,
    got_extra_bits: bool,
    crc_extra_bits: u32,
    gb_extra: Bits<'a>,
    kind: Kind,
    /// What turns an output integer into [-1, 1].
    scale: f64,
    /// An error limit was nonzero: the samples are lossy and the CRC is not enforced.
    lossy: bool,
}

impl<'a> Block<'a> {
    /// Sets the channel's error limit from its rate and slow level (FFmpeg's `update_error_limit`).
    /// False if the rate accumulator overflows.
    fn update_error_limit(&mut self) -> bool {
        let n = self.stereo_in as usize;
        let mut br = [0i32; 2];
        let mut sl = [0i32; 2];
        for i in 0..=n {
            let c = &mut self.ch[i];
            if c.bitrate_acc > u32::MAX - c.bitrate_delta {
                return false;
            }
            c.bitrate_acc += c.bitrate_delta;
            br[i] = (c.bitrate_acc >> 16) as i32;
            sl[i] = level_decay(c.slow_level);
        }
        if n == 1 && self.hybrid_bitrate {
            let balance = sl[1].wrapping_sub(sl[0]).wrapping_add(br[1]).wrapping_add(1) >> 1;
            if balance > br[0] {
                br[1] = br[0].wrapping_mul(2);
                br[0] = 0;
            } else if balance.wrapping_neg() > br[0] {
                br[0] = br[0].wrapping_mul(2);
                br[1] = 0;
            } else {
                br[1] = br[0].wrapping_add(balance);
                br[0] = br[0].wrapping_sub(balance);
            }
        }
        for i in 0..=n {
            self.ch[i].error_limit = if self.hybrid_bitrate {
                let d = sl[i].wrapping_sub(br[i]);
                if d > -0x100 {
                    wp_exp2(d.wrapping_add(0x100) as i16)
                } else {
                    0
                }
            } else {
                wp_exp2(br[i] as i16)
            };
            self.lossy |= self.ch[i].error_limit != 0;
        }
        true
    }

    /// The next coded value of `channel`, or `None` when the data has run out or is damaged
    /// (FFmpeg's `wv_get_value` with `last` set).
    fn get_value(&mut self, channel: usize) -> Option<i32> {
        if (self.ch[0].median[0] as u32) < 2 && (self.ch[1].median[0] as u32) < 2 && !self.zero && !self.one {
            if self.zeroes != 0 {
                self.zeroes -= 1;
                if self.zeroes != 0 {
                    let c = &mut self.ch[channel];
                    c.slow_level = c.slow_level.wrapping_sub(level_decay(c.slow_level));
                    return Some(0);
                }
            } else {
                let mut t = self.gb.unary_0_33() as i32;
                if t >= 2 {
                    if t >= 32 || self.gb.left() < t as i64 - 1 {
                        return None;
                    }
                    t = (self.gb.bits(t as u32 - 1) | (1u32 << (t - 1))) as i32;
                } else if self.gb.left() < 0 {
                    return None;
                }
                self.zeroes = t;
                if self.zeroes != 0 {
                    self.ch[0].median = [0; 3];
                    self.ch[1].median = [0; 3];
                    let c = &mut self.ch[channel];
                    c.slow_level = c.slow_level.wrapping_sub(level_decay(c.slow_level));
                    return Some(0);
                }
            }
        }

        let mut t: i32;
        if self.zero {
            t = 0;
            self.zero = false;
        } else {
            t = self.gb.unary_0_33() as i32;
            if self.gb.left() < 0 {
                return None;
            }
            if t == 16 {
                let t2 = self.gb.unary_0_33() as i32;
                if t2 < 2 {
                    if self.gb.left() < 0 {
                        return None;
                    }
                    t += t2;
                } else {
                    if t2 >= 32 || self.gb.left() < t2 as i64 - 1 {
                        return None;
                    }
                    t = t.wrapping_add((self.gb.bits(t2 as u32 - 1) | (1u32 << (t2 - 1))) as i32);
                }
            }
            if self.one {
                self.one = t & 1 != 0;
                t = (t >> 1) + 1;
            } else {
                self.one = t & 1 != 0;
                t >>= 1;
            }
            self.zero = !self.one;
        }

        if self.hybrid && channel == 0 && !self.update_error_limit() {
            return None;
        }

        let hybrid_bitrate = self.hybrid_bitrate;
        let c = &mut self.ch[channel];
        let m = &mut c.median;
        let (base, add);
        if t == 0 {
            base = 0;
            add = get_med(m, 0) - 1;
            dec_med(m, 0);
        } else if t == 1 {
            base = get_med(m, 0);
            add = get_med(m, 1) - 1;
            inc_med(m, 0);
            dec_med(m, 1);
        } else if t == 2 {
            base = get_med(m, 0).wrapping_add(get_med(m, 1));
            add = get_med(m, 2) - 1;
            inc_med(m, 0);
            inc_med(m, 1);
            dec_med(m, 2);
        } else {
            base = get_med(m, 0).wrapping_add(get_med(m, 1)).wrapping_add(get_med(m, 2).wrapping_mul((t as u32).wrapping_sub(2) as i32));
            add = get_med(m, 2) - 1;
            inc_med(m, 0);
            inc_med(m, 1);
            inc_med(m, 2);
        }
        let ret;
        if c.error_limit == 0 {
            ret = base.wrapping_add(get_tail(&mut self.gb, add as u32) as i32);
            if self.gb.left() <= 0 {
                return None;
            }
        } else {
            let (mut base, mut add) = (base, add);
            let half = |base: i32, add: i32| ((base as u32).wrapping_mul(2).wrapping_add(add as u32).wrapping_add(1) >> 1) as i32;
            let mut mid = half(base, add);
            while add > c.error_limit {
                if self.gb.left() <= 0 {
                    return None;
                }
                if self.gb.bit() != 0 {
                    add = add.wrapping_sub((mid as u32).wrapping_sub(base as u32) as i32);
                    base = mid;
                } else {
                    add = (mid as u32).wrapping_sub(base as u32).wrapping_sub(1) as i32;
                }
                mid = half(base, add);
            }
            ret = mid;
        }
        let sign = self.gb.bit();
        if hybrid_bitrate {
            c.slow_level = c.slow_level.wrapping_add(wp_log2(ret as u32).wrapping_sub(level_decay(c.slow_level)));
        }
        Some(if sign != 0 { !ret } else { ret })
    }

    /// An integer sample from the decorrelated value `s`: the extra bits, the shifts and, for hybrid,
    /// the clip. The result is in the scale FFmpeg outputs.
    fn value_integer(&mut self, crc: &mut u32, s: u32) -> i32 {
        let mut s = s;
        if self.extra_bits != 0 {
            s = s.wrapping_mul(1u32 << self.extra_bits);
            if self.got_extra_bits && self.gb_extra.left() >= self.extra_bits as i64 {
                s |= self.gb_extra.bits(self.extra_bits);
                *crc = crc.wrapping_mul(9).wrapping_add((s & 0xFFFF).wrapping_mul(3)).wrapping_add(s >> 16);
            }
        }
        let bit = (s & self.and) | self.or;
        let mut bit = (s.wrapping_add(bit) << self.shift).wrapping_sub(bit) as i32;
        if self.hybrid {
            bit = if bit < self.hybrid_minclip {
                self.hybrid_minclip
            } else if bit > self.hybrid_maxclip {
                self.hybrid_maxclip
            } else {
                bit
            };
        }
        ((bit as u32) << self.post_shift) as i32
    }

    /// A float sample from the decorrelated integer `s` and the extra bits.
    fn value_float(&mut self, crc: &mut u32, s: i32) -> f32 {
        let mut exp = self.float_max_exp;
        if self.got_extra_bits {
            let max_bits = 1 + 23 + 8 + 1;
            if self.gb_extra.left() + PADDING_BITS < max_bits {
                return 0.0;
            }
        }
        let sign: u32;
        let mut m: u32;
        if s != 0 {
            let mut v = (s as u32).wrapping_mul(1u32 << self.float_shift);
            sign = ((v as i32) < 0) as u32;
            if sign != 0 {
                v = v.wrapping_neg();
            }
            if v >= 0x100_0000 {
                m = if self.got_extra_bits && self.gb_extra.bit() != 0 { self.gb_extra.bits(23) } else { 0 };
                exp = 255;
            } else if exp != 0 {
                let log2 = if v == 0 { 0 } else { 31 - v.leading_zeros() as i32 };
                let mut shift = 23 - log2;
                exp = self.float_max_exp;
                if exp <= shift {
                    exp -= 1;
                    shift = exp;
                }
                exp -= shift;
                m = v;
                if shift != 0 {
                    m <<= shift;
                    if self.float_flag & WV_FLT_SHIFT_ONES != 0 || (self.got_extra_bits && self.float_flag & WV_FLT_SHIFT_SAME != 0 && self.gb_extra.bit() != 0) {
                        m |= (1u32 << shift) - 1;
                    } else if self.got_extra_bits && self.float_flag & WV_FLT_SHIFT_SENT != 0 {
                        m |= self.gb_extra.bits(shift as u32);
                    }
                }
            } else {
                exp = self.float_max_exp;
                m = v;
            }
            m &= 0x7F_FFFF;
        } else {
            let mut sg = 0;
            m = 0;
            exp = 0;
            if self.got_extra_bits && self.float_flag & WV_FLT_ZERO_SENT != 0 {
                if self.gb_extra.bit() != 0 {
                    m = self.gb_extra.bits(23);
                    if self.float_max_exp >= 25 {
                        exp = self.gb_extra.bits(8) as i32;
                    }
                    sg = self.gb_extra.bit();
                } else if self.float_flag & WV_FLT_ZERO_SIGN != 0 {
                    sg = self.gb_extra.bit();
                }
            }
            sign = sg;
        }
        *crc = crc.wrapping_mul(27).wrapping_add(m.wrapping_mul(9)).wrapping_add((exp as u32).wrapping_mul(3)).wrapping_add(sign);
        f32::from_bits((sign << 31) | ((exp as u32) << 23) | m)
    }

    /// The output sample for the decorrelated value `s`.
    fn emit(&mut self, crc_extra: &mut u32, s: i32) -> f64 {
        match self.kind {
            Kind::Float => self.value_float(crc_extra, s) as f64,
            // FFmpeg's 16-bit output type holds the low 16 bits
            Kind::S16 => (self.value_integer(crc_extra, s as u32) as i16) as f64 * self.scale,
            Kind::S32 => self.value_integer(crc_extra, s as u32) as f64 * self.scale,
        }
    }

    fn check_crc(&self, crc: u32, crc_extra: u32) -> Result<()> {
        if self.lossy {
            return Ok(());
        }
        if crc != self.crc {
            return Err(bad("CRC error"));
        }
        if self.got_extra_bits && crc_extra != self.crc_extra_bits {
            return Err(bad("extra bits CRC error"));
        }
        Ok(())
    }

    /// The prediction of a history term: the sample `a` it predicts from, and where its history takes `j`.
    fn history(d: &mut Decorr, pos: usize) -> (i32, i32, usize) {
        if d.value > 8 {
            let (a, b);
            if d.value & 1 != 0 {
                a = 2u32.wrapping_mul(d.samples_a[0] as u32).wrapping_sub(d.samples_a[1] as u32) as i32;
                b = 2u32.wrapping_mul(d.samples_b[0] as u32).wrapping_sub(d.samples_b[1] as u32) as i32;
            } else {
                a = (3u32.wrapping_mul(d.samples_a[0] as u32).wrapping_sub(d.samples_a[1] as u32) as i32) >> 1;
                b = (3u32.wrapping_mul(d.samples_b[0] as u32).wrapping_sub(d.samples_b[1] as u32) as i32) >> 1;
            }
            d.samples_a[1] = d.samples_a[0];
            d.samples_b[1] = d.samples_b[0];
            (a, b, 0)
        } else {
            (d.samples_a[pos], d.samples_b[pos], ((pos as i32 + d.value) & 7) as usize)
        }
    }

    /// `x` plus the weighted prediction `w * p`.
    fn predict(&self, x: i32, w: i32, p: i32) -> i32 {
        if self.kind != Kind::S16 {
            x.wrapping_add(((w as i64 * p as i64 + 512) >> 10) as i32)
        } else {
            x.wrapping_add(((w as u32).wrapping_mul(p as u32).wrapping_add(512) as i32) >> 10)
        }
    }

    fn unpack_stereo(&mut self) -> Result<(Vec<f64>, Vec<f64>)> {
        let n = self.samples;
        let (mut left, mut right) = (vec![0.0; n], vec![0.0; n]);
        let mut crc = 0xFFFF_FFFFu32;
        let mut crc_extra = 0xFFFF_FFFFu32;
        let mut pos = 0usize;
        let mut count = 0usize;
        self.one = false;
        self.zero = false;
        self.zeroes = 0;
        while count < n {
            let Some(mut l) = self.get_value(0) else { break };
            let Some(mut r) = self.get_value(1) else { break };
            for i in 0..self.terms {
                let mut d = self.decorr[i];
                let t = d.value;
                if t > 0 {
                    let (a, b, j) = Self::history(&mut d, pos);
                    let l2 = self.predict(l, d.weight_a, a);
                    let r2 = self.predict(r, d.weight_b, b);
                    if a != 0 && l != 0 {
                        d.weight_a = d.weight_a.wrapping_sub(((((l ^ a) >> 30) & 2) - 1).wrapping_mul(d.delta));
                    }
                    if b != 0 && r != 0 {
                        d.weight_b = d.weight_b.wrapping_sub(((((r ^ b) >> 30) & 2) - 1).wrapping_mul(d.delta));
                    }
                    d.samples_a[j] = l2;
                    d.samples_b[j] = r2;
                    l = l2;
                    r = r2;
                } else if t == -1 {
                    let l2 = self.predict(l, d.weight_a, d.samples_a[0]);
                    update_weight_clip(&mut d.weight_a, d.delta, d.samples_a[0], l);
                    l = l2;
                    let r2 = self.predict(r, d.weight_b, l2);
                    update_weight_clip(&mut d.weight_b, d.delta, l2, r);
                    r = r2;
                    d.samples_a[0] = r;
                } else {
                    let mut r2 = self.predict(r, d.weight_b, d.samples_b[0]);
                    update_weight_clip(&mut d.weight_b, d.delta, d.samples_b[0], r);
                    r = r2;
                    if t == -3 {
                        r2 = d.samples_a[0];
                        d.samples_a[0] = r;
                    }
                    let l2 = self.predict(l, d.weight_a, r2);
                    update_weight_clip(&mut d.weight_a, d.delta, r2, l);
                    l = l2;
                    d.samples_b[0] = l;
                }
                self.decorr[i] = d;
            }
            if self.kind == Kind::S16 && (l as i64).abs() + (r as i64).abs() > 1 << 19 {
                return Err(bad(format!("sample {l} {r} too large")));
            }
            pos = (pos + 1) & 7;
            if self.joint {
                r = (r as u32).wrapping_sub((l >> 1) as u32) as i32;
                l = (l as u32).wrapping_add(r as u32) as i32;
            }
            crc = (crc.wrapping_mul(3).wrapping_add(l as u32)).wrapping_mul(3).wrapping_add(r as u32);
            left[count] = self.emit(&mut crc_extra, l);
            right[count] = self.emit(&mut crc_extra, r);
            count += 1;
        }
        self.check_crc(crc, crc_extra)?;
        Ok((left, right))
    }

    fn unpack_mono(&mut self) -> Result<Vec<f64>> {
        let n = self.samples;
        let mut out = vec![0.0; n];
        let mut crc = 0xFFFF_FFFFu32;
        let mut crc_extra = 0xFFFF_FFFFu32;
        let mut pos = 0usize;
        let mut count = 0usize;
        self.one = false;
        self.zero = false;
        self.zeroes = 0;
        while count < n {
            let Some(mut t) = self.get_value(0) else { break };
            let mut s = t;
            for i in 0..self.terms {
                let mut d = self.decorr[i];
                let (a, _, j) = Self::history(&mut d, pos);
                s = self.predict(t, d.weight_a, a);
                if a != 0 && t != 0 {
                    d.weight_a = d.weight_a.wrapping_sub(((((t ^ a) >> 30) & 2) - 1).wrapping_mul(d.delta));
                }
                d.samples_a[j] = s;
                t = s;
                self.decorr[i] = d;
            }
            pos = (pos + 1) & 7;
            crc = crc.wrapping_mul(3).wrapping_add(s as u32);
            out[count] = self.emit(&mut crc_extra, s);
            count += 1;
        }
        self.check_crc(crc, crc_extra)?;
        Ok(out)
    }
}

/// A block's decoded channels (one or two) and what its flags say.
struct Decoded {
    left: Vec<f64>,
    /// the second channel of a stereo block (a copy of the first for false stereo)
    right: Option<Vec<f64>>,
    kind: Kind,
}

/// Decodes the block whose data after the 32-byte header is `body`, with `header`'s samples, flags
/// and CRC.
fn decode_block(header: &BlockHeader, body: &[u8], samples: usize) -> Result<Decoded> {
    let flags = header.flags;
    if header.samples as usize != samples {
        return Err(bad(format!("mismatching number of samples in a sequence: {samples} and {}", header.samples)));
    }
    let kind = if flags & WV_FLOAT_DATA != 0 {
        Kind::Float
    } else if flags & 3 <= 1 {
        Kind::S16
    } else {
        Kind::S32
    };
    let bpp = if kind == Kind::S16 { 2 } else { 4 };
    let orig_bpp = ((flags & 3) + 1) << 3;
    let post_shift = (bpp * 8 + ((flags >> 13) & 0x1F)) as i32 - orig_bpp as i32;
    if !(0..=31).contains(&post_shift) {
        return Err(bad("invalid sample shift"));
    }
    let stereo = flags & WV_MONO == 0;
    let stereo_in = stereo && flags & WV_FALSE_STEREO == 0;
    let mut s = Block {
        samples,
        crc: header.crc,
        stereo,
        stereo_in,
        joint: flags & WV_JOINT_STEREO != 0,
        hybrid: flags & WV_HYBRID_MODE != 0,
        hybrid_bitrate: flags & WV_HYBRID_BITRATE != 0,
        post_shift: post_shift as u32,
        hybrid_maxclip: ((1i64 << (orig_bpp - 1)) - 1) as i32,
        hybrid_minclip: (-1i64 << (orig_bpp - 1)) as i32,
        terms: 0,
        decorr: [Decorr::default(); MAX_TERMS],
        zero: false,
        one: false,
        zeroes: 0,
        extra_bits: 0,
        and: 0,
        or: 0,
        shift: 0,
        float_flag: 0,
        float_shift: 0,
        float_max_exp: 0,
        ch: [Channel::default(); 2],
        gb: Bits::new(&[]),
        got_extra_bits: false,
        crc_extra_bits: 0,
        gb_extra: Bits::new(&[]),
        kind,
        scale: 1.0 / (1u64 << (bpp * 8 - 1)) as f64,
        lossy: false,
    };
    let si = stereo_in as usize;
    let (mut got_terms, mut got_weights, mut got_samples, mut got_entropy, mut got_pcm, mut got_float, mut got_hybrid) = (false, false, false, false, false, false, false);

    let mut p = 0usize;
    while p < body.len() {
        let id = body[p];
        let mut size = *body.get(p + 1).unwrap_or(&0) as usize;
        p += 2;
        if id & ID_IDF_LONG != 0 {
            let Some(b) = body.get(p..p + 2) else { break };
            size |= (u16::from_le_bytes([b[0], b[1]]) as usize) << 8;
            p += 2;
        }
        size <<= 1; // in words
        let ssize = size;
        if id & ID_IDF_ODD != 0 {
            size = size.saturating_sub(1);
            if ssize == 0 {
                break;
            }
        }
        if body.len().saturating_sub(p) < ssize {
            break;
        }
        let sub = &body[p..p + size];
        p += ssize;
        let mut c = Cursor { data: sub, pos: 0 };
        match id & ID_IDF_MASK {
            ID_DECTERMS => {
                if size > MAX_TERMS {
                    s.terms = 0;
                    continue;
                }
                s.terms = size;
                for i in 0..size {
                    let val = c.u8();
                    s.decorr[size - i - 1].value = (val & 0x1F) as i32 - 5;
                    s.decorr[size - i - 1].delta = (val >> 5) as i32;
                }
                got_terms = true;
            }
            ID_DECWEIGHTS => {
                if !got_terms {
                    return Err(bad("decorrelation weights before the terms"));
                }
                let weights = size >> si;
                if weights > MAX_TERMS || weights > s.terms {
                    continue;
                }
                let weight = |c: &mut Cursor| {
                    let t = (c.u8() as i8 as i32) * 8;
                    if t > 0 {
                        t + ((t + 64) >> 7)
                    } else {
                        t
                    }
                };
                for i in 0..weights {
                    let w = weight(&mut c);
                    s.decorr[s.terms - i - 1].weight_a = w;
                    if stereo_in {
                        let w = weight(&mut c);
                        s.decorr[s.terms - i - 1].weight_b = w;
                    }
                }
                got_weights = true;
            }
            ID_DECSAMPLES => {
                if !got_terms {
                    return Err(bad("decorrelation samples before the terms"));
                }
                let mut t = 0usize;
                let mut i = s.terms as i32 - 1;
                while i >= 0 && t < size {
                    let d = &mut s.decorr[i as usize];
                    let next = |c: &mut Cursor| wp_exp2(c.u16() as u16 as i16);
                    if d.value > 8 {
                        d.samples_a[0] = next(&mut c);
                        d.samples_a[1] = next(&mut c);
                        if stereo_in {
                            d.samples_b[0] = next(&mut c);
                            d.samples_b[1] = next(&mut c);
                            t += 4;
                        }
                        t += 4;
                    } else if d.value < 0 {
                        d.samples_a[0] = next(&mut c);
                        d.samples_b[0] = next(&mut c);
                        t += 4;
                    } else {
                        for j in 0..d.value as usize {
                            d.samples_a[j] = next(&mut c);
                            if stereo_in {
                                d.samples_b[j] = next(&mut c);
                            }
                        }
                        t += d.value as usize * 2 * (si + 1);
                    }
                    i -= 1;
                }
                got_samples = true;
            }
            ID_ENTROPY => {
                if size != 6 * (si + 1) {
                    continue;
                }
                for j in 0..=si {
                    for i in 0..3 {
                        s.ch[j].median[i] = wp_exp2(c.u16() as u16 as i16);
                    }
                }
                got_entropy = true;
            }
            ID_HYBRID => {
                let mut left = size as i32;
                if s.hybrid_bitrate {
                    for i in 0..=si {
                        s.ch[i].slow_level = wp_exp2(c.u16() as u16 as i16);
                        left -= 2;
                    }
                }
                for i in 0..=si {
                    s.ch[i].bitrate_acc = c.u16() << 16;
                    left -= 2;
                }
                for i in 0..=si {
                    s.ch[i].bitrate_delta = if left > 0 { wp_exp2(c.u16() as u16 as i16) as u32 } else { 0 };
                }
                got_hybrid = true;
            }
            ID_INT32INFO => {
                if size != 4 {
                    continue;
                }
                let val = [c.u8(), c.u8(), c.u8(), c.u8()];
                if val[0] > 30 {
                    continue;
                }
                s.extra_bits = val[0] as u32;
                if val[1] != 0 {
                    s.shift = val[1] as u32;
                }
                if val[2] != 0 {
                    s.and = 1;
                    s.or = 1;
                    s.shift = val[2] as u32;
                }
                if val[3] != 0 {
                    s.and = 1;
                    s.shift = val[3] as u32;
                }
                if s.shift > 31 {
                    s.and = 0;
                    s.or = 0;
                    s.shift = 0;
                    continue;
                }
                // the reference decoder treats 32-bit lossy sound as 24-bit, for the clipping
                if s.hybrid && bpp == 4 && s.post_shift < 8 && s.shift > 8 {
                    s.post_shift += 8;
                    s.shift -= 8;
                    s.hybrid_maxclip >>= 8;
                    s.hybrid_minclip >>= 8;
                }
            }
            ID_FLOATINFO => {
                if size != 4 {
                    continue;
                }
                s.float_flag = c.u8();
                s.float_shift = c.u8() as u32;
                s.float_max_exp = c.u8() as i32;
                if s.float_shift > 31 {
                    s.float_shift = 0;
                    continue;
                }
                got_float = true;
            }
            ID_DATA => {
                s.gb = Bits::new(sub);
                got_pcm = true;
            }
            ID_DSD_DATA => return Err(Error::new(ErrorCode::UnsupportedFormat, "WavPack: DSD audio is not supported")),
            ID_EXTRABITS => {
                if size <= 4 {
                    continue;
                }
                s.gb_extra = Bits::new(sub);
                s.crc_extra_bits = s.gb_extra.bits(32);
                s.got_extra_bits = true;
            }
            ID_CHANINFO if size <= 1 => return Err(bad("insufficient channel information")),
            _ => {}
        }
    }

    if !got_pcm {
        return Err(bad("packed samples not found"));
    }
    if !got_terms {
        return Err(bad("no block with decorrelation terms"));
    }
    if !got_weights {
        return Err(bad("no block with decorrelation weights"));
    }
    if !got_samples {
        return Err(bad("no block with decorrelation samples"));
    }
    if !got_entropy {
        return Err(bad("no block with entropy information"));
    }
    if s.hybrid && !got_hybrid {
        return Err(bad("hybrid configuration not found"));
    }
    if kind == Kind::Float && !got_float {
        return Err(bad("float information not found"));
    }
    if s.got_extra_bits && kind != Kind::Float {
        let wanted = (samples as i64 * s.extra_bits as i64) << si;
        if s.gb_extra.left() < wanted {
            s.got_extra_bits = false;
        }
    }

    if stereo_in {
        let (l, r) = s.unpack_stereo()?;
        Ok(Decoded { left: l, right: Some(r), kind })
    } else {
        let l = s.unpack_mono()?;
        let right = s.stereo.then(|| l.clone());
        Ok(Decoded { left: l, right, kind })
    }
}

/// Rebuilds a frame in Matroska's form (the sample count, then each block's flags and CRC, its length
/// when the frame has several blocks, and its data) as blocks with headers.
fn from_matroska(d: &[u8]) -> Result<Vec<u8>> {
    if d.len() < 12 {
        return Err(bad("a Matroska block is too short"));
    }
    let samples = le32(d, 0);
    let mut src = &d[4..];
    let mut out = Vec::with_capacity(d.len() + 32);
    while src.len() >= 8 {
        let flags = le32(src, 0);
        let crc = le32(src, 4);
        src = &src[8..];
        let multiblock = flags & WV_SINGLE_BLOCK != WV_SINGLE_BLOCK;
        let len = if multiblock {
            if src.len() < 4 {
                return Err(bad("a Matroska block is cut short"));
            }
            let n = le32(src, 0) as usize;
            src = &src[4..];
            n
        } else {
            src.len()
        };
        if len > src.len() {
            return Err(bad("a Matroska block is longer than its data"));
        }
        out.extend_from_slice(b"wvpk");
        out.extend_from_slice(&(len as u32 + 24).to_le_bytes());
        out.extend_from_slice(&0x410u16.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&samples.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&src[..len]);
        src = &src[len..];
    }
    Ok(out)
}

/// Decodes WavPack frames to interleaved f64.
pub struct WavPackDecoder {
    channels: usize,
    sample_rate: u32,
}

impl WavPackDecoder {
    pub fn new(s: &StreamInfo) -> Result<Self> {
        let StreamParams::Audio { sample_rate, channels } = s.params else {
            return Err(bad("not an audio stream"));
        };
        if channels == 0 {
            return Err(bad("the stream has no channels"));
        }
        Ok(Self { channels: channels as usize, sample_rate })
    }

    /// Decodes one frame (the blocks of all channels) to its samples, interleaved.
    fn decode_frame(&self, data: &[u8]) -> Result<(Vec<f64>, usize)> {
        if data.len() <= HEADER_SIZE {
            return Err(bad("packet too short"));
        }
        let data: Cow<[u8]> = if &data[..4] == b"wvpk" { Cow::Borrowed(data) } else { Cow::Owned(from_matroska(data)?) };
        if data.len() <= HEADER_SIZE {
            return Err(bad("packet too short"));
        }
        let samples = le32(&data, 20) as usize;
        let flags = le32(&data, 24);
        if samples == 0 || samples > MAX_SAMPLES {
            return Err(bad(format!("invalid number of samples: {samples}")));
        }
        if flags & WV_DSD_DATA != 0 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "WavPack: DSD audio is not supported"));
        }
        let channels = self.channels;
        let mut out = vec![0.0f64; samples * channels];
        let mut ch_offset = 0usize;
        let mut first_kind = None;
        let mut buf: &[u8] = &data;
        while buf.len() > HEADER_SIZE {
            let Some(h) = BlockHeader::parse(buf) else { return Err(bad("invalid block header")) };
            let end = h.body_len() + HEADER_SIZE;
            if end > buf.len() {
                return Err(bad(format!("block has invalid size (size {} vs. {} bytes left)", end, buf.len())));
            }
            let d = decode_block(&h, &buf[HEADER_SIZE..end], samples)?;
            if *first_kind.get_or_insert(d.kind) != d.kind {
                return Err(bad("blocks of one frame differ in sample format"));
            }
            let stereo = d.right.is_some() as usize;
            if ch_offset + stereo >= channels {
                // more channels coded than the stream has
                if ch_offset == 0 {
                    return Err(bad("too many channels coded in a packet"));
                }
            } else {
                for (k, plane) in std::iter::once(&d.left).chain(d.right.iter()).enumerate() {
                    for (i, &v) in plane.iter().enumerate() {
                        out[i * channels + ch_offset + k] = v;
                    }
                }
                ch_offset += 1 + stereo;
            }
            buf = &buf[end..];
        }
        if ch_offset != channels {
            return Err(bad("not enough channels coded in a packet"));
        }
        Ok((out, samples))
    }
}

impl Decoder for WavPackDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let (samples, _) = self.decode_frame(&packet.data)?;
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.sample_rate, channels: self.channels as u16, pts: packet.pts, samples })])
    }

    /// Every block carries its own state, so there is nothing to forget.
    fn reset(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
