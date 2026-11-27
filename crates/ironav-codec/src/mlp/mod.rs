//! Dolby TrueHD and MLP (Meridian Lossless Packing) decoder: lossless audio of DVD-Audio and Blu-ray,
//! and the lossless layer of Dolby Atmos in TrueHD. Both are decoded to the integers the encoder was
//! given, as FFmpeg's `mlpdec.c` does.
//!
//! Decoded: the major sync (TrueHD 0xF8726FBA, MLP 0xF8726FBB) with its checksum, the restart header
//! (channel ranges, noise type and seed, channel assignment, header checksum), the decoding
//! parameters (block size, matrices, output shift, quantiser step, FIR and IIR prediction filters per
//! channel, Huffman offsets and codebooks), Huffman and raw LSB residuals, the prediction filters,
//! the primitive matrices with their bypassed LSBs and noise (the 2-channel noise generator of MLP
//! and the table-driven noise buffer of TrueHD), the output shift, the 8-bit substream parity and
//! checksum, the access unit parity, the lossless check data, and the end-of-stream marker with its
//! shortening of the last access unit.
//!
//! Substreams: the decoder decodes substreams up to `max_decoded_substream`, which is the last one
//! but never past the third (index 2), as FFmpeg does. A TrueHD stream carries a 2-channel
//! presentation in substream 0, a 6-channel one in substream 1 and an 8-channel one in substream 2;
//! each adds channels to the ones before, so a stream with all three decodes to 8 channels (7.1).
//! The fourth substream, which Dolby Atmos uses for its object data (the 16-channel presentation),
//! is not decoded: it is skipped, so an Atmos stream plays its 7.1 TrueHD bed.
//!
//! Output is interleaved `AudioFrame` samples in FFmpeg's native channel order (FL FR FC LFE BL BR
//! SL SR for 7.1, the same order the AC-3, E-AC-3 and DTS decoders here use), scaled to [-1, 1) by
//! 2^23, so a 24-bit sample is exact. A stream of 16 bits or fewer (MLP only) gives the 16-bit
//! samples FFmpeg's S16 output gives, scaled by 2^15. Multiply by 2^23 (or 2^15) to get the integers.
//!
//! Failed checks: an access unit whose parity or major sync checksum is wrong is refused, as in
//! FFmpeg. A wrong restart header checksum, substream parity or checksum, or lossless check value
//! is, as in FFmpeg, reported but does not stop decoding; they are counted in
//! [`MlpDecoder::check_failures`].
//!
//! The decoding follows FFmpeg's `libavcodec/mlpdec.c`, `mlp.c`, `mlp_parse.c` and `mlpdsp.c`
//! (LGPL-2.1 or later, Copyright (c) 2007-2008 Ian Caulfield), including its tables.

mod bits;
mod decode;

pub use decode::MlpDecoder;

use bits::Bits;
use ironav_core::{Error, ErrorCode, Result};

/// The channels of FFmpeg's native order, as bit masks (the bit is the channel's number in FFmpeg).
pub(crate) mod ch {
    pub const FL: u64 = 1 << 0;
    pub const FR: u64 = 1 << 1;
    pub const FC: u64 = 1 << 2;
    pub const LFE: u64 = 1 << 3;
    pub const BL: u64 = 1 << 4;
    pub const BR: u64 = 1 << 5;
    pub const FLC: u64 = 1 << 6;
    pub const FRC: u64 = 1 << 7;
    pub const BC: u64 = 1 << 8;
    pub const SL: u64 = 1 << 9;
    pub const SR: u64 = 1 << 10;
    pub const TC: u64 = 1 << 11;
    pub const TFL: u64 = 1 << 12;
    pub const TFC: u64 = 1 << 13;
    pub const TFR: u64 = 1 << 14;
    pub const WL: u64 = 1 << 31;
    pub const WR: u64 = 1 << 32;
    pub const SDL: u64 = 1 << 33;
    pub const SDR: u64 = 1 << 34;
    pub const LFE2: u64 = 1 << 35;
}

use ch::*;

pub(crate) const SYNC_MLP: u8 = 0xBB;
pub(crate) const SYNC_TRUEHD: u8 = 0xBA;

