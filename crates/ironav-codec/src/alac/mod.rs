//! Apple Lossless (ALAC), following the bitstream that Apple's reference decoder and FFmpeg read and write.
//!
//! Translated from FFmpeg's `libavcodec/alac.c` (Copyright (c) 2005 David Hammerton), `libavcodec/alacenc.c`
//! (Copyright (c) 2008 Jaikrishnan Menon) and the channel tables of `libavcodec/alac_data.c`, LGPL-2.1-or-later.
//!
//! * [`decoder`] handles compressed frames (adaptive predictor, adaptive Rice codes with zero runs,
//!   stereo decorrelation, extra low bits) and uncompressed frames, at 16, 20 and 24 bits, and 32-bit
//!   mono, with one to eight channels in the elements the layout asks for.
//! * [`encoder`] writes 16 and 24-bit frames: a second-order predictor per channel, the best of
//!   left-right, left-side and mid-side for each pair, and an uncompressed frame where that is smaller.
//!
//! Each packet is exactly one ALAC frame of up to 4096 samples. The codec configuration is the
//! 24-byte ALACSpecificConfig, which an MP4 carries in an `alac` box (see [`specific_config`]).

pub mod decoder;
pub mod encoder;

pub use decoder::AlacDecoder;
pub use encoder::AlacEncoder;

use ironav_core::{Error, Result};

/// Samples per frame in the configuration; a shorter frame says its length in the frame.
pub const FRAME_LENGTH: u32 = 4096;
/// Where each channel of a frame is in the output: the `i`-th channel of the bitstream (counting the
/// elements in order, a pair as two) is output channel `CHANNEL_OFFSETS[channels - 1][i]`, so the
/// output is in the usual order (front left, front right, centre, LFE, back left, back right, ...).
/// As FFmpeg's `ff_alac_channel_layout_offsets`.
pub(crate) const CHANNEL_OFFSETS: [&[usize]; 8] = [&[0], &[0, 1], &[2, 0, 1], &[2, 0, 1, 3], &[2, 0, 1, 3, 4], &[2, 0, 1, 4, 5, 3], &[2, 0, 1, 4, 5, 6, 3], &[2, 6, 7, 0, 1, 4, 5, 3]];

/// The Rice history multiplier, the initial history and the largest Rice parameter the encoder uses.
const PB: u32 = 40;
const MB: u32 = 10;
const KB: u32 = 14;

pub(crate) fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("ALAC: {msg}"))
}

/// The fields of an ALACSpecificConfig that decoding needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Config {
    pub frame_length: u32,
    pub bits: u32,
    pub pb: u32,
    pub mb: u32,
    pub kb: u32,
    pub channels: u32,
    pub rate: u32,
}

/// Parse the 24-byte ALACSpecificConfig, or the 36-byte `alac` atom (size, tag, version, then the
/// config) that FFmpeg keeps as extradata.
pub(crate) fn parse_config(b: &[u8]) -> Result<Config> {
    let c = match b.len() {
        24 => b,
        36 if &b[4..8] == b"alac" => &b[12..],
        _ => return Err(bad("the codec configuration is not a 24-byte ALACSpecificConfig")),
    };
    if c[4] != 0 {
        return Err(bad("unsupported ALAC compatible version"));
    }
    let cfg = Config {
        frame_length: u32::from_be_bytes(c[0..4].try_into().unwrap()),
        bits: c[5] as u32,
        pb: c[6] as u32,
        mb: c[7] as u32,
        kb: c[8] as u32,
        channels: c[9] as u32,
        rate: u32::from_be_bytes(c[20..24].try_into().unwrap()),
    };
    if cfg.frame_length == 0 || cfg.frame_length > 4096 * 4096 {
        return Err(bad("invalid frame length in the configuration"));
    }
    if cfg.kb == 0 {
        return Err(bad("invalid Rice limit in the configuration"));
    }
    Ok(cfg)
}

/// The 24-byte ALACSpecificConfig for a stream this crate's encoder writes.
pub fn specific_config(rate: u32, channels: u8, bits: u8) -> [u8; 24] {
    let mut c = [0u8; 24];
    c[0..4].copy_from_slice(&FRAME_LENGTH.to_be_bytes());
    c[5] = bits;
    c[6] = PB as u8;
    c[7] = MB as u8;
    c[8] = KB as u8;
    c[9] = channels;
    c[10..12].copy_from_slice(&255u16.to_be_bytes()); // maxRun
    c[20..24].copy_from_slice(&rate.to_be_bytes());
    c
}

