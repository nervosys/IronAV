//! Windows Media Audio versions 1 and 2 (ASF format tags 0x160 and 0x161): lossy MDCT audio, mono
//! or stereo, up to 50 kHz. Output is float, interleaved, at the stream's rate.
//!
//! Decoded: the superframes with and without the bit reservoir (a frame may continue into the next
//! packet), block switching (variable block sizes, with the windows of a long block next to a short
//! one), exponents coded as ten LSP coefficients or as Huffman-coded deltas, noise substitution of
//! the high bands, run-level coded coefficients with the six Huffman tables chosen by rate, and
//! mid/side stereo. Not decoded: WMA Pro, WMA Lossless and WMA Voice, which are other codecs under
//! other format tags.
//!
//! The decoder is built from a stream's `extradata`, which holds the WAVEFORMATEX the container
//! gave (as Matroska's `A_MS/ACM` private data does): it carries the average byte rate (the bit
//! rate the tables are chosen by), the block align (the packet, or superframe, size) and the codec's
//! own flag words.
//!
//! The decoding follows FFmpeg's `libavcodec/wmadec.c`, `wma.c` and `wma_common.c`, and its tables
//! in `wmadata.h` and `wma_freqs.c` (see `tools/gen_wma_tables.py`), which are licensed under the
//! LGPL-2.1 or later (Copyright (c) 2002 The FFmpeg Project).

mod tables;

use crate::imdct::Imdct;
use ironav_core::{AudioFrame, CodecId, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamInfo, StreamParams};
use std::f64::consts::PI;
use tables::*;

const BLOCK_MIN_BITS: usize = 7;
const BLOCK_MAX_BITS: usize = 11;
const BLOCK_MAX_SIZE: usize = 1 << BLOCK_MAX_BITS;
const BLOCK_NB_SIZES: usize = BLOCK_MAX_BITS - BLOCK_MIN_BITS + 1;
const NB_LSP_COEFS: usize = 10;
const MAX_CODED_SUPERFRAME_SIZE: usize = 32768;
const NOISE_TAB_SIZE: usize = 8192;
const LSP_POW_BITS: u32 = 7;
/// Room in each exponent row beyond a full block, for an exponent run that overshoots it.
const EXP_SLACK: usize = 8;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("WMA: {msg}"))
}

/// MSB-first bit reader. Bits past the end of the data read as zero; `left` says how many remain
/// before the end the caller set, and goes negative when a stream reads past it.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8], end: usize) -> Self {
        Self { d, pos: 0, end: end.min(d.len() * 8) }
    }

    fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let mut w = [0u8; 8];
        if byte < self.d.len() {
            let take = (self.d.len() - byte).min(8);
            w[..take].copy_from_slice(&self.d[byte..byte + take]);
        }
        ((u64::from_be_bytes(w) << (self.pos & 7)) >> (64 - n)) as u32
    }

    /// Reads `n` (0 to 32) bits.
    fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn left(&self) -> isize {
        self.end as isize - self.pos as isize
    }

    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

const LEAF: u32 = 1 << 31;

/// A prefix code, as a binary tree walked from a peek of the longest code.
struct Vlc {
    nodes: Vec<[u32; 2]>,
    symbols: Vec<i32>,
    max_len: u32,
}

impl Vlc {
    /// A code from (code value, length, symbol) entries.
    fn new(entries: impl Iterator<Item = (u32, u8, i32)>) -> Self {
        let mut v = Vlc { nodes: vec![[0, 0]], symbols: Vec::new(), max_len: 0 };
        for (code, len, symbol) in entries {
            if len == 0 {
                continue;
            }
            v.max_len = v.max_len.max(len as u32);
            let mut n = 0usize;
            for i in (0..len as u32).rev() {
                let bit = (code >> i & 1) as usize;
                if i == 0 {
                    v.nodes[n][bit] = LEAF | v.symbols.len() as u32;
                    v.symbols.push(symbol);
                } else {
                    if v.nodes[n][bit] == 0 {
                        v.nodes.push([0, 0]);
                        v.nodes[n][bit] = (v.nodes.len() - 1) as u32;
                    }
                    n = v.nodes[n][bit] as usize;
                }
            }
        }
        v
    }

    /// A code whose values follow the order of the entries (FFmpeg's `ff_vlc_init_from_lengths`):
    /// each code is the one after the previous, as long as the entry's length.
    fn from_lengths(entries: &[(u8, u8)], offset: i32) -> Self {
        let mut code = 0u64;
        let list: Vec<_> = entries
            .iter()
            .map(|&(sym, len)| {
                let c = (code >> (32 - len as u32)) as u32;
                code += 1u64 << (32 - len as u32);
                (c, len, sym as i32 + offset)
            })
            .collect();
        Vlc::new(list.into_iter())
    }

    fn get(&self, gb: &mut Bits) -> Result<i32> {
        let bits = gb.peek(self.max_len);
        let mut n = 0usize;
        for i in 0..self.max_len {
            let c = self.nodes[n][(bits >> (self.max_len - 1 - i) & 1) as usize];
            if c == 0 {
                break;
            }
            if c & LEAF != 0 {
                gb.skip(i as usize + 1);
                return Ok(self.symbols[(c & !LEAF) as usize]);
            }
            n = c as usize;
        }
        Err(bad("invalid Huffman code"))
    }
}

/// The run and level of each symbol of a coefficient code, built from the table of how many runs
/// each level has (`init_coef_vlc` of `wma.c`). Symbol 0 is the escape, 1 the end of block.
struct CoefCode {
    vlc: Vlc,
    run: Vec<u16>,
    level: Vec<f32>,
}

impl CoefCode {
    fn new(codes: &[u32], bits: &[u8], levels: &[u16]) -> Self {
        let n = codes.len();
        let vlc = Vlc::new((0..n).map(|i| (codes[i], bits[i], i as i32)));
        let mut run = vec![0u16; n];
        let mut level = vec![0f32; n];
        let (mut i, mut lv, mut k) = (2usize, 1f32, 0usize);
        while i < n {
            let l = levels[k] as usize;
            k += 1;
            for j in 0..l {
                if i >= n {
                    break;
                }
                run[i] = j as u16;
                level[i] = lv;
                i += 1;
            }
            lv += 1.0;
        }
        CoefCode { vlc, run, level }
    }
}

