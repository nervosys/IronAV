//! MPEG-1/2 Audio Layer II (MP2) decoding.
//!
//! The bit allocation tables (ISO/IEC 11172-3 tables B.2a-d and the MPEG-2 LSF table of 13818-3),
//! the choice among them, the grouped (3, 5 and 9 level) and ungrouped quantizer codes and the scale
//! factor arithmetic follow FFmpeg's `mpegaudiodec_template.c` (`mp_decode_layer2`), `mpegaudiodata.c`
//! and `mpegaudio.c` (`ff_mpa_l2_select_table`); FFmpeg is LGPL-2.1+. The polyphase synthesis filterbank
//! is the Layer III decoder's, so the two layers share their output path. Joint stereo is intensity
//! stereo only: subbands from the bound up carry one set of codes for both channels. The CRC is read
//! past, not checked. Layer I is not decoded.

use super::{bad, conceal, parse_frame, statics, synth, Bits, Header};
use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};

/// The number of subbands in use, per allocation table (FFmpeg's `ff_mpa_sblimit_table`).
pub(super) const SBLIMIT: [usize; 5] = [27, 30, 8, 12, 30];

/// Per quantizer class: the number of levels (2^bits - 1, or 3, 5, 9 for the grouped classes).
pub(super) const QUANT_STEPS: [u32; 17] = [3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535];

