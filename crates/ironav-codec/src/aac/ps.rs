//! Parametric stereo (ISO/IEC 14496-3 subpart 8), the tool that makes HE-AAC v2: a mono SBR signal
//! plus a few parameters per band (inter-channel intensity and phase differences, coherence) is turned
//! into stereo in the QMF domain, before the synthesis filterbank.
//!
//! What is decoded: the `ps_data()` bitstream with its header, envelope borders, IID (coarse and fine
//! quantisation, 10, 20 and 34 bands), ICC (10, 20 and 34 bands) and the IPD / OPD extension (11 or 17
//! bands); the hybrid analysis filterbank (the lowest QMF bands split into 6+2+2 subbands, or, with
//! 34 parameter bands, 12+8+4+4+4); the decorrelator (three fractional-delay all-pass links per band
//! with the transient-aware ducking, and the plain delays above them); the stereo mixing with the
//! H11/H12/H21/H22 matrices (mixing rotation A below ICC mode 3 and rotation B above), interpolated
//! across each envelope; and the hybrid synthesis. IPD and OPD are applied (the full profile, not just
//! the baseline one). The syntax, tables and signal-processing equations follow 3GPP SP-040428.
//! Tests compare the decoded output against FFmpeg.
//!
//! Nothing of the tool is skipped. Parameters that arrive before the first header are ignored (the
//! stereo signal stays a copy of the mono one), and a frame that carries no `ps_data` reuses the last
//! one, as FFmpeg does. One difference from FFmpeg: whether IPD/OPD are in use is read per frame (a
//! frame without the extension has none), where FFmpeg keeps the last frame's flag; the two agree on
//! every stream whose encoder writes the extension in each frame or in none.

use super::ps_tables::HUFFMAN;
use super::Bits;
use std::collections::HashMap;
use std::sync::OnceLock;

mod bands;
mod decorrelate;
mod hybrid;
mod mix;
mod syntax;

use bands::remap;
use decorrelate::Decorrelator;
use hybrid::HybridBank;
use mix::{Cues, Mixer};

const MAX_ENV: usize = 5;
const MAX_PAR: usize = 34;
/// QMF time slots per frame.
const SLOTS: usize = 32;
const BANDS: [usize; 2] = [71, 91];
type Par = [[i8; MAX_PAR]; MAX_ENV];
type Cx = [f32; 2];

/// Read a Huffman codeword in MSB-first order. The key includes its length so leading zeros
/// distinguish words with the same numeric value.
fn read_huff(bits: &mut Bits, table: usize) -> Option<i32> {
    static CODEBOOKS: OnceLock<[HashMap<u32, i32>; 10]> = OnceLock::new();
    let books = CODEBOOKS.get_or_init(|| HUFFMAN.map(|(base, words)| {
        words.iter().enumerate().map(|(i, &(len, code))| (u32::from(len) << 24 | code, base + i as i32)).collect()
    }));
    let mut prefix = 0;
    for len in 1..=20 {
        prefix = prefix << 1 | bits.read(1);
        if let Some(&delta) = books[table].get(&(len << 24 | prefix)) {
            return Some(delta);
        }
    }
    None
}

// Indices of the Huffman tables in `HUFFMAN`.
const HUFF_IID_DF1: usize = 0;
const HUFF_IID_DT1: usize = 1;
const HUFF_IID_DF0: usize = 2;
const HUFF_IID_DT0: usize = 3;
const HUFF_ICC_DF: usize = 4;
const HUFF_ICC_DT: usize = 5;
const HUFF_IPD_DF: usize = 6;
const HUFF_IPD_DT: usize = 7;
const HUFF_OPD_DF: usize = 8;
const HUFF_OPD_DT: usize = 9;

// ---- the decoder ----------------------------------------------------------------------------------

pub struct Ps {
    // bitstream state
    pub start: bool,
    enable_iid: bool,
    iid_quant: bool,
    nr_iid_par: usize,
    nr_ipdopd_par: usize,
    enable_icc: bool,
    icc_mode: usize,
    nr_icc_par: usize,
    enable_ext: bool,
    num_env_old: usize,
    num_env: usize,
    enable_ipdopd: bool,
    border: [i32; MAX_ENV + 1],
    iid_par: Par,
    icc_par: Par,
    ipd_par: Par,
    opd_par: Par,
    is34: bool,
    is34_old: bool,
    // signal state
    hybrid: HybridBank,
    decorrelator: Decorrelator,
    mixer: Mixer,
    lbuf: Vec<Cx>,
    rbuf: Vec<Cx>,
}