/// Bits per sample by the MLP quantisation code.
const MLP_QUANTS: [u8; 16] = [16, 20, 24, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

pub(crate) const STEREO: u64 = FL | FR;
const SURROUND: u64 = FL | FR | FC;
const QUAD: u64 = FL | FR | BL | BR;
const FOUR_POINT_ZERO: u64 = SURROUND | BC;
pub(crate) const FIVE_ZERO_BACK: u64 = SURROUND | BL | BR;
pub(crate) const FIVE_ONE_BACK: u64 = FIVE_ZERO_BACK | LFE;

/// The channels of each MLP channel arrangement (`mlp_layout`).
const MLP_LAYOUT: [u64; 32] = [
    FC,
    STEREO,
    STEREO | BC,
    QUAD,
    STEREO | LFE,
    STEREO | BC | LFE,
    QUAD | LFE,
    SURROUND,
    FOUR_POINT_ZERO,
    FIVE_ZERO_BACK,
    SURROUND | LFE,
    FOUR_POINT_ZERO | LFE,
    FIVE_ONE_BACK,
    FOUR_POINT_ZERO,
    FIVE_ZERO_BACK,
    SURROUND | LFE,
    FOUR_POINT_ZERO | LFE,
    FIVE_ONE_BACK,
    QUAD | LFE,
    FIVE_ZERO_BACK,
    FIVE_ONE_BACK,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
];

/// The channels of each of the 13 groups of a TrueHD channel map (`thd_layout`), in bit order:
/// LR, C, LFE, LRs, LRvh, LRc, LRrs, Cs, Ts, LRsd, LRw, Cvh, LFE2.
const THD_LAYOUT: [u64; 13] = [FL | FR, FC, LFE, SL | SR, TFL | TFR, FLC | FRC, BL | BR, BC, TC, SDL | SDR, WL | WR, TFC, LFE2];

/// The order TrueHD numbers its channels in (`thd_channel_order`).
pub(crate) const THD_CHANNEL_ORDER: [u64; 20] = [FL, FR, FC, LFE, SL, SR, TFL, TFR, FLC, FRC, BL, BR, BC, TC, SDL, SDR, WL, WR, TFC, LFE2];

fn truehd_layout(chanmap: u32) -> u64 {
    (0..13).filter(|i| chanmap >> i & 1 != 0).fold(0, |m, i| m | THD_LAYOUT[i])
}

/// The sample rate for a rate code, 0 for none.
fn samplerate(code: u32) -> u32 {
    if code == 0xF {
        return 0;
    }
    (if code & 8 != 0 { 44100 } else { 48000 }) << (code & 7)
}

fn invalid(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("MLP/TrueHD: {msg}"))
}

/// MSB-first CRC with polynomial `poly` (8 bits) over `data`, from `init`.
fn crc8(poly: u8, init: u8, data: &[u8]) -> u8 {
    let mut c = init;
    for &b in data {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ poly } else { c << 1 };
        }
    }
    c
}

/// The 16-bit CRC with polynomial 0x002D (`crc_2D`) over `data`, as the big-endian value.
fn crc16(data: &[u8]) -> u16 {
    let mut c = 0u16;
    for &b in data {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x002D } else { c << 1 };
        }
    }
    c
}

/// Whether the 16-bit check of a major sync of `size` bytes holds (`ff_mlp_checksum16` of the
/// header less its last two bytes, against those two bytes).
fn major_sync_checksum_ok(buf: &[u8], size: usize) -> bool {
    let at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
    crc16(&buf[..size - 4]) ^ at(size - 4) == at(size - 2)
}

/// `ff_mlp_checksum8` of the first `size` bytes of `buf` (at least 1): the CRC-8 (polynomial 0x63,
/// starting at 0x3C) of all but the last, xor the last.
fn checksum8(buf: &[u8], size: usize) -> u8 {
    crc8(0x63, 0x3C, &buf[..size - 1]) ^ buf[size - 1]
}

/// `ff_mlp_restart_checksum`: the CRC-8 (polynomial 0x1D) of the `bit_size` bits of a restart header
/// and the two flag bits before it, the two top bits of the first byte left out.
fn restart_checksum(buf: &[u8], bit_size: usize) -> u8 {
    let num_bytes = (bit_size + 2) / 8;
    if num_bytes == 0 || buf.len() < num_bytes {
        return 0;
    }
    let mut crc = crc8(0x1D, buf[0] & 0xC0, &buf[..num_bytes - 1]) as u32;
    crc ^= buf[num_bytes - 1] as u32;
    for i in 0..((bit_size + 2) & 7) {
        crc <<= 1;
        if crc & 0x100 != 0 {
            crc ^= 0x11D;
        }
        crc ^= ((buf.get(num_bytes).copied().unwrap_or(0) >> (7 - i)) & 1) as u32;
    }
    crc as u8
}