/// Per quantizer class: the bits of a code; negative for a grouped class, whose code holds three
/// values in that many bits (FFmpeg's `ff_mpa_quant_bits`).
pub(super) const QUANT_BITS: [i32; 17] = [-5, -7, 3, -10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// The allocation tables, flattened as FFmpeg has them: for each subband, the bits of its allocation
/// index followed by the quantizer class for each allocation index (`1 << bits` entries).
const ALLOC_1: &[u8] = &[
    4, 0, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    4, 0, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    4, 0, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    3, 0, 1, 2, 3, 4, 5, 16, 3, 0, 1, 2, 3, 4, 5, 16,
    2, 0, 1, 16, 2, 0, 1, 16, 2, 0, 1, 16, 2, 0, 1, 16,
    2, 0, 1, 16, 2, 0, 1, 16, 2, 0, 1, 16,
];
const ALLOC_3: &[u8] = &[
    4, 0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    4, 0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
];
const ALLOC_4: &[u8] = &[
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    4, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 3, 0, 1, 3, 4, 5, 6, 7,
    3, 0, 1, 3, 4, 5, 6, 7, 2, 0, 1, 3, 2, 0, 1, 3,
    2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3,
    2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3,
    2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3,
    2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3, 2, 0, 1, 3,
    2, 0, 1, 3,
];
pub(super) const ALLOC: [&[u8]; 5] = [ALLOC_1, ALLOC_1, ALLOC_3, ALLOC_3, ALLOC_4];

/// The allocation table for a frame, from its bit rate per channel, sample rate and version
/// (FFmpeg's `ff_mpa_l2_select_table`).
fn select_table(h: &Header) -> usize {
    select_table_for(h.bitrate, h.channels() as u32, h.sample_rate, h.lsf)
}

/// The allocation table for a bit rate in kbit/s over `channels`, at `sample_rate`, for MPEG-2 LSF or not.
pub(super) fn select_table_for(bitrate: u32, channels: u32, sample_rate: u32, lsf: bool) -> usize {
    if lsf {
        return 4;
    }
    let per_channel = bitrate / channels;
    if (sample_rate == 48000 && per_channel >= 56) || (56..=80).contains(&per_channel) {
        0
    } else if sample_rate != 48000 && per_channel >= 96 {
        1
    } else if sample_rate != 32000 && per_channel <= 48 {
        2
    } else {
        3
    }
}

/// The three codes of one quantizer class. A grouped class packs them into one value, least
/// significant first, base `steps`; the others are three values of `bits` bits each.
fn read_codes(br: &mut Bits, q: usize) -> [u32; 3] {
    let steps = QUANT_STEPS[q];
    let bits = QUANT_BITS[q];
    if bits < 0 {
        let mut v = br.get((-bits) as u32);
        let m0 = v % steps;
        v /= steps;
        let m1 = v % steps;
        v /= steps;
        [m0, m1, v]
    } else {
        [br.get(bits as u32), br.get(bits as u32), br.get(bits as u32)]
    }
}

/// A code's value: the centred level times 4/steps, times the scale factor 2^(-index/3). This is
/// FFmpeg's `l1_unscale` (ungrouped, with `steps` = 2^bits - 1) and `l2_unscale_group` (grouped).
#[inline]
fn requant(code: u32, steps: u32, factor: f32) -> f32 {
    (code as f32 - ((steps - 1) / 2) as f32) * (4.0 / steps as f32) * factor
}

/// The Layer II synthesis state: the decoding of frames, with the filterbank's history per channel.
pub(super) struct Layer2 {
    /// 2^(-i/3) for each scale factor index i (0..63).
    factors: [f32; 64],
    v: Vec<[f32; 1024]>,
    v_off: Vec<usize>,
}

impl Layer2 {
    pub(super) fn new() -> Self {
        Self { factors: std::array::from_fn(|i| (-(i as f32) / 3.0).exp2()), v: vec![[0.0; 1024]; 2], v_off: vec![0; 2] }
    }

    /// Decode one Layer II frame (its header included); 1152 samples per channel, interleaved.
    pub(super) fn decode(&mut self, p: &Packet) -> Result<Frame> {
        let h = parse_frame(&p.data).filter(|h| h.layer == 2).ok_or_else(|| bad("not a Layer II frame header"))?;
        let nch = h.channels() as usize;
        let hdr = 4 + if h.crc { 2 } else { 0 };
        let mut br = Bits { d: &p.data[..h.frame_len.min(p.data.len())], pos: hdr * 8 };

        let tab = select_table(&h);
        let sblimit = SBLIMIT[tab];
        let alloc = ALLOC[tab];
        let bound = (if h.mode == 1 { (h.mode_ext as usize + 1) * 4 } else { sblimit }).min(sblimit);

        // bit allocation: per channel below the bound, shared above it
        let mut bit_alloc = [[0u8; 32]; 2];
        let mut j = 0usize;
        for i in 0..bound {
            let nbal = alloc[j] as usize;
            for ch in 0..nch {
                bit_alloc[ch][i] = br.get(nbal as u32) as u8;
            }
            j += 1 << nbal;
        }
        for i in bound..sblimit {
            let nbal = alloc[j] as usize;
            let v = br.get(nbal as u32) as u8;
            bit_alloc[0][i] = v;
            bit_alloc[1][i] = v;
            j += 1 << nbal;
        }

        // scale factor selectors, then the scale factors themselves: three per subband, or fewer
        // where the selector says they repeat
        let mut scale_code = [[0u8; 32]; 2];
        for i in 0..sblimit {
            for ch in 0..nch {
                if bit_alloc[ch][i] != 0 {
                    scale_code[ch][i] = br.get(2) as u8;
                }
            }
        }
        let mut sf = [[[0u8; 3]; 32]; 2];
        for i in 0..sblimit {
            for ch in 0..nch {
                if bit_alloc[ch][i] == 0 {
                    continue;
                }
                let s = &mut sf[ch][i];
                match scale_code[ch][i] {
                    2 => {
                        s[0] = br.get(6) as u8;
                        s[1] = s[0];
                        s[2] = s[0];
                    }
                    1 => {
                        s[0] = br.get(6) as u8;
                        s[2] = br.get(6) as u8;
                        s[1] = s[0];
                    }
                    3 => {
                        s[0] = br.get(6) as u8;
                        s[2] = br.get(6) as u8;
                        s[1] = s[2];
                    }
                    _ => {
                        s[0] = br.get(6) as u8;
                        s[1] = br.get(6) as u8;
                        s[2] = br.get(6) as u8;
                    }
                }
            }
        }

        // the samples: 3 groups of 12 per subband, each group 3 values per subband
        let mut samples = [[[0f32; 32]; 36]; 2]; // [channel][time slot][subband]
        for k in 0..3 {
            for l in (0..12).step_by(3) {
                let t0 = k * 12 + l;
                let mut j = 0usize;
                for i in 0..bound {
                    let nbal = alloc[j] as usize;
                    for ch in 0..nch {
                        let b = bit_alloc[ch][i] as usize;
                        if b == 0 {
                            continue;
                        }
                        let q = alloc[j + b] as usize;
                        let factor = self.factors[sf[ch][i][k] as usize];
                        let codes = read_codes(&mut br, q);
                        for (m, &c) in codes.iter().enumerate() {
                            samples[ch][t0 + m][i] = requant(c, QUANT_STEPS[q], factor);
                        }
                    }
                    j += 1 << nbal;
                }
                for i in bound..sblimit {
                    let nbal = alloc[j] as usize;
                    let b = bit_alloc[0][i] as usize;
                    if b != 0 {
                        let q = alloc[j + b] as usize;
                        let codes = read_codes(&mut br, q);
                        for ch in 0..nch {
                            let factor = self.factors[sf[ch][i][k] as usize];
                            for (m, &c) in codes.iter().enumerate() {
                                samples[ch][t0 + m][i] = requant(c, QUANT_STEPS[q], factor);
                            }
                        }
                    }
                    j += 1 << nbal;
                }
            }
        }

        // synthesis: 36 subband slots per channel, 32 PCM samples each
        let s = statics();
        let mut out = vec![0.0f64; 1152 * nch];
        for ch in 0..nch {
            for t in 0..36 {
                let mut pcm = [0.0f32; 32];
                synth(s, &mut self.v[ch], &mut self.v_off[ch], &samples[ch][t], &mut pcm);
                for (sb, &x) in pcm.iter().enumerate() {
                    out[(t * 32 + sb) * nch + ch] = x as f64;
                }
            }
        }
        Ok(Frame::Audio(AudioFrame { sample_rate: h.sample_rate, channels: nch as u16, pts: p.pts, samples: out }))
    }
}

/// An MPEG-1/2 Audio Layer II decoder. Each packet is one whole frame, header included.
pub struct Mp2Decoder {
    channels: u16,
    layer2: Layer2,
}

impl Mp2Decoder {
    pub fn new(params: &StreamParams) -> Result<Self> {
        let channels = match params {
            StreamParams::Audio { channels, .. } => (*channels).clamp(1, 2),
            _ => return Err(Error::new(ErrorCode::InvalidInput, "MP2 needs an audio stream")),
        };
        Ok(Self { channels, layer2: Layer2::new() })
    }
}

impl Decoder for Mp2Decoder {
    fn decode(&mut self, p: &Packet) -> Result<Vec<Frame>> {
        conceal(self.channels, p, self.layer2.decode(p))
    }
}