impl Ps {
    pub fn new() -> Self {
        Ps {
            start: false,
            enable_iid: false,
            iid_quant: false,
            nr_iid_par: 0,
            nr_ipdopd_par: 0,
            enable_icc: false,
            icc_mode: 0,
            nr_icc_par: 0,
            enable_ext: false,
            num_env_old: 0,
            num_env: 0,
            enable_ipdopd: false,
            border: [0; MAX_ENV + 1],
            iid_par: [[0; MAX_PAR]; MAX_ENV],
            icc_par: [[0; MAX_PAR]; MAX_ENV],
            ipd_par: [[0; MAX_PAR]; MAX_ENV],
            opd_par: [[0; MAX_PAR]; MAX_ENV],
            is34: false,
            is34_old: false,
            hybrid: HybridBank::default(),
            decorrelator: Decorrelator::new(),
            mixer: Mixer::new(),
            lbuf: vec![[0.0; 2]; 91 * SLOTS],
            rbuf: vec![[0.0; 2]; 91 * SLOTS],
        }
    }

    /// Turn the mono signal in `x[0]` (QMF samples, `[re | im][slot][band]` with 38 slots each) into
    /// stereo in `x[0]` and `x[1]`. `top` is the highest QMF band SBR fills (`kx + m`).
    pub fn apply(&mut self, x: &mut [Vec<[f32; 64]>], top: usize) {
        let is34 = self.is34;
        let mode = is34 as usize;
        // The subbands above SBR's top carry no history into this frame.
        let top = (top + BANDS[mode]).saturating_sub(64);
        self.hybrid.analyze(&x[0], &mut self.lbuf, is34);
        let layout_changed = is34 != self.is34_old;
        self.decorrelator.process(&self.lbuf, &mut self.rbuf, is34, layout_changed, top);

        let iid = remap(&self.iid_par, self.nr_iid_par, self.num_env, is34, true);
        let icc = remap(&self.icc_par, self.nr_icc_par, self.num_env, is34, true);
        let phase = self.enable_ipdopd.then(|| {
            let ipd = remap(&self.ipd_par, self.nr_ipdopd_par, self.num_env, is34, false);
            let opd = remap(&self.opd_par, self.nr_ipdopd_par, self.num_env, is34, false);
            (ipd, opd)
        });
        let cues = Cues {
            iid: &iid,
            icc: &icc,
            phase: phase.as_ref().map(|(ipd, opd)| (ipd, opd)),
            fine_iid: self.iid_quant,
            rotation_b: self.icc_mode >= 3,
            borders: &self.border[..=self.num_env],
            previous_envelopes: self.num_env_old,
            wide: is34,
            layout_changed,
        };
        self.mixer.apply(&mut self.lbuf, &mut self.rbuf, cues);
        let (left, right) = x.split_at_mut(1);
        HybridBank::synthesize(&self.lbuf, &mut left[0], is34);
        HybridBank::synthesize(&self.rbuf, &mut right[0], is34);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hybrid analysis followed by the synthesis (a sum of the subbands) returns the QMF signal,
    /// delayed by the six slots of the filters' symmetric halves.
    fn hybrid_round_trip(is34: bool) -> f64 {
        let mut ps = Ps::new();
        let mut state = 12345u32;
        let mut noise = move || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let frames = 6;
        let mut input = vec![vec![[0f32; 64]; 2 * 38]; frames];
        for f in 0..frames {
            // each frame's slots 32..38 are the first of the next
            for slot in 0..38 {
                for band in 0..64 {
                    let (re, im) = (noise(), noise());
                    if f > 0 && slot < 6 {
                        let prev = input[f - 1][32 + slot];
                        let prev_im = input[f - 1][38 + 32 + slot];
                        input[f][slot][band] = prev[band];
                        input[f][38 + slot][band] = prev_im[band];
                    } else {
                        input[f][slot][band] = re;
                        input[f][38 + slot][band] = im;
                    }
                }
            }
        }
        let mut worst = 0f64;
        for f in 0..frames {
            ps.hybrid.analyze(&input[f], &mut ps.lbuf, is34);
            let mut out = vec![[0f32; 64]; 76];
            HybridBank::synthesize(&ps.lbuf, &mut out, is34);
            if f == 0 {
                continue;
            }
            for n in 0..SLOTS {
                for band in 0..64 {
                    let got = out[n][band] as f64;
                    let want = input[f][n][band] as f64;
                    worst = worst.max((got - want).abs());
                }
            }
        }
        worst
    }

    #[test]
    fn huffman_tables_are_prefix_codes() {
        for (table, &(first_delta, words)) in HUFFMAN.iter().enumerate() {
            let sum: u32 = words.iter().map(|&(len, _)| 1 << (20 - len)).sum();
            assert_eq!(sum, 1 << 20, "incomplete Huffman table {table}");
            for (index, &(len, code)) in words.iter().enumerate() {
                let data = (code << (32 - u32::from(len))).to_be_bytes();
                let mut bits = Bits::new(&data);
                let delta = first_delta + index as i32;
                assert_eq!(read_huff(&mut bits, table), Some(delta), "table {table}, delta {delta}");
                assert_eq!(bits.pos, usize::from(len), "table {table}, delta {delta}");
            }
        }
    }

    /// `bits` ('0' and '1', spaces ignored) as bytes, zero-padded.
    fn bytes(bits: &str) -> Vec<u8> {
        let b: Vec<bool> = bits.chars().filter(|c| !c.is_whitespace()).map(|c| c == '1').collect();
        b.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &x)| a | (x as u8) << (7 - i))).collect()
    }

    #[test]
    fn a_small_ps_data_parses() {
        // header, IID (mode 0: 10 bands, coarse) on, ICC off, no extension; fixed class, one envelope
        // (code 1): border 31; IID frequency-coded: ten zero differences (the one-bit code 0)
        let data = bytes("1 1 000 0 0  0 01  0 0000000000");
        let mut ps = Ps::new();
        let mut br = Bits::new(&data);
        assert_eq!(ps.read(&mut br, 24), 21);
        assert_eq!(br.pos, 21);
        assert!(ps.start && !ps.is34);
        assert_eq!((ps.num_env, ps.border[1]), (1, 31));
        assert_eq!(ps.iid_par[0][..10], [0; 10]);
    }

    #[test]
    fn bad_or_cut_ps_data_switches_the_tool_off_and_skips_its_bits() {
        let good = bytes("1 1 000 0 0  0 01  0 0000000000");
        // a reserved IID mode
        let reserved = bytes("1 1 110 0 0 0 01 0 0000000000");
        // borders that go backwards (variable class, two envelopes: 20 then 3)
        let backwards = bytes("1 0 0 0  1 01 10100 00011");
        // the data runs past what the extension says is there
        for (name, data, left) in [("reserved", &reserved, 24), ("backwards", &backwards, 24), ("cut", &good, 12), ("empty", &vec![], 8)] {
            let mut ps = Ps::new();
            ps.start = true;
            let mut br = Bits::new(data);
            assert_eq!(ps.read(&mut br, left), left, "{name}");
            assert_eq!(br.pos, left as usize, "{name}: the bits are skipped");
            assert!(!ps.start, "{name}: switched off");
            assert!(ps.iid_par.iter().all(|e| e.iter().all(|&v| v == 0)), "{name}: parameters cleared");
        }
        // anything at all, of any length, is survived
        let mut x = 0x1234_5678u32;
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (x >> 24) as u8 }).collect();
            let mut ps = Ps::new();
            let mut br = Bits::new(&data);
            ps.read(&mut br, len * 8);
        }
    }

    #[test]
    fn hybrid_bank_is_a_delay_in_both_layouts() {
        for is34 in [false, true] {
            let worst = hybrid_round_trip(is34);
            assert!(worst < 0.02, "is34 {is34}: worst error {worst}");
        }
    }
}