/// Sign-extends the low `bits` bits of `v`.
pub(crate) fn sign_extend(v: i32, bits: u32) -> i32 {
    let s = 32 - bits;
    (((v as u32) << s) as i32) >> s
}

/// Residuals are coded as unsigned zigzag values: 0, -1, 1, -2, ...
pub(crate) fn zigzag(v: i32) -> u32 {
    if v >= 0 {
        (v as u32) << 1
    } else {
        ((-(v as i64) as u32) << 1) - 1
    }
}

pub(crate) fn unzigzag(x: u32) -> i32 {
    (x >> 1) as i32 ^ -((x & 1) as i32)
}

/// The adaptive predictor's estimate of sample `i`, before its residual is added: the sample is
/// predicted from the `coefs.len()` samples before it, relative to the one before those.
pub(crate) fn lpc_base(out: &[i32], i: usize, coefs: &[i16], quant: u32) -> i32 {
    let order = coefs.len();
    let d = out[i - order - 1];
    let mut sum = 0i32;
    for (j, &c) in coefs.iter().enumerate() {
        sum = sum.wrapping_add(out[i - order + j].wrapping_sub(d).wrapping_mul(c as i32));
    }
    let v = ((sum as i64 + (1i64 << (quant - 1))) >> quant) as i32;
    v.wrapping_add(d)
}

/// Sign-sign adaptation of the predictor after sample `i`, whose residual is `err`.
pub(crate) fn lpc_adapt(out: &[i32], i: usize, coefs: &mut [i16], quant: u32, err: i32) {
    let order = coefs.len();
    let d = out[i - order - 1];
    let sign = err.signum();
    if sign == 0 {
        return;
    }
    let mut e = err;
    for (j, c) in coefs.iter_mut().enumerate() {
        if e.wrapping_mul(sign) <= 0 {
            break;
        }
        let val = d.wrapping_sub(out[i - order + j]);
        let s = val.signum() * sign;
        *c = c.wrapping_sub(s as i16);
        e = e.wrapping_sub((val.wrapping_mul(s) >> quant).wrapping_mul(j as i32 + 1));
    }
}

/// The adaptive Rice parameter for a history value.
pub(crate) fn rice_k(history: u32, limit: u32) -> u32 {
    (ilog2((history >> 9) + 3)).min(limit)
}

/// The Rice parameter of the zero-run length that follows a low-history sample.
pub(crate) fn run_k(history: u32, limit: u32) -> u32 {
    (7 + ((history + 16) >> 6)).saturating_sub(ilog2(history)).min(limit)
}

fn ilog2(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        31 - v.leading_zeros()
    }
}

/// The running history after a residual `x` (zigzag) at Rice multiplier `mult`.
pub(crate) fn update_history(history: u32, x: u32, mult: u32) -> u32 {
    if x > 0xffff {
        0xffff
    } else {
        history.wrapping_add(x.wrapping_mul(mult)).wrapping_sub(history.wrapping_mul(mult) >> 9)
    }
}

/// The Rice multiplier a frame's header selects: the 3-bit field times the configured `pb` over 4.
pub(crate) fn frame_mult(field: u32, pb: u32) -> u32 {
    field * pb / 4
}

pub(crate) const INITIAL_HISTORY: u32 = MB;
pub(crate) const MULT: u32 = PB;
pub(crate) const LIMIT: u32 = KB;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let c = specific_config(96_000, 2, 24);
        assert_eq!(&c[0..4], &4096u32.to_be_bytes());
        let p = parse_config(&c).unwrap();
        assert_eq!(p, Config { frame_length: 4096, bits: 24, pb: 40, mb: 10, kb: 14, channels: 2, rate: 96_000 });
        // the atom form ffmpeg keeps as extradata parses the same
        let mut atom = vec![0, 0, 0, 36];
        atom.extend_from_slice(b"alac");
        atom.extend_from_slice(&[0, 0, 0, 0]);
        atom.extend_from_slice(&c);
        assert_eq!(parse_config(&atom).unwrap(), p);
        assert!(parse_config(&c[..23]).is_err());
    }

    #[test]
    fn zigzag_is_its_own_inverse() {
        for v in [0, 1, -1, 2, -2, 32767, -32768, 1 << 20, -(1 << 20)] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!((zigzag(0), zigzag(-1), zigzag(1), zigzag(-2)), (0, 1, 2, 3));
    }

    #[test]
    fn sign_extension_keeps_in_range_values() {
        assert_eq!(sign_extend(0xFFFF, 16), -1);
        assert_eq!(sign_extend(0x7FFF, 16), 32767);
        assert_eq!(sign_extend(-5, 32), -5);
    }
}