/// The log2 of the samples per frame (`ff_wma_get_frame_len_bits` for versions 1 and 2).
fn frame_len_bits_for(sample_rate: u32, version: u32) -> usize {
    if sample_rate <= 16000 {
        9
    } else if sample_rate <= 22050 || (sample_rate <= 32000 && version == 1) {
        10
    } else {
        11
    }
}

/// What the WAVEFORMATEX of an audio stream says: the format tag, channels, rate, average bytes per
/// second, block align, and the codec-specific bytes after it.
pub struct WaveFormat<'a> {
    pub tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub extra: &'a [u8],
}

/// Reads a WAVEFORMATEX (at least 16 bytes; with the `cbSize` extension when 18 or more).
pub fn parse_waveformat(d: &[u8]) -> Result<WaveFormat<'_>> {
    if d.len() < 16 {
        return Err(bad("the WAVEFORMATEX is shorter than 16 bytes"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    let extra = if d.len() >= 18 { &d[18..d.len().min(18 + u16_at(16) as usize)] } else { &d[16..16] };
    Ok(WaveFormat { tag: u16_at(0), channels: u16_at(2), sample_rate: u32_at(4), avg_bytes_per_sec: u32_at(8), block_align: u16_at(12), extra })
}

/// Fills one exponent band's worth of `row` with `v`, as `decode_exp_vlc` does (a band of size 0 is
/// four wide there).
fn fill_band(row: &mut [f32], q: &mut usize, size: u16, v: f32) {
    let count = if size == 0 { 4 } else { size as usize };
    for _ in 0..count {
        if let Some(slot) = row.get_mut(*q) {
            *slot = v;
        }
        *q += 1;
    }
}

/// The WMA decoder: each packet is one superframe of `block_align` bytes.
pub struct WmaDecoder {
    version: u32,
    channels: usize,
    sample_rate: u32,
    block_align: usize,
    use_bit_reservoir: bool,
    use_variable_block_len: bool,
    use_exp_vlc: bool,
    use_noise_coding: bool,
    byte_offset_bits: u32,
    frame_len: usize,
    frame_len_bits: usize,
    nb_block_sizes: usize,
    exponent_bands: [[u16; 25]; BLOCK_NB_SIZES],
    high_band_start: [i64; BLOCK_NB_SIZES],
    coefs_start: usize,
    coefs_end: [i64; BLOCK_NB_SIZES],
    exponent_high_sizes: [usize; BLOCK_NB_SIZES],
    exponent_high_bands: [[i64; 25]; BLOCK_NB_SIZES],
    exp_vlc: Option<Vlc>,
    hgain_vlc: Option<Vlc>,
    coef: [CoefCode; 2],
    mdct: Vec<Imdct>,
    windows: Vec<Vec<f32>>,
    noise_table: Vec<f32>,
    noise_index: usize,
    noise_mult: f32,
    lsp_cos_table: Vec<f32>,
    lsp_pow_e_table: Vec<f32>,
    lsp_pow_m_table1: Vec<f32>,
    lsp_pow_m_table2: Vec<f32>,
    // block state
    reset_block_lengths: bool,
    block_len_bits: usize,
    next_block_len_bits: usize,
    prev_block_len_bits: usize,
    block_len: usize,
    block_pos: usize,
    ms_stereo: bool,
    channel_coded: [bool; 2],
    exponents_bsize: [usize; 2],
    exponents: [Vec<f32>; 2],
    max_exponent: [f32; 2],
    coefs1: [Vec<f32>; 2],
    coefs: [Vec<f32>; 2],
    output: Vec<f32>,
    frame_out: [Vec<f32>; 2],
    high_band_coded: [[bool; 25]; 2],
    high_band_values: [[i32; 25]; 2],
    exponents_initialized: [bool; 2],
    // the bit reservoir
    last_superframe: Vec<u8>,
    last_bitoffset: usize,
    eof_done: bool,
    started: bool,
    next_pts: i64,
    /// Output samples (per channel) still to drop: the decoder's delay of two frames, at the start and after a reset.
    skip: usize,
    /// Time units (the stream's time base) per output sample, for the pts of a frame trimmed at its start.
    units_per_sample: f64,
}

impl WmaDecoder {
    /// A decoder for a stream whose `extradata` is its WAVEFORMATEX.
    pub fn new(s: &StreamInfo) -> Result<Self> {
        let version = match s.codec {
            CodecId::WmaV1 => 1,
            CodecId::WmaV2 => 2,
            c => return Err(Error::new(ErrorCode::UnsupportedCodec, format!("{} is not a WMA codec", c.name()))),
        };
        let StreamParams::Audio { sample_rate, channels } = s.params else {
            return Err(bad("not an audio stream"));
        };
        let wf = parse_waveformat(&s.extradata).map_err(|_| bad("the stream carries no WAVEFORMATEX in its extradata"))?;
        let mut d = Self::from_parts(version, sample_rate, channels, wf.avg_bytes_per_sec as u64 * 8, wf.block_align as usize, wf.extra)?;
        d.units_per_sample = s.time_base.den as f64 / (sample_rate as f64 * s.time_base.num.max(1) as f64);
        Ok(d)
    }

    /// A decoder from the stream's parameters: `extra` is what follows the WAVEFORMATEX proper (4
    /// bytes for version 1, 6 for version 2).
    pub fn from_parts(version: u32, sample_rate: u32, channels: u16, bit_rate: u64, block_align: usize, extra: &[u8]) -> Result<Self> {
        if block_align == 0 {
            return Err(bad("block_align is not set"));
        }
        if sample_rate == 0 || sample_rate > 50000 || channels == 0 || channels > 2 || bit_rate == 0 {
            return Err(bad(format!("{channels} channels at {sample_rate} Hz and {bit_rate} bit/s are not a stream this decoder takes")));
        }
        let channels = channels as usize;
        // extract the flags
        let word = |at: usize| extra.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        let flags2 = if version == 1 { word(2) } else { word(4) };
        let use_exp_vlc = flags2 & 1 != 0;
        let use_bit_reservoir = flags2 & 2 != 0;
        let mut use_variable_block_len = flags2 & 4 != 0;
        if version == 2 && extra.len() >= 8 && word(4) == 0x0d && use_variable_block_len {
            use_variable_block_len = false; // FFmpeg's fix for a file that decodes wrongly with it
        }

        // ff_wma_init
        let frame_len_bits = frame_len_bits_for(sample_rate, version);
        let frame_len = 1usize << frame_len_bits;
        let nb_block_sizes = if use_variable_block_len {
            let mut nb = ((flags2 as usize >> 3) & 3) + 1;
            if bit_rate / channels as u64 >= 32000 {
                nb += 2;
            }
            nb.min(frame_len_bits - BLOCK_MIN_BITS) + 1
        } else {
            1
        };
        let mut use_noise_coding = true;
        let mut high_freq = (sample_rate as f64 * 0.5) as f32;
        // with version 2 the rates are normalised
        let mut sample_rate1 = sample_rate;
        if version == 2 {
            for r in [44100, 22050, 16000, 11025, 8000] {
                if sample_rate1 >= r {
                    sample_rate1 = r;
                    break;
                }
            }
        }
        let bps = bit_rate as f32 / (channels as f32 * sample_rate as f32);
        let offset_bytes = ((bps * frame_len as f32) as f64 / 8.0 + 0.5) as u32;
        let byte_offset_bits = offset_bytes.max(1).ilog2() + 2;
        if byte_offset_bits + 3 > 25 {
            return Err(bad(format!("byte_offset_bits {byte_offset_bits} is too large")));
        }
        // the high frequency value, and whether noise coding is used
        let mut bps1 = bps;
        if channels == 2 {
            bps1 = (bps as f64 * 1.6) as f32;
        }
        let (bps_d, bps1_d) = (bps as f64, bps1 as f64);
        let mut scale_high = |m: f64| high_freq = (high_freq as f64 * m) as f32;
        if sample_rate1 == 44100 {
            if bps1_d >= 0.61 {
                use_noise_coding = false;
            } else {
                scale_high(0.4);
            }
        } else if sample_rate1 == 22050 {
            if bps1_d >= 1.16 {
                use_noise_coding = false;
            } else if bps1_d >= 0.72 {
                scale_high(0.7);
            } else {
                scale_high(0.6);
            }
        } else if sample_rate1 == 16000 {
            if bps_d > 0.5 {
                scale_high(0.5);
            } else {
                scale_high(0.3);
            }
        } else if sample_rate1 == 11025 {
            scale_high(0.7);
        } else if sample_rate1 == 8000 {
            if bps_d <= 0.625 {
                scale_high(0.5);
            } else if bps_d > 0.75 {
                use_noise_coding = false;
            } else {
                scale_high(0.65);
            }
        } else if bps_d >= 0.8 {
            scale_high(0.75);
        } else if bps_d >= 0.6 {
            scale_high(0.6);
        } else {
            scale_high(0.5);
        }

        // the scale factor band sizes of each block size
        let coefs_start = if version == 1 { 3 } else { 0 };
        let mut exponent_sizes = [0usize; BLOCK_NB_SIZES];
        let mut exponent_bands = [[0u16; 25]; BLOCK_NB_SIZES];
        let mut high_band_start = [0i64; BLOCK_NB_SIZES];
        let mut coefs_end = [0i64; BLOCK_NB_SIZES];
        let mut exponent_high_sizes = [0usize; BLOCK_NB_SIZES];
        let mut exponent_high_bands = [[0i64; 25]; BLOCK_NB_SIZES];
        for k in 0..nb_block_sizes {
            let block_len = (frame_len >> k) as i64;
            if version == 1 {
                // (as in FFmpeg, every block size writes row 0)
                let mut lpos = 0i64;
                let mut i = 0;
                while i < 25 {
                    let a = CRITICAL_FREQS[i] as i64;
                    let b = sample_rate as i64;
                    let pos = ((block_len * 2 * a + (b >> 1)) / b).min(block_len);
                    exponent_bands[0][i] = (pos - lpos) as u16;
                    i += 1;
                    if pos >= block_len {
                        break;
                    }
                    lpos = pos;
                }
                exponent_sizes[0] = i;
            } else {
                // hardcoded tables for the larger rates
                let a = frame_len_bits as i64 - BLOCK_MIN_BITS as i64 - k as i64;
                let table = if (0..3).contains(&a) {
                    let a = a as usize;
                    if sample_rate >= 44100 {
                        Some(&EXPONENT_BAND_44100[a])
                    } else if sample_rate >= 32000 {
                        Some(&EXPONENT_BAND_32000[a])
                    } else if sample_rate >= 22050 {
                        Some(&EXPONENT_BAND_22050[a])
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(t) = table {
                    let n = t[0] as usize;
                    for i in 0..n {
                        exponent_bands[k][i] = t[1 + i] as u16;
                    }
                    exponent_sizes[k] = n;
                } else {
                    let (mut j, mut lpos) = (0usize, 0i64);
                    for &freq in CRITICAL_FREQS.iter() {
                        let b = sample_rate as i64;
                        let pos = (((block_len * 2 * freq as i64 + (b << 1)) / (4 * b)) << 2).min(block_len);
                        if pos > lpos {
                            exponent_bands[k][j] = (pos - lpos) as u16;
                            j += 1;
                        }
                        if pos >= block_len {
                            break;
                        }
                        lpos = pos;
                    }
                    exponent_sizes[k] = j;
                }
            }
            // the number of coefficients, and where the high bands start
            coefs_end[k] = ((frame_len - (frame_len * 9) / 100) >> k) as i64;
            high_band_start[k] = (((block_len as f32 * 2.0 * high_freq) / sample_rate as f32) as f64 + 0.5) as i64;
            let (mut j, mut pos) = (0usize, 0i64);
            for i in 0..exponent_sizes[k] {
                let mut start = pos;
                pos += exponent_bands[k][i] as i64;
                let mut end = pos;
                if start < high_band_start[k] {
                    start = high_band_start[k];
                }
                if end > coefs_end[k] {
                    end = coefs_end[k];
                }
                if end > start {
                    exponent_high_bands[k][j] = end - start;
                    j += 1;
                }
            }
            exponent_high_sizes[k] = j;
        }

        // MDCT windows: simple sine windows
        let windows = (0..nb_block_sizes)
            .map(|i| {
                let n = 1usize << (frame_len_bits - i);
                (0..n).map(|j| ((j as f64 + 0.5) / (2.0 * n as f64) * PI).sin() as f32).collect()
            })
            .collect();
        let mdct = (0..nb_block_sizes).map(|i| Imdct::new(1 << (frame_len_bits - i))).collect();

        // the noise generator
        let noise_mult: f32 = if use_exp_vlc { 0.02 } else { 0.04 };
        let mut noise_table = Vec::new();
        if use_noise_coding {
            let mut seed = 1u32;
            let norm = ((1.0 / (1u64 << 31) as f64) * 3f64.sqrt() * noise_mult as f64) as f32;
            for _ in 0..NOISE_TAB_SIZE {
                seed = seed.wrapping_mul(314159).wrapping_add(1);
                // FFmpeg's decoder output has this table's samples with the opposite sign to the
                // plain `(int) seed * norm` (measured against its float decode), so the sign is flipped
                noise_table.push(-(seed as i32 as f32 * norm));
            }
        }

        // the coefficient code tables
        let mut coef_table = 2;
        if sample_rate >= 32000 {
            if bps1_d < 0.72 {
                coef_table = 0;
            } else if bps1_d < 1.16 {
                coef_table = 1;
            }
        }
        let all = [
            (&COEF0_CODES[..], &COEF0_BITS[..], &COEF0_LEVELS[..]),
            (&COEF1_CODES[..], &COEF1_BITS[..], &COEF1_LEVELS[..]),
            (&COEF2_CODES[..], &COEF2_BITS[..], &COEF2_LEVELS[..]),
            (&COEF3_CODES[..], &COEF3_BITS[..], &COEF3_LEVELS[..]),
            (&COEF4_CODES[..], &COEF4_BITS[..], &COEF4_LEVELS[..]),
            (&COEF5_CODES[..], &COEF5_BITS[..], &COEF5_LEVELS[..]),
        ];
        let make = |i: usize| CoefCode::new(all[i].0, all[i].1, all[i].2);
        let coef = [make(coef_table * 2), make(coef_table * 2 + 1)];

        let hgain_vlc = use_noise_coding.then(|| Vlc::from_lengths(&HGAIN_HUFF, -18));
        let exp_vlc = use_exp_vlc.then(|| Vlc::new((0..121).map(|i| (EXP_CODES[i], EXP_BITS[i], i as i32))));

        // the lsp_to_curve tables
        let (mut lsp_cos_table, mut e_table, mut m1, mut m2) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        if !use_exp_vlc {
            let wdel = PI / frame_len as f64;
            lsp_cos_table = (0..frame_len).map(|i| (2.0 * (wdel * i as f64).cos()) as f32).collect();
            e_table = (0..256).map(|i| (((i - 126) as f32 * -0.25) as f64).exp2() as f32).collect();
            m1 = vec![0f32; 1 << LSP_POW_BITS];
            m2 = vec![0f32; 1 << LSP_POW_BITS];
            let mut b = 1.0f32;
            for i in (0..1usize << LSP_POW_BITS).rev() {
                let m = (1usize << LSP_POW_BITS) + i;
                let a = (m as f64 * (0.5 / (1 << LSP_POW_BITS) as f64)) as f32;
                let a = (1.0 / (a as f64).sqrt().sqrt()) as f32;
                m1[i] = 2.0 * a - b;
                m2[i] = b - a;
                b = a;
            }
        }

        let zero = || vec![0f32; BLOCK_MAX_SIZE];
        let row = || vec![0f32; BLOCK_MAX_SIZE + EXP_SLACK];
        Ok(WmaDecoder {
            version,
            channels,
            sample_rate,
            block_align,
            use_bit_reservoir,
            use_variable_block_len,
            use_exp_vlc,
            use_noise_coding,
            byte_offset_bits,
            frame_len,
            frame_len_bits,
            nb_block_sizes,
            exponent_bands,
            high_band_start,
            coefs_start,
            coefs_end,
            exponent_high_sizes,
            exponent_high_bands,
            exp_vlc,
            hgain_vlc,
            coef,
            mdct,
            windows,
            noise_table,
            noise_index: 0,
            noise_mult,
            lsp_cos_table,
            lsp_pow_e_table: e_table,
            lsp_pow_m_table1: m1,
            lsp_pow_m_table2: m2,
            reset_block_lengths: true,
            block_len_bits: frame_len_bits,
            next_block_len_bits: frame_len_bits,
            prev_block_len_bits: frame_len_bits,
            block_len: 0,
            block_pos: 0,
            ms_stereo: false,
            channel_coded: [false; 2],
            exponents_bsize: [0; 2],
            exponents: [row(), row()],
            max_exponent: [1.0; 2],
            coefs1: [zero(), zero()],
            coefs: [zero(), zero()],
            output: vec![0f32; BLOCK_MAX_SIZE * 2],
            frame_out: [vec![0f32; BLOCK_MAX_SIZE * 2], vec![0f32; BLOCK_MAX_SIZE * 2]],
            high_band_coded: [[false; 25]; 2],
            high_band_values: [[0; 25]; 2],
            exponents_initialized: [false; 2],
            last_superframe: Vec::new(),
            last_bitoffset: 0,
            eof_done: false,
            started: false,
            next_pts: 0,
            skip: 2 * frame_len,
            units_per_sample: 1.0,
        })
    }

    /// The samples in each decoded frame, per channel.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// x^-0.25 from an exponent and mantissa table with linear interpolation (`pow_m1_4`).
    fn pow_m1_4(&self, x: f32) -> f32 {
        let u = x.to_bits();
        let e = (u >> 23) as usize & 0xFF;
        let m = ((u >> (23 - LSP_POW_BITS)) & ((1 << LSP_POW_BITS) - 1)) as usize;
        let t = f32::from_bits(((u << LSP_POW_BITS) & ((1 << 23) - 1)) | (127 << 23));
        let a = self.lsp_pow_m_table1[m];
        let b = self.lsp_pow_m_table2[m];
        self.lsp_pow_e_table[e] * (a + b * t)
    }

    /// Exponents coded as LSP coefficients (the same idea as Vorbis).
    fn decode_exp_lsp(&mut self, gb: &mut Bits, ch: usize) {
        let mut lsp = [0f32; NB_LSP_COEFS];
        for (i, l) in lsp.iter_mut().enumerate() {
            let val = if i == 0 || i >= 8 { gb.read(3) } else { gb.read(4) } as usize;
            *l = LSP_CODEBOOK[i][val];
        }
        let mut val_max = 0f32;
        for i in 0..self.block_len {
            let (mut p, mut q) = (0.5f32, 0.5f32);
            let w = self.lsp_cos_table[i];
            let mut j = 1;
            while j < NB_LSP_COEFS {
                q *= w - lsp[j - 1];
                p *= w - lsp[j];
                j += 2;
            }
            let t = p * (2.0 - w);
            p *= t;
            let t = q * (2.0 + w);
            q *= t;
            let v = self.pow_m1_4(p + q);
            if v > val_max {
                val_max = v;
            }
            self.exponents[ch][i] = v;
        }
        self.max_exponent[ch] = val_max;
    }

    /// Exponents coded as Huffman-coded deltas.
    fn decode_exp_vlc(&mut self, gb: &mut Bits, ch: usize) -> Result<()> {
        let bsize = self.frame_len_bits - self.block_len_bits;
        let bands = self.exponent_bands[bsize];
        let pow = |e: i32| 10f64.powf(e as f64 / 16.0) as f32;
        let (mut band, mut q) = (0usize, 0usize);
        let mut max_scale = 0f32;
        let mut row = std::mem::take(&mut self.exponents[ch]);
        let mut last_exp = 36i32;
        if self.version == 1 {
            last_exp = gb.read(5) as i32 + 10;
            let v = pow(last_exp);
            max_scale = v;
            fill_band(&mut row, &mut q, bands[0], v);
            band = 1;
        }
        let vlc = self.exp_vlc.as_ref().expect("exponent code");
        let mut result = Ok(());
        while q < self.block_len {
            let code = match vlc.get(gb) {
                Ok(c) => c,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            // this offset is the same as MPEG-4 AAC's
            last_exp += code - 60;
            if (last_exp + 60) as u32 >= 156 {
                result = Err(bad(format!("exponent out of range: {last_exp}")));
                break;
            }
            let v = pow(last_exp);
            if v > max_scale {
                max_scale = v;
            }
            let Some(&size) = bands.get(band) else {
                result = Err(bad("the exponent bands run out"));
                break;
            };
            band += 1;
            fill_band(&mut row, &mut q, size, v);
        }
        self.exponents[ch] = row;
        self.max_exponent[ch] = max_scale;
        result
    }

    /// Windows the IMDCT output and adds it into the frame buffer of `ch` at `index` (`wma_window`).
    fn window(&mut self, ch: usize, index: usize) {
        let block_len = self.block_len;
        let out = &mut self.frame_out[ch][index..index + 2 * block_len];
        let (out_l, out_r) = out.split_at_mut(block_len);
        let (in_l, in_r) = self.output[..2 * block_len].split_at(block_len);
        // the left part
        if self.block_len_bits <= self.prev_block_len_bits {
            let w = &self.windows[self.frame_len_bits - self.block_len_bits];
            for i in 0..block_len {
                out_l[i] += in_l[i] * w[i];
            }
        } else {
            let bl = 1usize << self.prev_block_len_bits;
            let n = (block_len - bl) / 2;
            let w = &self.windows[self.frame_len_bits - self.prev_block_len_bits];
            for i in 0..bl {
                out_l[n + i] += in_l[n + i] * w[i];
            }
            out_l[n + bl..n + bl + n].copy_from_slice(&in_l[n + bl..n + bl + n]);
        }
        // the right part
        if self.block_len_bits <= self.next_block_len_bits {
            let w = &self.windows[self.frame_len_bits - self.block_len_bits];
            for i in 0..block_len {
                out_r[i] = in_r[i] * w[block_len - 1 - i];
            }
        } else {
            let bl = 1usize << self.next_block_len_bits;
            let n = (block_len - bl) / 2;
            let w = &self.windows[self.frame_len_bits - self.next_block_len_bits];
            out_r[..n].copy_from_slice(&in_r[..n]);
            for i in 0..bl {
                out_r[n + i] = in_r[n + i] * w[bl - 1 - i];
            }
            out_r[n + bl..n + bl + n].fill(0.0);
        }
    }

    /// Reads one block. Returns true when it is the last of the frame.
    fn decode_block(&mut self, gb: &mut Bits) -> Result<bool> {
        let channels = self.channels;
        // the current block length
        if self.use_variable_block_len {
            let m = self.nb_block_sizes - 1;
            let n = if m == 0 { 0 } else { m.ilog2() } + 1;
            if self.reset_block_lengths {
                self.reset_block_lengths = false;
                let v = gb.read(n) as usize;
                if v >= self.nb_block_sizes {
                    return Err(bad("prev_block_len_bits out of range"));
                }
                self.prev_block_len_bits = self.frame_len_bits - v;
                let v = gb.read(n) as usize;
                if v >= self.nb_block_sizes {
                    return Err(bad("block_len_bits out of range"));
                }
                self.block_len_bits = self.frame_len_bits - v;
            } else {
                // update the block lengths
                self.prev_block_len_bits = self.block_len_bits;
                self.block_len_bits = self.next_block_len_bits;
            }
            let v = gb.read(n) as usize;
            if v >= self.nb_block_sizes {
                return Err(bad("next_block_len_bits out of range"));
            }
            self.next_block_len_bits = self.frame_len_bits - v;
        } else {
            self.next_block_len_bits = self.frame_len_bits;
            self.prev_block_len_bits = self.frame_len_bits;
            self.block_len_bits = self.frame_len_bits;
        }
        if self.frame_len_bits - self.block_len_bits >= self.nb_block_sizes {
            return Err(bad("block_len_bits not initialized to a valid value"));
        }
        // the block length must fit the frame
        self.block_len = 1 << self.block_len_bits;
        if self.block_pos + self.block_len > self.frame_len {
            return Err(bad("frame_len overflow"));
        }
        if channels == 2 {
            self.ms_stereo = gb.read(1) != 0;
        }
        let mut any = false;
        for ch in 0..channels {
            let a = gb.read(1) != 0;
            self.channel_coded[ch] = a;
            any |= a;
        }
        let bsize = self.frame_len_bits - self.block_len_bits;
        let block_len = self.block_len;

        if any {
            // the total gain gives the bits of a coefficient escape
            let mut total_gain = 1i32;
            loop {
                if gb.left() < 7 {
                    return Err(bad("total_gain overread"));
                }
                let a = gb.read(7) as i32;
                total_gain += a;
                if a != 127 {
                    break;
                }
            }
            let coef_nb_bits = match total_gain {
                ..=14 => 13,
                15..=31 => 12,
                32..=39 => 11,
                40..=44 => 10,
                _ => 9,
            };

            // the number of coefficients
            let n = self.coefs_end[bsize] - self.coefs_start as i64;
            let mut nb_coefs = [n; 2];

            // complex coding
            if self.use_noise_coding {
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        for i in 0..self.exponent_high_sizes[bsize] {
                            let a = gb.read(1) != 0;
                            self.high_band_coded[ch][i] = a;
                            // with noise coding the coefficients are not transmitted
                            if a {
                                nb_coefs[ch] -= self.exponent_high_bands[bsize][i];
                            }
                        }
                    }
                }
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        let mut val = i32::MIN;
                        for i in 0..self.exponent_high_sizes[bsize] {
                            if self.high_band_coded[ch][i] {
                                if val == i32::MIN {
                                    val = gb.read(7) as i32 - 19;
                                } else {
                                    val += self.hgain_vlc.as_ref().expect("gain code").get(gb)?;
                                }
                                self.high_band_values[ch][i] = val;
                            }
                        }
                    }
                }
            }

            // exponents can be reused in short blocks
            if self.block_len_bits == self.frame_len_bits || gb.read(1) != 0 {
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        if self.use_exp_vlc {
                            self.decode_exp_vlc(gb, ch)?;
                        } else {
                            self.decode_exp_lsp(gb, ch);
                        }
                        self.exponents_bsize[ch] = bsize;
                        self.exponents_initialized[ch] = true;
                    }
                }
            }
            for ch in 0..channels {
                if self.channel_coded[ch] && !self.exponents_initialized[ch] {
                    return Err(bad("a block reuses exponents that were never read"));
                }
            }

            // the spectral coefficients: run-level coding
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    // special codes are used for mid/side stereo: there is potentially less energy there
                    let tindex = (ch == 1 && self.ms_stereo) as usize;
                    let mut ptr = std::mem::take(&mut self.coefs1[ch]);
                    ptr[..block_len].fill(0.0);
                    let r = self.run_level_decode(gb, tindex, &mut ptr, nb_coefs[ch].max(0) as usize, coef_nb_bits);
                    self.coefs1[ch] = ptr;
                    r?;
                }
                if self.version == 1 && channels >= 2 {
                    gb.align();
                }
            }

            // normalise
            let n4 = block_len / 2;
            let mut mdct_norm = 1.0 / n4 as f32;
            if self.version == 1 {
                mdct_norm *= (n4 as f64).sqrt() as f32;
            }

            // the MDCT coefficients
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    self.make_coefs(ch, bsize, total_gain, mdct_norm, &nb_coefs);
                }
            }

            if self.ms_stereo && self.channel_coded[1] {
                // the nominal case for mid/side stereo: done before the MDCT
                if !self.channel_coded[0] {
                    self.coefs[0][..block_len].fill(0.0);
                    self.channel_coded[0] = true;
                }
                let (c0, c1) = self.coefs.split_at_mut(1);
                for i in 0..block_len {
                    let (a, b) = (c0[0][i], c1[0][i]);
                    c0[0][i] = a + b;
                    c1[0][i] = a - b;
                }
            }
        }

        for ch in 0..channels {
            let n4 = block_len / 2;
            if self.channel_coded[ch] {
                self.mdct[bsize].run(&self.coefs[ch][..block_len], &mut self.output[..2 * block_len], 1.0 / 32768.0);
            } else if !(self.ms_stereo && ch == 1) {
                self.output.fill(0.0);
            }
            // multiply by the window and add in the frame
            self.window(ch, self.frame_len / 2 + self.block_pos - n4);
        }

        self.block_pos += block_len;
        Ok(self.block_pos >= self.frame_len)
    }

    /// The run-level coded coefficients of one channel (`ff_wma_run_level_decode`, version 0).
    fn run_level_decode(&self, gb: &mut Bits, tindex: usize, ptr: &mut [f32], num_coefs: usize, coef_nb_bits: u32) -> Result<()> {
        let table = &self.coef[tindex];
        let mask = self.block_len - 1;
        let mut offset = 0usize;
        while offset < num_coefs {
            let code = table.vlc.get(gb)?;
            if code > 1 {
                // a normal code
                offset += table.run[code as usize] as usize;
                let level = table.level[code as usize];
                ptr[offset & mask] = if gb.read(1) != 0 { -level } else { level };
            } else if code == 1 {
                break; // the end of the block
            } else {
                // an escape
                let level = gb.read(coef_nb_bits) as f32;
                offset += gb.read(self.frame_len_bits as u32) as usize;
                ptr[offset & mask] = if gb.read(1) != 0 { -level } else { level };
            }
            offset += 1;
        }
        // the end of block can be omitted
        if offset > num_coefs {
            return Err(bad(format!("overflow ({offset} > {num_coefs}) in spectral RLE")));
        }
        Ok(())
    }

    /// Scales one channel's coefficients by their exponents and fills in the noise.
    fn make_coefs(&mut self, ch: usize, bsize: usize, total_gain: i32, mdct_norm: f32, nb_coefs: &[i64; 2]) {
        let block_len = self.block_len;
        let esize = self.exponents_bsize[ch];
        let exp = &self.exponents[ch];
        // the exponent at a signed position of the stored row
        let ex = |idx: i64| -> f32 { if idx < 0 { 0.0 } else { exp.get(idx as usize).copied().unwrap_or(0.0) } };
        // the position in the row of coefficient `i` of this block
        let at = |i: i64| -> i64 { (i << bsize) >> esize };
        let mut mult = (10f64.powf(total_gain as f64 * 0.05) as f32) / self.max_exponent[ch];
        mult *= mdct_norm;
        let coefs1 = &self.coefs1[ch];
        let mut out: Vec<f32> = Vec::with_capacity(block_len);
        if self.use_noise_coding {
            let mut noise_index = self.noise_index;
            let noise_table = &self.noise_table;
            let mut noise = || {
                let v = noise_table[noise_index];
                noise_index = (noise_index + 1) & (NOISE_TAB_SIZE - 1);
                v
            };
            let mut mult1 = mult;
            // very low frequencies: noise
            for i in 0..self.coefs_start {
                out.push(noise() * ex(at(i as i64)) * mult1);
            }
            let n1 = self.exponent_high_sizes[bsize];
            // the power of the high bands
            let mut exp_power = [0f32; 25];
            let mut e = at(self.high_band_start[bsize]);
            let mut last_high_band = 0;
            for j in 0..n1 {
                let n = self.exponent_high_bands[bsize][j];
                if self.high_band_coded[ch][j] {
                    let mut e2 = 0f32;
                    for i in 0..n {
                        let v = ex(e + at(i));
                        e2 += v * v;
                    }
                    exp_power[j] = e2 / n as f32;
                    last_high_band = j;
                }
                e += at(n);
            }
            // main and high frequencies
            let mut c1 = 0usize;
            let mut e = at(self.coefs_start as i64);
            for j in -1..n1 as i64 {
                let n = if j < 0 { self.high_band_start[bsize] - self.coefs_start as i64 } else { self.exponent_high_bands[bsize][j as usize] };
                if j >= 0 && self.high_band_coded[ch][j as usize] {
                    // noise of the given power
                    mult1 = (exp_power[j as usize] / exp_power[last_high_band]).sqrt();
                    mult1 *= 10f64.powf(self.high_band_values[ch][j as usize] as f64 * 0.05) as f32;
                    mult1 /= self.max_exponent[ch] * self.noise_mult;
                    mult1 *= mdct_norm;
                    for i in 0..n {
                        out.push(noise() * ex(e + at(i)) * mult1);
                    }
                } else {
                    // coded values plus a little noise
                    for i in 0..n {
                        let c = coefs1.get(c1).copied().unwrap_or(0.0);
                        c1 += 1;
                        out.push((c + noise()) * ex(e + at(i)) * mult);
                    }
                }
                e += at(n.max(0));
            }
            // very high frequencies: noise
            let n = block_len as i64 - self.coefs_end[bsize];
            mult1 = mult * ex(e + ((-(1i64 << bsize)) >> esize));
            for _ in 0..n.max(0) {
                out.push(noise() * mult1);
            }
            self.noise_index = noise_index;
        } else {
            out.resize(self.coefs_start, 0.0);
            for i in 0..nb_coefs[ch].max(0) as usize {
                out.push(coefs1[i] * ex(at(i as i64)) * mult);
            }
            out.resize(out.len() + (block_len as i64 - self.coefs_end[bsize]).max(0) as usize, 0.0);
        }
        out.resize(block_len, 0.0);
        self.coefs[ch][..block_len].copy_from_slice(&out);
    }

    /// Decodes a frame of `frame_len` samples per channel, appending them to `out`.
    fn decode_frame(&mut self, gb: &mut Bits, out: &mut [Vec<f32>; 2]) -> Result<()> {
        self.block_pos = 0;
        while !self.decode_block(gb)? {}
        let n = self.frame_len;
        for ch in 0..self.channels {
            // copy the current block to the output, and prepare for the next
            out[ch].extend_from_slice(&self.frame_out[ch][..n]);
            self.frame_out[ch].copy_within(n..2 * n, 0);
            self.frame_out[ch][n..2 * n].fill(0.0);
        }
        Ok(())
    }

    /// Decodes one superframe (`wma_decode_superframe`); `None` when it only fed the reservoir.
    fn decode_superframe(&mut self, buf: &[u8]) -> Result<Option<[Vec<f32>; 2]>> {
        if buf.len() < self.block_align {
            return Err(bad(format!("input packet size too small ({} < {})", buf.len(), self.block_align)));
        }
        let buf = &buf[..self.block_align];
        let buf_size = buf.len();
        let mut out: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
        let mut gb = Bits::new(buf, buf_size * 8);
        let mut nb_frames = 1usize;
        if self.use_bit_reservoir {
            // the superframe header
            gb.skip(4); // the superframe index
            let n = gb.read(4) as i32 - self.last_superframe.is_empty() as i32;
            if n <= 0 {
                if n < 0 || gb.left() <= 8 {
                    return Err(bad(format!("nb_frames is {n} with {} bits left", gb.left())));
                }
                if self.last_superframe.len() + buf_size - 1 > MAX_CODED_SUPERFRAME_SIZE {
                    self.last_superframe.clear();
                    return Err(bad("the bit reservoir overflows"));
                }
                self.last_superframe.extend_from_slice(&buf[1..]);
                return Ok(None);
            }
            nb_frames = n as usize;
        }
        let r = (|| -> Result<()> {
            if self.use_bit_reservoir {
                let bit_offset = gb.read(self.byte_offset_bits + 3) as usize;
                if bit_offset as isize > gb.left() {
                    return Err(bad(format!("invalid last frame bit offset {bit_offset} > {}", gb.left())));
                }
                if !self.last_superframe.is_empty() {
                    // add bit_offset bits to the last frame
                    if self.last_superframe.len() + ((bit_offset + 7) >> 3) > MAX_CODED_SUPERFRAME_SIZE {
                        return Err(bad("the bit reservoir overflows"));
                    }
                    let mut joined = self.last_superframe.clone();
                    let mut len = bit_offset;
                    while len > 7 {
                        joined.push(gb.read(8) as u8);
                        len -= 8;
                    }
                    if len > 0 {
                        joined.push((gb.read(len as u32) << (8 - len)) as u8);
                    }
                    // bit_offset bits into the last frame
                    let mut gb2 = Bits::new(&joined, self.last_superframe.len() * 8 + bit_offset);
                    // skip the unused bits
                    if self.last_bitoffset > 0 {
                        gb2.skip(self.last_bitoffset);
                    }
                    // this frame is stored in the last superframe and in the current one
                    self.decode_frame(&mut gb2, &mut out)?;
                    nb_frames -= 1;
                }
                // read each frame starting from bit_offset
                let head = bit_offset + 4 + 4 + self.byte_offset_bits as usize + 3;
                if head >= MAX_CODED_SUPERFRAME_SIZE * 8 || head > buf_size * 8 {
                    return Err(bad("the first frame starts beyond the packet"));
                }
                let mut gb3 = Bits::new(&buf[head >> 3..], (buf_size - (head >> 3)) * 8);
                gb3.skip(head & 7);
                self.reset_block_lengths = true;
                for _ in 0..nb_frames {
                    self.decode_frame(&mut gb3, &mut out)?;
                }
                // the end of the frame goes in the last frame buffer
                let pos = gb3.pos + (head & !7);
                self.last_bitoffset = pos & 7;
                let pos = pos >> 3;
                if pos > buf_size {
                    return Err(bad("the last frame ends beyond the packet"));
                }
                self.last_superframe = buf[pos..].to_vec();
            } else {
                // a single frame
                self.decode_frame(&mut gb, &mut out)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            // on an error the bit reservoir is reset
            self.last_superframe.clear();
            return Err(e);
        }
        Ok(Some(out))
    }

    /// Interleaves the planes into a frame, first dropping the samples the decoder's delay still owes.
    fn interleave(&mut self, planes: &[Vec<f32>; 2], pts: i64) -> Option<Frame> {
        let drop = self.skip.min(planes[0].len());
        self.skip -= drop;
        let pts = pts.saturating_add((drop as f64 * self.units_per_sample).round() as i64);
        let planes = [&planes[0][drop..], &planes[1][drop.min(planes[1].len())..]];
        let n = planes[0].len();
        if n == 0 {
            return None;
        }
        let mut samples = Vec::with_capacity(n * self.channels);
        for i in 0..n {
            for plane in planes.iter().take(self.channels) {
                samples.push(plane[i] as f64);
            }
        }
        Some(Frame::Audio(AudioFrame { sample_rate: self.sample_rate, channels: self.channels as u16, pts, samples }))
    }
}

impl Decoder for WmaDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        self.started = true;
        self.eof_done = false;
        self.next_pts = packet.pts.saturating_add(packet.duration);
        match self.decode_superframe(&packet.data)? {
            Some(planes) if !planes[0].is_empty() => Ok(self.interleave(&planes, packet.pts).into_iter().collect()),
            _ => Ok(Vec::new()),
        }
    }

    /// The overlap tail: the half-frame the last block's window still holds, which FFmpeg also
    /// outputs at the end of the stream.
    fn flush(&mut self) -> Result<Vec<Frame>> {
        if !self.started || self.eof_done {
            return Ok(Vec::new());
        }
        self.eof_done = true;
        self.last_superframe.clear();
        let tail = [self.frame_out[0][..self.frame_len].to_vec(), self.frame_out[1][..self.frame_len].to_vec()];
        Ok(self.interleave(&tail, self.next_pts).into_iter().collect())
    }

    fn reset(&mut self) -> bool {
        self.last_superframe.clear();
        self.last_bitoffset = 0;
        self.eof_done = false;
        self.started = false;
        self.skip = 2 * self.frame_len;
        for f in &mut self.frame_out {
            f.fill(0.0);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(version: u32, rate: u32, channels: u16, kbps: u64, flags: u16) -> WmaDecoder {
        let mut extra = vec![0u8; if version == 1 { 4 } else { 6 }];
        let at = extra.len() - 2;
        extra[at..].copy_from_slice(&flags.to_le_bytes());
        WmaDecoder::from_parts(version, rate, channels, kbps * 1000, 743, &extra).unwrap()
    }

    #[test]
    fn every_code_table_decodes_its_own_codes() {
        let tables = [(&COEF0_CODES[..], &COEF0_BITS[..]), (&COEF1_CODES[..], &COEF1_BITS[..]), (&COEF2_CODES[..], &COEF2_BITS[..]), (&COEF3_CODES[..], &COEF3_BITS[..]), (&COEF4_CODES[..], &COEF4_BITS[..]), (&COEF5_CODES[..], &COEF5_BITS[..]), (&EXP_CODES[..], &EXP_BITS[..])];
        for (codes, bits) in tables {
            let vlc = Vlc::new((0..codes.len()).map(|i| (codes[i], bits[i], i as i32)));
            for i in 0..codes.len() {
                // the code, padded with ones, then the code followed by what comes next
                let v = (codes[i] as u64) << (40 - bits[i] as u64) | ((1u64 << (40 - bits[i] as u64)) - 1);
                let bytes = v.to_be_bytes();
                let mut gb = Bits::new(&bytes[3..], 40);
                assert_eq!(vlc.get(&mut gb).unwrap(), i as i32);
                assert_eq!(gb.pos, bits[i] as usize);
            }
        }
        // the gain code: 37 symbols, offset -18
        let v = Vlc::from_lengths(&HGAIN_HUFF, -18);
        let mut gb = Bits::new(&[0, 0, 0, 0], 32);
        assert_eq!(v.get(&mut gb).unwrap(), HGAIN_HUFF[0].0 as i32 - 18);
    }

    #[test]
    fn parameters_are_chosen_as_ffmpeg_does() {
        let d = decoder(2, 44100, 2, 128, 0);
        assert_eq!((d.frame_len, d.nb_block_sizes, d.use_noise_coding), (2048, 1, false));
        let d = decoder(2, 44100, 2, 32, 0);
        assert!(d.use_noise_coding);
        let d = decoder(1, 22050, 1, 32, 4);
        assert_eq!((d.frame_len, d.coefs_start, d.nb_block_sizes), (1024, 3, 4));
        assert!(WmaDecoder::from_parts(2, 44100, 6, 128000, 743, &[]).is_err());
        assert!(WmaDecoder::from_parts(2, 96000, 2, 128000, 743, &[]).is_err());
        assert!(WmaDecoder::from_parts(2, 44100, 2, 128000, 0, &[]).is_err());
    }

    #[test]
    fn garbage_packets_are_errors_or_audio_never_panics() {
        let mut state = 7u64;
        for (version, flags) in [(1, 0u16), (1, 6), (2, 0), (2, 3), (2, 7), (2, 0x0d)] {
            for (rate, ch) in [(22050, 1), (44100, 2), (16000, 2)] {
                let mut d = decoder(version, rate, ch, 64, flags);
                for _ in 0..40 {
                    let data: Vec<u8> = (0..743)
                        .map(|_| {
                            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                            (state >> 33) as u8
                        })
                        .collect();
                    let _ = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data });
                }
                assert!(d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: vec![0; 10] }).is_err());
                let _ = d.flush();
            }
        }
    }
}