/// The xor of all the bytes (`ff_mlp_calculate_parity`).
fn parity(buf: &[u8]) -> u8 {
    buf.iter().fold(0, |a, &b| a ^ b)
}

/// What a major sync says (`MLPHeaderInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MajorSync {
    /// TrueHD (sync 0xF8726FBA) or MLP (0xF8726FBB).
    pub truehd: bool,
    /// Bytes of the major sync, not counting the 4-byte access unit header before it.
    pub header_size: usize,
    /// Bits per sample of the first channel group (always 24 for TrueHD).
    pub group1_bits: u32,
    pub group2_bits: u32,
    pub sample_rate: u32,
    pub group2_sample_rate: u32,
    pub channel_arrangement: u32,
    pub modifier_stream0: u32,
    pub modifier_stream1: u32,
    pub modifier_stream2: u32,
    /// MLP: the channels. TrueHD: those of the 6-channel presentation (substream 1) and of the
    /// 8-channel one (substream 2); as channel masks in FFmpeg's channel bits.
    pub layout_mlp: u64,
    pub layout_thd1: u64,
    pub layout_thd2: u64,
    /// Samples per channel in an access unit.
    pub access_unit_size: usize,
    pub access_unit_size_pow2: usize,
    pub is_vbr: bool,
    pub peak_bitrate: u32,
    pub num_substreams: usize,
    pub extended_substream_info: u32,
    pub substream_info: u32,
}

impl MajorSync {
    /// The channel masks of the substreams (`read_major_sync` sets them; a substream a stream has no
    /// word for keeps `previous`).
    fn masks(&self, previous: [u64; 4]) -> [u64; 4] {
        let mut m = previous;
        let n = self.num_substreams;
        if !self.truehd {
            let substr = usize::from(n > 1);
            if substr == 1 {
                m[0] = STEREO;
            }
            m[substr] = self.layout_mlp;
        } else {
            let (c1, c2) = (self.layout_thd1.count_ones(), self.layout_thd2.count_ones());
            m[1] = self.layout_thd1;
            if c1 == 2 && c2 == 2 {
                m[0] = STEREO;
            }
            if n > 1 {
                m[0] = STEREO;
            }
            if n == 1 && c1 == 1 && c2 == 1 {
                m[0] = FC;
            }
            if n > 2 {
                m[2] = if self.layout_thd2 != 0 { self.layout_thd2 } else { self.layout_thd1 };
            }
            if n == 2 {
                m[1] = self.layout_thd2;
            }
        }
        m
    }

    /// The channels the decoder outputs: those of the last substream it decodes.
    pub fn channels(&self) -> usize {
        let last = self.num_substreams.saturating_sub(1).min(2);
        self.masks([0; 4])[last].count_ones() as usize
    }
}

/// Reads the major sync at the start of `buf`, which begins just after the 4-byte access unit header
/// (`ff_mlp_read_major_sync`). The 16-bit check is verified.
pub fn parse_major_sync(buf: &[u8]) -> Result<MajorSync> {
    if buf.len() < 28 {
        return Err(invalid("packet too short, unable to read major sync"));
    }
    let mut header_size = 28;
    if buf[..4] == [0xF8, 0x72, 0x6F, 0xBA] && buf[25] & 1 != 0 {
        header_size += 2 + (buf[26] >> 4) as usize * 2;
    }
    if buf.len() < header_size {
        return Err(invalid("packet too short, unable to read major sync"));
    }
    if !major_sync_checksum_ok(buf, header_size) {
        return Err(invalid("major sync info header checksum error"));
    }
    let mut r = Bits::new(buf);
    if r.bits(24) != 0xF8726F {
        return Err(invalid("bad major sync word"));
    }
    let stream_type = r.bits(8) as u8;
    let mut mh = MajorSync {
        truehd: false,
        header_size,
        group1_bits: 0,
        group2_bits: 0,
        sample_rate: 0,
        group2_sample_rate: 0,
        channel_arrangement: 0,
        modifier_stream0: 0,
        modifier_stream1: 0,
        modifier_stream2: 0,
        layout_mlp: 0,
        layout_thd1: 0,
        layout_thd2: 0,
        access_unit_size: 0,
        access_unit_size_pow2: 0,
        is_vbr: false,
        peak_bitrate: 0,
        num_substreams: 0,
        extended_substream_info: 0,
        substream_info: 0,
    };
    let ratebits;
    match stream_type {
        SYNC_MLP => {
            mh.group1_bits = MLP_QUANTS[r.bits(4) as usize] as u32;
            mh.group2_bits = MLP_QUANTS[r.bits(4) as usize] as u32;
            ratebits = r.bits(4);
            mh.sample_rate = samplerate(ratebits);
            mh.group2_sample_rate = samplerate(r.bits(4));
            r.skip(11);
            mh.channel_arrangement = r.bits(5);
            mh.layout_mlp = MLP_LAYOUT[mh.channel_arrangement as usize];
        }
        SYNC_TRUEHD => {
            mh.truehd = true;
            mh.group1_bits = 24;
            ratebits = r.bits(4);
            mh.sample_rate = samplerate(ratebits);
            r.skip(4);
            mh.modifier_stream0 = r.bits(2);
            mh.modifier_stream1 = r.bits(2);
            mh.channel_arrangement = r.bits(5);
            mh.layout_thd1 = truehd_layout(mh.channel_arrangement);
            mh.modifier_stream2 = r.bits(2);
            mh.layout_thd2 = truehd_layout(r.bits(13));
        }
        _ => return Err(invalid("unknown stream type in the major sync")),
    }
    mh.access_unit_size = 40 << (ratebits & 7);
    mh.access_unit_size_pow2 = 64 << (ratebits & 7);
    r.skip(48);
    mh.is_vbr = r.bit();
    mh.peak_bitrate = ((r.bits(15) as u64 * mh.sample_rate as u64 + 8) >> 4) as u32;
    mh.num_substreams = r.bits(4) as usize;
    r.skip(2);
    mh.extended_substream_info = r.bits(2);
    mh.substream_info = r.bits(8);
    Ok(mh)
}

/// The length in bytes of the access unit at the start of `data` (its 12-bit length field, in
/// 16-bit words), or `None` if it is under 4 bytes.
pub fn access_unit_length(data: &[u8]) -> Option<usize> {
    let w = u16::from_be_bytes([*data.first()?, *data.get(1)?]);
    let n = (w & 0xFFF) as usize * 2;
    (n >= 4).then_some(n)
}

/// Whether the access unit at the start of `data` carries a major sync: `Some(true)` for TrueHD's,
/// `Some(false)` for MLP's.
pub fn major_sync_kind(data: &[u8]) -> Option<bool> {
    match data.get(4..8)? {
        [0xF8, 0x72, 0x6F, 0xBA] => Some(true),
        [0xF8, 0x72, 0x6F, 0xBB] => Some(false),
        _ => None,
    }
}

/// The major sync of the access unit at the start of `data`, if it has one with a sound header.
pub fn major_sync_at_start(data: &[u8]) -> Option<MajorSync> {
    let len = access_unit_length(data)?;
    major_sync_kind(data)?;
    parse_major_sync(data.get(4..len.min(data.len()))?).ok()
}

/// Checks the first nibble of an access unit that has no major sync: the parity of its 4-byte
/// header and its substream headers (2 bytes each, and 2 more when the substream's extra-word flag
/// is set), as FFmpeg's parser does. `num_substreams` is the stream's count.
pub fn access_unit_parity_ok(data: &[u8], num_substreams: usize) -> bool {
    let mut p = 0usize;
    let mut parity_bits = 0u8;
    for i in -1..num_substreams as i32 {
        let Some(b) = data.get(p..p + 2) else { return false };
        parity_bits ^= b[0] ^ b[1];
        p += 2;
        if i < 0 || b[0] & 0x80 != 0 {
            let Some(b) = data.get(p..p + 2) else { return false };
            parity_bits ^= b[0] ^ b[1];
            p += 2;
        }
    }
    ((parity_bits >> 4) ^ parity_bits) & 0xF == 0xF
}

/// The sample rate and channel count a `dmlp` box (MP4's TrueHD configuration) gives, from its
/// 32-bit format info, as FFmpeg's `mov_read_dmlp` reads it: the rate code in the top four bits, and
/// the channel map of the 8-channel presentation if it has one, else that of the 6-channel one.
/// `None` for no rate or no channels.
pub fn dmlp_params(format_info: u32) -> Option<(u32, u16)> {
    let rate = samplerate(format_info >> 28);
    let (a1, a2) = ((format_info >> 15) & 0x1F, format_info & 0x1FFF);
    let channels = truehd_layout(if a2 != 0 { a2 } else { a1 }).count_ones() as u16;
    (rate != 0 && channels != 0).then_some((rate, channels))
}

#[cfg(test)]
mod tests;
