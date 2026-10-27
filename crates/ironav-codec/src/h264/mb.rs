//! Slice data: macroblock parsing (CAVLC and CABAC) and intra reconstruction.

use super::bits::BitReader;
use super::cabac::Cabac;
use super::cavlc;
use super::dpb::RefList;
use super::idct::{hadamard4, idct4_add, idct8_add};
use super::inter;
use super::intra::*;
use super::ps::{Pps, SliceHeader, SliceType, Sps, ZIGZAG4};
use super::tables::*;
use ironav_core::{Error, ErrorCode, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    #[default]
    None,
    I4x4,
    I8x8,
    I16x16,
    IPcm,
    /// Predicted from reference pictures (P and B macroblocks, skipped or not).
    Inter,
}

/// What later macroblocks and the loop filter need to know about a decoded macroblock.
#[derive(Clone, Copy)]
pub struct MbInfo {
    /// 1-based id of the slice that holds the macroblock; 0 until it is decoded.
    pub slice: u16,
    pub kind: Kind,
    pub qp: i8,
    /// Non-zero coefficient counts: 16 luma blocks in raster order, then 4 Cb, then 4 Cr.
    pub nnz: [u8; 24],
    /// Coded-block flags of the DC blocks: bit 0 Intra16x16 luma, bit 1 Cb, bit 2 Cr.
    pub cbf_dc: u8,
    /// Coded block pattern: luma in bits 0-3, chroma in bits 4-5.
    pub cbp: u8,
    pub chroma_mode: u8,
    pub t8x8: bool,
    /// Intra 4x4 / 8x8 prediction modes in raster order of 4x4 blocks.
    pub ipm: [u8; 16],
    /// Motion vectors per list, in raster order of 4x4 blocks, in quarter luma samples.
    pub mv: [[[i16; 2]; 16]; 2],
    /// Reference index per list and 8x8 block; -1 when the list is not used.
    pub ref_idx: [[i8; 4]; 2],
    /// Identity of the reference picture behind `ref_idx`; -1 when unused.
    pub ref_id: [[i32; 4]; 2],
    pub skip: bool,
    /// Absolute motion vector differences (clipped) per list and 4x4 block, for CABAC contexts.
    pub mvd: [[[u8; 2]; 16]; 2],
}

impl Default for MbInfo {
    fn default() -> Self {
        MbInfo {
            slice: 0,
            kind: Kind::None,
            qp: 0,
            nnz: [0; 24],
            cbf_dc: 0,
            cbp: 0,
            chroma_mode: 0,
            t8x8: false,
            ipm: [2; 16],
            mv: [[[0; 2]; 16]; 2],
            ref_idx: [[-1; 4]; 2],
            ref_id: [[-1; 4]; 2],
            skip: false,
            mvd: [[[0; 2]; 16]; 2],
        }
    }
}

pub struct Picture {
    pub mb_w: usize,
    pub mb_h: usize,
    pub planes: [Vec<u8>; 3],
    pub mbs: Vec<MbInfo>,
    /// Per slice (index = slice id - 1): loop-filter parameters and chroma QP offsets.
    pub slices: Vec<SliceParams>,
}

#[derive(Clone, Copy)]
pub struct SliceParams {
    pub disable_deblocking: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
    pub chroma_qp_offset: [i32; 2],
}

impl Picture {
    pub fn new(mb_w: usize, mb_h: usize) -> Self {
        Picture {
            mb_w,
            mb_h,
            planes: [vec![0; mb_w * mb_h * 256], vec![0; mb_w * mb_h * 64], vec![0; mb_w * mb_h * 64]],
            mbs: vec![MbInfo::default(); mb_w * mb_h],
            slices: Vec::new(),
        }
    }
}

/// Level scale tables: `l4[list][qp % 6][raster]` for the six 4x4 lists and `l8[list][qp % 6][raster]`
/// for the two 8x8 luma lists.
pub struct LevelScale {
    pub l4: Vec<[[i32; 16]; 6]>,
    pub l8: Vec<[[i32; 64]; 6]>,
}

impl LevelScale {
    pub fn new(pps: &Pps) -> Self {
        let mut l4 = Vec::new();
        for list in 0..6 {
            let mut w = [0i32; 16];
            for (k, &p) in ZIGZAG4.iter().enumerate() {
                w[p as usize] = pps.lists4[list][k] as i32;
            }
            let mut per_m = [[0i32; 16]; 6];
            for m in 0..6 {
                for i in 0..4 {
                    for j in 0..4 {
                        let n = DEQUANT4_COEFF_INIT[m][(i & 1) + (j & 1)] as i32;
                        per_m[m][i * 4 + j] = w[i * 4 + j] * n;
                    }
                }
            }
            l4.push(per_m);
        }
        let mut l8 = Vec::new();
        for list in 0..2 {
            let mut w = [0i32; 64];
            for (k, &p) in ZIGZAG_DIRECT.iter().enumerate() {
                w[p as usize] = pps.lists8[list][k] as i32;
            }
            let mut per_m = [[0i32; 64]; 6];
            for m in 0..6 {
                for i in 0..8 {
                    for j in 0..8 {
                        let cls = if i % 4 == 0 && j % 4 == 0 {
                            0
                        } else if i % 2 == 1 && j % 2 == 1 {
                            1
                        } else if i % 4 == 2 && j % 4 == 2 {
                            2
                        } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
                            3
                        } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
                            4
                        } else {
                            5
                        };
                        per_m[m][i * 8 + j] = w[i * 8 + j] * DEQUANT8_COEFF_INIT[m][cls] as i32;
                    }
                }
            }
            l8.push(per_m);
        }
        LevelScale { l4, l8 }
    }
}

const CHROMA_QP: [u8; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];

pub fn chroma_qp(qp: i32, offset: i32) -> i32 {
    let qpi = (qp + offset).clamp(0, 51);
    if qpi < 30 {
        qpi
    } else {
        CHROMA_QP[(qpi - 30) as usize] as i32
    }
}

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

/// Coefficients of one macroblock, as coded levels at raster positions within each block.
struct Res {
    /// 16 blocks of 4x4 (raster block order), or 4 blocks of 8x8, in the same 256 entries.
    luma: [i32; 256],
    dc: [i32; 16],
    cdc: [[i32; 4]; 2],
    /// Chroma AC: [component * 4 + block][raster position within the 4x4 block].
    cac: [[i32; 16]; 8],
}

impl Res {
    fn new() -> Self {
        Res { luma: [0; 256], dc: [0; 16], cdc: [[0; 4]; 2], cac: [[0; 16]; 8] }
    }
}

enum Reader<'a> {
    Cavlc(BitReader<'a>),
    Cabac(Cabac<'a>),
}

pub struct SliceDec<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub hdr: &'a SliceHeader,
    pub pic: &'a mut Picture,
    pub scale: &'a LevelScale,
    pub slice_id: u16,
    qp: i32,
    last_dqp: bool,
    mbx: usize,
    mby: usize,
    cur: MbInfo,
    rd: Reader<'a>,
    refs: &'a [RefList; 2],
    /// Bit per 4x4 block (raster) of the current macroblock whose motion is already known.
    done: u16,
}

/// z-order index of the 4x4 block at (bx, by) within a macroblock.
fn zorder(bx: usize, by: usize) -> usize {
    ((by >> 1) << 3) | ((bx >> 1) << 2) | ((by & 1) << 1) | (bx & 1)
}

/// Raster (bx, by) of the n-th 4x4 block in decoding order.
fn block_pos(n: usize) -> (usize, usize) {
    (((n >> 2) & 1) * 2 + (n & 1), ((n >> 3) & 1) * 2 + ((n >> 1) & 1))
}

impl<'a> SliceDec<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(sps: &'a Sps, pps: &'a Pps, hdr: &'a SliceHeader, pic: &'a mut Picture, scale: &'a LevelScale, slice_id: u16, rbsp: &'a [u8], refs: &'a [RefList; 2]) -> Result<Self> {
        let rd = if pps.cabac {
            // cabac_alignment_one_bit up to the next byte
            let start = hdr.data_pos.div_ceil(8);
            Reader::Cabac(Cabac::new(rbsp, start, hdr.slice_type == SliceType::I, hdr.cabac_init_idc, hdr.qp))
        } else {
            let mut r = BitReader::new(rbsp);
            r.skip(hdr.data_pos);
            Reader::Cavlc(r)
        };
        Ok(SliceDec { sps, pps, hdr, pic, scale, slice_id, qp: hdr.qp, last_dqp: false, mbx: 0, mby: 0, cur: MbInfo::default(), rd, refs, done: 0 })
    }

    fn start_mb(&mut self, addr: usize) -> Result<()> {
        if addr >= self.pic.mb_w * self.pic.mb_h {
            return Err(Error::invalid_input("H.264 slice runs past the end of the picture"));
        }
        self.mbx = addr % self.pic.mb_w;
        self.mby = addr / self.pic.mb_w;
        self.cur = MbInfo { slice: self.slice_id, ..MbInfo::default() };
        self.done = 0;
        Ok(())
    }

    fn finish_mb(&mut self, addr: usize) {
        self.cur.qp = if self.cur.kind == Kind::IPcm { 0 } else { self.qp as i8 };
        self.pic.mbs[addr] = self.cur;
    }

    pub fn decode(&mut self) -> Result<()> {
        let inter_slice = self.hdr.slice_type != SliceType::I;
        if inter_slice {
            if self.hdr.slice_type == SliceType::B {
                return Err(unsupported("H.264 B slices are not supported yet"));
            }
            if self.pps.constrained_intra_pred {
                return Err(unsupported("H.264 constrained intra prediction in P slices is not supported yet"));
            }
        }
        let mut addr = self.hdr.first_mb;
        loop {
            let mut more = true;
            if inter_slice {
                let run = match &mut self.rd {
                    Reader::Cavlc(r) => r.ue()?,
                    Reader::Cabac(_) => 0,
                };
                if matches!(self.rd, Reader::Cabac(_)) {
                    self.start_mb(addr)?;
                    if self.read_skip_flag() {
                        self.decode_skip()?;
                        self.finish_mb(addr);
                        addr += 1;
                        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
                        if c.terminate() == 1 {
                            break;
                        }
                        continue;
                    }
                    self.decode_mb()?;
                    self.finish_mb(addr);
                    addr += 1;
                    let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
                    if c.terminate() == 1 {
                        break;
                    }
                    continue;
                }
                for _ in 0..run {
                    self.start_mb(addr)?;
                    self.decode_skip()?;
                    self.finish_mb(addr);
                    addr += 1;
                }
                if run > 0 {
                    if let Reader::Cavlc(r) = &self.rd {
                        more = r.more_rbsp_data();
                    }
                }
            }
            if more {
                self.start_mb(addr)?;
                self.decode_mb()?;
                self.finish_mb(addr);
                addr += 1;
                more = match &mut self.rd {
                    Reader::Cavlc(r) => r.more_rbsp_data(),
                    Reader::Cabac(c) => c.terminate() == 0,
                };
            }
            if !more {
                break;
            }
        }
        Ok(())
    }

    fn idx(&self) -> usize {
        self.mby * self.pic.mb_w + self.mbx
    }

    fn avail_left(&self) -> bool {
        self.mbx > 0 && self.pic.mbs[self.idx() - 1].slice == self.slice_id
    }
    fn avail_top(&self) -> bool {
        self.mby > 0 && self.pic.mbs[self.idx() - self.pic.mb_w].slice == self.slice_id
    }
    fn avail_topleft(&self) -> bool {
        self.mbx > 0 && self.mby > 0 && self.pic.mbs[self.idx() - self.pic.mb_w - 1].slice == self.slice_id
    }
    fn avail_topright(&self) -> bool {
        self.mby > 0 && self.mbx + 1 < self.pic.mb_w && self.pic.mbs[self.idx() - self.pic.mb_w + 1].slice == self.slice_id
    }
    fn left(&self) -> &MbInfo {
        &self.pic.mbs[self.idx() - 1]
    }
    fn top(&self) -> &MbInfo {
        &self.pic.mbs[self.idx() - self.pic.mb_w]
    }

    // ---- macroblock layer ----------------------------------------------------------------------

    fn decode_mb(&mut self) -> Result<()> {
        let mut mb_type = self.read_mb_type()?;
        if self.hdr.slice_type == SliceType::P {
            if mb_type < 5 {
                return self.decode_inter_mb(mb_type);
            }
            mb_type -= 5;
        }
        if mb_type == 25 {
            return self.decode_pcm();
        }
        let mut res = Res::new();
        let mut i16_mode = 0u8;
        let cbp;
        let mut ipm_syntax = [(false, 0u8); 16];
        let nxn = mb_type == 0;
        if nxn {
            if self.pps.transform_8x8 {
                self.cur.t8x8 = self.read_t8x8_flag()?;
            }
            self.cur.kind = if self.cur.t8x8 { Kind::I8x8 } else { Kind::I4x4 };
            let n = if self.cur.t8x8 { 4 } else { 16 };
            for (k, e) in ipm_syntax.iter_mut().enumerate().take(n) {
                let _ = k;
                *e = self.read_pred_mode_syntax()?;
            }
            self.cur.chroma_mode = self.read_chroma_pred_mode()?;
            cbp = self.read_cbp()?;
        } else if mb_type <= 24 {
            self.cur.kind = Kind::I16x16;
            let t = mb_type - 1;
            i16_mode = (t % 4) as u8;
            let chroma = (t / 4) % 3;
            cbp = (chroma << 4) | if t >= 12 { 15 } else { 0 };
            self.cur.chroma_mode = self.read_chroma_pred_mode()?;
        } else {
            return Err(Error::invalid_input("H.264 macroblock type is out of range for an I slice"));
        }
        self.cur.cbp = cbp as u8;
        if cbp > 0 || self.cur.kind == Kind::I16x16 {
            let dqp = self.read_qp_delta()?;
            self.qp = (self.qp + dqp + 52).rem_euclid(52);
            self.last_dqp = dqp != 0;
            self.read_residual(&mut res, cbp, self.cur.kind == Kind::I16x16)?;
        } else {
            self.last_dqp = false;
        }
        if nxn {
            self.assign_pred_modes(&ipm_syntax)?;
        }
        self.reconstruct(&res, i16_mode, cbp)?;
        Ok(())
    }

    fn read_mb_type(&mut self) -> Result<u32> {
        let (a, b) = {
            let la = self.avail_left();
            let ta = self.avail_top();
            (la && self.left().kind != Kind::I4x4 && self.left().kind != Kind::I8x8, ta && self.top().kind != Kind::I4x4 && self.top().kind != Kind::I8x8)
        };
        let p_slice = self.hdr.slice_type == SliceType::P;
        match &mut self.rd {
            Reader::Cavlc(r) => r.ue(),
            Reader::Cabac(c) if p_slice => {
                if c.decision(14) == 0 {
                    return Ok(if c.decision(15) == 0 {
                        if c.decision(16) == 0 {
                            0
                        } else {
                            3
                        }
                    } else if c.decision(17) == 0 {
                        2
                    } else {
                        1
                    });
                }
                // intra macroblock type with the prefix/suffix contexts
                if c.decision(17) == 0 {
                    return Ok(5);
                }
                if c.terminate() == 1 {
                    return Ok(5 + 25);
                }
                let mut t = 1;
                t += 12 * c.decision(17 + 1);
                if c.decision(17 + 2) == 1 {
                    t += 4 + 4 * c.decision(17 + 2);
                }
                t += 2 * c.decision(17 + 3);
                t += c.decision(17 + 3);
                Ok(5 + t)
            }
            Reader::Cabac(c) => {
                let inc = a as usize + b as usize;
                if c.decision(3 + inc) == 0 {
                    return Ok(0);
                }
                if c.terminate() == 1 {
                    return Ok(25);
                }
                let mut t = 1;
                t += 12 * c.decision(3 + 3);
                if c.decision(3 + 4) == 1 {
                    t += 4 + 4 * c.decision(3 + 5);
                }
                t += 2 * c.decision(3 + 6);
                t += c.decision(3 + 7);
                Ok(t)
            }
        }
    }

    fn read_t8x8_flag(&mut self) -> Result<bool> {
        let (a, b) = (self.avail_left() && self.left().t8x8, self.avail_top() && self.top().t8x8);
        match &mut self.rd {
            Reader::Cavlc(r) => r.flag(),
            Reader::Cabac(c) => Ok(c.decision(399 + a as usize + b as usize) == 1),
        }
    }

    /// prev_intra_pred_mode_flag and rem_intra_pred_mode for one block.
    fn read_pred_mode_syntax(&mut self) -> Result<(bool, u8)> {
        match &mut self.rd {
            Reader::Cavlc(r) => {
                if r.flag()? {
                    Ok((true, 0))
                } else {
                    Ok((false, r.bits(3)? as u8))
                }
            }
            Reader::Cabac(c) => {
                if c.decision(68) == 1 {
                    Ok((true, 0))
                } else {
                    let mut m = c.decision(69);
                    m |= c.decision(69) << 1;
                    m |= c.decision(69) << 2;
                    Ok((false, m as u8))
                }
            }
        }
    }

    fn read_chroma_pred_mode(&mut self) -> Result<u8> {
        let cond = |m: &MbInfo| m.kind != Kind::IPcm && m.chroma_mode != 0;
        let (a, b) = (self.avail_left() && cond(self.left()), self.avail_top() && cond(self.top()));
        let mode = match &mut self.rd {
            Reader::Cavlc(r) => r.ue()?,
            Reader::Cabac(c) => {
                if c.decision(64 + a as usize + b as usize) == 0 {
                    0
                } else if c.decision(64 + 3) == 0 {
                    1
                } else if c.decision(64 + 3) == 0 {
                    2
                } else {
                    3
                }
            }
        };
        if mode > 3 {
            return Err(Error::invalid_input("H.264 intra chroma prediction mode is out of range"));
        }
        Ok(mode as u8)
    }

    fn read_cbp(&mut self) -> Result<u32> {
        match &mut self.rd {
            Reader::Cavlc(r) => {
                let k = r.ue()?;
                if k > 47 {
                    return Err(Error::invalid_input("H.264 coded_block_pattern code is out of range"));
                }
                Ok(GOLOMB_TO_INTRA4X4_CBP[k as usize] as u32)
            }
            Reader::Cabac(_) => {
                let (la, ta) = (self.avail_left(), self.avail_top());
                let (lcbp, tcbp) = (if la { self.left().cbp } else { 0 }, if ta { self.top().cbp } else { 0 });
                let (lpcm, tpcm) = (la && self.left().kind == Kind::IPcm, ta && self.top().kind == Kind::IPcm);
                let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
                let mut luma = 0u32;
                for b8 in 0..4usize {
                    // condTermFlag is 1 only for an available, non-PCM neighbour whose bit is clear
                    let cond_a = if b8 & 1 == 1 {
                        (luma >> (b8 - 1)) & 1 == 0
                    } else {
                        la && !lpcm && (lcbp >> (b8 + 1)) & 1 == 0
                    };
                    let cond_b = if b8 >= 2 {
                        (luma >> (b8 - 2)) & 1 == 0
                    } else {
                        ta && !tpcm && (tcbp >> (b8 + 2)) & 1 == 0
                    };
                    let bit = c.decision(73 + cond_a as usize + 2 * cond_b as usize);
                    luma |= bit << b8;
                }
                let lc = (lcbp >> 4) as u32;
                let tc = (tcbp >> 4) as u32;
                let ca = la && (lpcm || lc != 0);
                let cb = ta && (tpcm || tc != 0);
                let mut chroma = 0;
                if c.decision(77 + ca as usize + 2 * cb as usize) == 1 {
                    let ca2 = la && (lpcm || lc == 2);
                    let cb2 = ta && (tpcm || tc == 2);
                    chroma = 1 + c.decision(77 + 4 + ca2 as usize + 2 * cb2 as usize);
                }
                Ok(luma | (chroma << 4))
            }
        }
    }

    fn read_qp_delta(&mut self) -> Result<i32> {
        let prev = self.last_dqp;
        match &mut self.rd {
            Reader::Cavlc(r) => {
                let v = r.se()?;
                if !(-26..=25).contains(&v) {
                    return Err(Error::invalid_input("H.264 mb_qp_delta is out of range"));
                }
                Ok(v)
            }
            Reader::Cabac(c) => {
                let mut k = 0;
                let mut ctx = 60 + prev as usize;
                while c.decision(ctx) == 1 {
                    k += 1;
                    ctx = if k == 1 { 62 } else { 63 };
                    if k > 104 {
                        return Err(Error::invalid_input("H.264 mb_qp_delta is too large"));
                    }
                }
                Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
            }
        }
    }

    fn decode_pcm(&mut self) -> Result<()> {
        self.cur.kind = Kind::IPcm;
        self.cur.nnz = [16; 24];
        self.cur.cbf_dc = 7;
        self.cur.cbp = 0x2F;
        self.last_dqp = false;
        let mut samples = [0u8; 384];
        match &mut self.rd {
            Reader::Cavlc(r) => {
                r.align();
                for s in samples.iter_mut() {
                    *s = r.bits(8)? as u8;
                }
            }
            Reader::Cabac(c) => {
                let start = c.pcm_byte_pos();
                let d = c.data();
                let src = d.get(start..start + 384).ok_or_else(|| Error::invalid_input("H.264 I_PCM samples are cut short"))?;
                samples.copy_from_slice(src);
                c.restart_at(start + 384);
            }
        }
        let (mbx, mby) = (self.mbx, self.mby);
        let ys = self.pic.mb_w * 16;
        for y in 0..16 {
            let o = (mby * 16 + y) * ys + mbx * 16;
            self.pic.planes[0][o..o + 16].copy_from_slice(&samples[y * 16..y * 16 + 16]);
        }
        let cs = self.pic.mb_w * 8;
        for c in 0..2 {
            for y in 0..8 {
                let o = (mby * 8 + y) * cs + mbx * 8;
                let s = 256 + c * 64 + y * 8;
                self.pic.planes[1 + c][o..o + 8].copy_from_slice(&samples[s..s + 8]);
            }
        }
        Ok(())
    }

    // ---- inter macroblocks ---------------------------------------------------------------------------

    /// Reference index and motion vector of the 4x4 block at (x, y) relative to the current
    /// macroblock (in 4x4 units, possibly outside it). None when the block is not available.
    fn nb_motion(&self, l: usize, x: i32, y: i32) -> Option<(i8, [i16; 2])> {
        let w = self.pic.mb_w;
        let (mbx, mby) = (self.mbx as i32, self.mby as i32);
        let (dx, dy, bx, by) = match (x < 0, x >= 4, y < 0) {
            (true, _, true) => (-1, -1, 3, 3),
            (true, _, false) => (-1, 0, 3, y),
            (_, true, true) => (1, -1, 0, 3),
            (_, true, false) => return None,
            (_, _, true) => (0, -1, x, 3),
            _ => {
                if self.done & (1 << (y * 4 + x)) == 0 {
                    return None;
                }
                let b8 = ((y >> 1) * 2 + (x >> 1)) as usize;
                return Some((self.cur.ref_idx[l][b8], self.cur.mv[l][(y * 4 + x) as usize]));
            }
        };
        let (nx, ny) = (mbx + dx, mby + dy);
        if nx < 0 || ny < 0 || nx >= w as i32 || ny >= self.pic.mb_h as i32 {
            return None;
        }
        let m = &self.pic.mbs[ny as usize * w + nx as usize];
        if m.slice != self.slice_id {
            return None;
        }
        let b8 = ((by >> 1) * 2 + (bx >> 1)) as usize;
        if m.kind != Kind::Inter {
            return Some((-1, [0, 0]));
        }
        Some((m.ref_idx[l][b8], m.mv[l][(by * 4 + bx) as usize]))
    }

    /// Motion vector prediction for a partition (8.4.1.3). `dir`: 0 median, 1 = 16x8 partition,
    /// 2 = 8x16 partition.
    fn pred_mv(&self, l: usize, bx: i32, by: i32, w: i32, dir: u8, refi: i8) -> [i16; 2] {
        let a = self.nb_motion(l, bx - 1, by);
        let b = self.nb_motion(l, bx, by - 1);
        let c = self.nb_motion(l, bx + w, by - 1).or_else(|| self.nb_motion(l, bx - 1, by - 1));
        let r = |n: Option<(i8, [i16; 2])>| n.map(|v| v.0).unwrap_or(-1);
        let mv = |n: Option<(i8, [i16; 2])>| n.map(|v| v.1).unwrap_or([0, 0]);
        match dir {
            1 => {
                if by == 0 && r(b) == refi {
                    return mv(b);
                }
                if by != 0 && r(a) == refi {
                    return mv(a);
                }
            }
            2 => {
                if bx == 0 && r(a) == refi {
                    return mv(a);
                }
                if bx != 0 && r(c) == refi {
                    return mv(c);
                }
            }
            _ => {}
        }
        if b.is_none() && c.is_none() && a.is_some() {
            return mv(a);
        }
        let (ra, rb, rc) = (r(a) == refi, r(b) == refi, r(c) == refi);
        match (ra as u8 + rb as u8 + rc as u8, ra, rb) {
            (1, true, _) => mv(a),
            (1, _, true) => mv(b),
            (1, _, _) => mv(c),
            _ => {
                let (ma, mb, mc) = (mv(a), mv(b), mv(c));
                let med = |p: i16, q: i16, s: i16| p.max(q).min(p.min(q).max(s));
                [med(ma[0], mb[0], mc[0]), med(ma[1], mb[1], mc[1])]
            }
        }
    }

    fn set_motion(&mut self, l: usize, bx: usize, by: usize, w: usize, h: usize, mv: [i16; 2]) {
        for y in by..by + h {
            for x in bx..bx + w {
                self.cur.mv[l][y * 4 + x] = mv;
                self.done |= 1 << (y * 4 + x);
            }
        }
    }

    fn set_ref(&mut self, l: usize, bx: usize, by: usize, w: usize, h: usize, r: i8) {
        for y in (by..by + h).step_by(2) {
            for x in (bx..bx + w).step_by(2) {
                self.cur.ref_idx[l][(y >> 1) * 2 + (x >> 1)] = r;
            }
        }
    }

    /// Resolves each 8x8 block's reference index to the identity of its picture.
    fn resolve_refs(&mut self) -> Result<()> {
        for l in 0..2 {
            for b8 in 0..4 {
                let r = self.cur.ref_idx[l][b8];
                if r < 0 {
                    continue;
                }
                match self.refs[l].get(r as usize) {
                    Some(Some(e)) => self.cur.ref_id[l][b8] = e.pic.id,
                    _ => return Err(Error::invalid_input("H.264 macroblock refers to a reference picture that is not available")),
                }
            }
        }
        Ok(())
    }

    fn read_skip_flag(&mut self) -> bool {
        let a = self.avail_left() && !self.left().skip;
        let b = self.avail_top() && !self.top().skip;
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        c.decision(11 + a as usize + b as usize) == 1
    }

    /// Whether the 8x8 block at (bx, by) in 4x4 units next to the current partition has a
    /// reference index above zero (CABAC ref_idx context).
    fn nb_ref_gt0(&self, x: i32, y: i32) -> bool {
        let w = self.pic.mb_w;
        let (m, bx, by) = if x < 0 {
            if !self.avail_left() {
                return false;
            }
            (self.left(), 3, y)
        } else if y < 0 {
            if !self.avail_top() {
                return false;
            }
            (self.top(), x, 3)
        } else {
            let _ = w;
            (&self.cur, x, y)
        };
        m.kind == Kind::Inter && !m.skip && m.ref_idx[0][((by >> 1) * 2 + (bx >> 1)) as usize] > 0
    }

    /// Sum of the absolute mvd values of the neighbours (CABAC mvd context).
    fn nb_mvd(&self, x: i32, y: i32, comp: usize) -> u32 {
        let (m, bx, by) = if x < 0 {
            if !self.avail_left() {
                return 0;
            }
            (self.left(), 3, y)
        } else if y < 0 {
            if !self.avail_top() {
                return 0;
            }
            (self.top(), x, 3)
        } else {
            (&self.cur, x, y)
        };
        m.mvd[0][(by * 4 + bx) as usize][comp] as u32
    }

    fn cabac_ref_idx(&mut self, bx: usize, by: usize, max: u32) -> Result<i8> {
        if max == 0 {
            return Ok(0);
        }
        let a = self.nb_ref_gt0(bx as i32 - 1, by as i32);
        let b = self.nb_ref_gt0(bx as i32, by as i32 - 1);
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        let mut v = 0u32;
        let mut ctx = 54 + a as usize + 2 * b as usize;
        while c.decision(ctx) == 1 {
            v += 1;
            ctx = if v == 1 { 54 + 4 } else { 54 + 5 };
            if v > max {
                return Err(Error::invalid_input("H.264 reference index is out of range"));
            }
        }
        if v > max {
            return Err(Error::invalid_input("H.264 reference index is out of range"));
        }
        Ok(v as i8)
    }

    fn cabac_mvd_comp(&mut self, bx: usize, by: usize, comp: usize) -> Result<i32> {
        let sum = self.nb_mvd(bx as i32 - 1, by as i32, comp) + self.nb_mvd(bx as i32, by as i32 - 1, comp);
        let base = if comp == 0 { 40 } else { 47 };
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        let inc = if sum < 3 {
            0
        } else if sum > 32 {
            2
        } else {
            1
        };
        if c.decision(base + inc) == 0 {
            return Ok(0);
        }
        let mut v = 1i32;
        let mut ctx = base + 3;
        while v < 9 && c.decision(ctx) == 1 {
            v += 1;
            if ctx < base + 6 {
                ctx += 1;
            }
        }
        if v >= 9 {
            let mut k = 3;
            while c.bypass() == 1 {
                v += 1 << k;
                k += 1;
                if k > 24 {
                    return Err(Error::invalid_input("H.264 CABAC mvd is too long"));
                }
            }
            while k > 0 {
                k -= 1;
                v += (c.bypass() as i32) << k;
            }
        }
        Ok(if c.bypass() == 1 { -v } else { v })
    }

    fn decode_skip(&mut self) -> Result<()> {
        self.cur.kind = Kind::Inter;
        self.cur.skip = true;
        self.last_dqp = false;
        // P_Skip motion (8.4.1.1)
        let a = self.nb_motion(0, -1, 0);
        let b = self.nb_motion(0, 0, -1);
        let still = |n: Option<(i8, [i16; 2])>| n.is_some_and(|v| v.0 == 0 && v.1 == [0, 0]);
        let mv = if a.is_none() || b.is_none() || still(a) || still(b) { [0, 0] } else { self.pred_mv(0, 0, 0, 4, 0, 0) };
        self.set_ref(0, 0, 0, 4, 4, 0);
        self.set_motion(0, 0, 0, 4, 4, mv);
        self.resolve_refs()?;
        self.mc_mb()
    }

    fn read_ref_idx(&mut self, bx: usize, by: usize, max: u32) -> Result<i8> {
        if max == 0 {
            return Ok(0);
        }
        if matches!(self.rd, Reader::Cabac(_)) {
            return self.cabac_ref_idx(bx, by, max);
        }
        let Reader::Cavlc(r) = &mut self.rd else { unreachable!() };
        let v = r.te(max)?;
        if v > max {
            return Err(Error::invalid_input("H.264 reference index is out of range"));
        }
        Ok(v as i8)
    }

    fn read_mvd(&mut self, bx: usize, by: usize) -> Result<[i32; 2]> {
        if let Reader::Cavlc(r) = &mut self.rd {
            return Ok([r.se()?, r.se()?]);
        }
        Ok([self.cabac_mvd_comp(bx, by, 0)?, self.cabac_mvd_comp(bx, by, 1)?])
    }

    fn read_partition_mv(&mut self, bx: usize, by: usize, w: usize, h: usize, dir: u8) -> Result<()> {
        let refi = self.cur.ref_idx[0][(by >> 1) * 2 + (bx >> 1)];
        let p = self.pred_mv(0, bx as i32, by as i32, w as i32, dir, refi);
        let d = self.read_mvd(bx, by)?;
        let clip = |v: i32| v.unsigned_abs().min(64) as u8;
        for y in by..by + h {
            for x in bx..bx + w {
                self.cur.mvd[0][y * 4 + x] = [clip(d[0]), clip(d[1])];
            }
        }
        let mv = [(p[0] as i32 + d[0]) as i16, (p[1] as i32 + d[1]) as i16];
        self.set_motion(0, bx, by, w, h, mv);
        Ok(())
    }

    fn decode_inter_mb(&mut self, t: u32) -> Result<()> {
        self.cur.kind = Kind::Inter;
        let max_ref = self.hdr.num_ref_idx_active[0].saturating_sub(1);
        let mut all_8x8 = true;
        match t {
            0 => {
                let r = self.read_ref_idx(0, 0, max_ref)?;
                self.set_ref(0, 0, 0, 4, 4, r);
                self.read_partition_mv(0, 0, 4, 4, 0)?;
            }
            1 => {
                let r0 = self.read_ref_idx(0, 0, max_ref)?;
                self.set_ref(0, 0, 0, 4, 2, r0);
                let r1 = self.read_ref_idx(0, 2, max_ref)?;
                self.set_ref(0, 0, 2, 4, 2, r1);
                self.read_partition_mv(0, 0, 4, 2, 1)?;
                self.read_partition_mv(0, 2, 4, 2, 1)?;
            }
            2 => {
                let r0 = self.read_ref_idx(0, 0, max_ref)?;
                self.set_ref(0, 0, 0, 2, 4, r0);
                let r1 = self.read_ref_idx(2, 0, max_ref)?;
                self.set_ref(0, 2, 0, 2, 4, r1);
                self.read_partition_mv(0, 0, 2, 4, 2)?;
                self.read_partition_mv(2, 0, 2, 4, 2)?;
            }
            _ => {
                let mut sub = [0u32; 4];
                for s in sub.iter_mut() {
                    *s = match &mut self.rd {
                        Reader::Cavlc(r) => r.ue()?,
                        Reader::Cabac(c) => {
                            if c.decision(21) == 1 {
                                0
                            } else if c.decision(22) == 0 {
                                1
                            } else if c.decision(23) == 1 {
                                2
                            } else {
                                3
                            }
                        }
                    };
                    if *s > 3 {
                        return Err(Error::invalid_input("H.264 sub-macroblock type is out of range"));
                    }
                    if *s != 0 {
                        all_8x8 = false;
                    }
                }
                for i in 0..4usize {
                    let r = if t == 4 { 0 } else { self.read_ref_idx((i & 1) * 2, (i >> 1) * 2, max_ref)? };
                    self.set_ref(0, (i & 1) * 2, (i >> 1) * 2, 2, 2, r);
                }
                for (i, &s) in sub.iter().enumerate() {
                    let (x0, y0) = ((i & 1) * 2, (i >> 1) * 2);
                    match s {
                        0 => self.read_partition_mv(x0, y0, 2, 2, 0)?,
                        1 => {
                            self.read_partition_mv(x0, y0, 2, 1, 0)?;
                            self.read_partition_mv(x0, y0 + 1, 2, 1, 0)?;
                        }
                        2 => {
                            self.read_partition_mv(x0, y0, 1, 2, 0)?;
                            self.read_partition_mv(x0 + 1, y0, 1, 2, 0)?;
                        }
                        _ => {
                            for j in 0..4 {
                                self.read_partition_mv(x0 + (j & 1), y0 + (j >> 1), 1, 1, 0)?;
                            }
                        }
                    }
                }
            }
        }
        self.resolve_refs()?;
        let cbp = if let Reader::Cavlc(r) = &mut self.rd {
            let k = r.ue()?;
            if k > 47 {
                return Err(Error::invalid_input("H.264 coded_block_pattern code is out of range"));
            }
            GOLOMB_TO_INTER_CBP[k as usize] as u32
        } else {
            self.read_cbp()?
        };
        self.cur.cbp = cbp as u8;
        if cbp & 15 != 0 && self.pps.transform_8x8 && all_8x8 {
            self.cur.t8x8 = self.read_t8x8_flag()?;
        }
        let mut res = Res::new();
        if cbp > 0 {
            let dqp = self.read_qp_delta()?;
            self.qp = (self.qp + dqp + 52).rem_euclid(52);
            self.last_dqp = dqp != 0;
            self.read_residual(&mut res, cbp, false)?;
        } else {
            self.last_dqp = false;
        }
        self.mc_mb()?;
        self.add_inter_residual(&res)?;
        Ok(())
    }

    /// Motion-compensated prediction of the whole macroblock, written into the picture.
    fn mc_mb(&mut self) -> Result<()> {
        let uniform = |this: &Self, bx: usize, by: usize, n: usize| {
            let first = this.cur.mv[0][by * 4 + bx];
            (0..n).all(|dy| (0..n).all(|dx| this.cur.mv[0][(by + dy) * 4 + bx + dx] == first))
        };
        let same_ref = self.cur.ref_idx[0].iter().all(|&r| r == self.cur.ref_idx[0][0]);
        if same_ref && uniform(self, 0, 0, 4) {
            return self.mc_block(0, 0, 4, 4);
        }
        for i in 0..4usize {
            let (bx, by) = ((i & 1) * 2, (i >> 1) * 2);
            if uniform(self, bx, by, 2) {
                self.mc_block(bx, by, 2, 2)?;
            } else {
                for j in 0..4usize {
                    self.mc_block(bx + (j & 1), by + (j >> 1), 1, 1)?;
                }
            }
        }
        Ok(())
    }

    /// Predicts a block of `w` x `h` 4x4 units at (bx, by) from list 0.
    fn mc_block(&mut self, bx: usize, by: usize, w: usize, h: usize) -> Result<()> {
        let b8 = (by >> 1) * 2 + (bx >> 1);
        let r = self.cur.ref_idx[0][b8];
        let entry = match self.refs[0].get(r.max(0) as usize) {
            Some(Some(e)) if r >= 0 => e.pic.clone(),
            _ => return Err(Error::invalid_input("H.264 macroblock refers to a reference picture that is not available")),
        };
        let mv = self.cur.mv[0][by * 4 + bx];
        let (pw, ph) = (self.pic.mb_w * 16, self.pic.mb_h * 16);
        let (x, y) = ((self.mbx * 16 + bx * 4) as i32, (self.mby * 16 + by * 4) as i32);
        let (bw, bh) = (w * 4, h * 4);
        let wt = self.hdr.weights.as_ref().map(|wt| (wt, wt.entries[0].get(r as usize).copied()));
        let mut out = [0u8; 256];
        inter::luma_mc(&entry.planes[0], pw, ph, x, y, mv[0] as i32, mv[1] as i32, bw, bh, &mut out);
        if let Some((wt, Some(e))) = wt {
            inter::weight_uni(&mut out[..bw * bh], wt.luma_log2_denom, e[0], e[1]);
        }
        for row in 0..bh {
            let o = (y as usize + row) * pw + x as usize;
            self.pic.planes[0][o..o + bw].copy_from_slice(&out[row * bw..row * bw + bw]);
        }
        let (cw, ch) = (bw / 2, bh / 2);
        let (cpw, cph) = (pw / 2, ph / 2);
        for c in 0..2usize {
            let mut co = [0u8; 64];
            inter::chroma_mc(&entry.planes[1 + c], cpw, cph, x / 2, y / 2, mv[0] as i32, mv[1] as i32, cw, ch, &mut co);
            if let Some((wt, Some(e))) = wt {
                inter::weight_uni(&mut co[..cw * ch], wt.chroma_log2_denom, e[2 + 2 * c], e[3 + 2 * c]);
            }
            for row in 0..ch {
                let o = (y as usize / 2 + row) * cpw + x as usize / 2;
                self.pic.planes[1 + c][o..o + cw].copy_from_slice(&co[row * cw..row * cw + cw]);
            }
        }
        Ok(())
    }

    /// Adds the decoded residual of an inter macroblock to its prediction.
    fn add_inter_residual(&mut self, res: &Res) -> Result<()> {
        let qp = self.qp;
        let ls = self.luma_stride();
        let (x0, y0) = (self.mbx * 16, self.mby * 16);
        let cbp = self.cur.cbp as u32;
        if self.cur.t8x8 {
            for i8 in 0..4usize {
                if cbp & (1 << i8) == 0 {
                    continue;
                }
                let mut c = [0i32; 64];
                let lsv = &self.scale.l8[1][(qp % 6) as usize];
                for k in 0..64 {
                    let v = res.luma[i8 * 64 + k];
                    if v != 0 {
                        c[k] = if qp >= 36 { v.wrapping_mul(lsv[k]) << (qp / 6 - 6) } else { (v.wrapping_mul(lsv[k]) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                    }
                }
                if c.iter().any(|&v| v != 0) {
                    let (x, y) = (x0 + (i8 & 1) * 8, y0 + (i8 >> 1) * 8);
                    idct8_add(&c, &mut self.pic.planes[0][y * ls + x..], ls);
                }
            }
        } else {
            for b in 0..16usize {
                if cbp & (1 << ((b / 8) * 2 + (b % 4) / 2)) == 0 {
                    continue;
                }
                let c = self.scale4(&res.luma[b * 16..b * 16 + 16], 3, qp, false);
                if c.iter().any(|&v| v != 0) {
                    let (bx, by) = (b % 4, b / 4);
                    idct4_add(&c, &mut self.pic.planes[0][(y0 + by * 4) * ls + x0 + bx * 4..], ls);
                }
            }
        }
        self.reconstruct_chroma(res)
    }

    // ---- prediction modes ------------------------------------------------------------------------

    fn assign_pred_modes(&mut self, syn: &[(bool, u8); 16]) -> Result<()> {
        let (la, ta) = (self.avail_left(), self.avail_top());
        let t8 = self.cur.t8x8;
        let step = if t8 { 2 } else { 1 };
        let count = if t8 { 4 } else { 16 };
        for n in 0..count {
            let (bx, by) = if t8 { ((n & 1) * 2, (n >> 1) * 2) } else { block_pos(n) };
            let mode_of = |this: &Self, x: i32, y: i32| -> Option<u8> {
                // None: the neighbour is unavailable; Some(2): it is not an NxN macroblock
                if x >= 0 && y >= 0 {
                    return Some(this.cur.ipm[(y * 4 + x) as usize]);
                }
                let m = if x < 0 {
                    if !la {
                        return None;
                    }
                    this.left()
                } else {
                    if !ta {
                        return None;
                    }
                    this.top()
                };
                Some(match m.kind {
                    Kind::I4x4 | Kind::I8x8 => m.ipm[(if x < 0 { y * 4 + 3 } else { 12 + x }) as usize],
                    _ => 2,
                })
            };
            let a = mode_of(self, bx as i32 - 1, by as i32);
            let b = mode_of(self, bx as i32, by as i32 - 1);
            let pred = match (a, b) {
                (Some(a), Some(b)) => a.min(b),
                _ => 2,
            };
            let (prev, rem) = syn[n];
            let mode = if prev {
                pred
            } else if rem < pred {
                rem
            } else {
                rem + 1
            };
            for dy in 0..step {
                for dx in 0..step {
                    self.cur.ipm[(by + dy) * 4 + bx + dx] = mode;
                }
            }
        }
        Ok(())
    }

    // ---- residual parsing --------------------------------------------------------------------------

    /// nC for a luma block (CAVLC) or the neighbour flags for coded_block_flag (CABAC).
    fn neighbour_nnz_luma(&self, bx: usize, by: usize) -> (Option<u8>, Option<u8>) {
        let a = if bx > 0 {
            Some(self.cur.nnz[by * 4 + bx - 1])
        } else if self.avail_left() {
            Some(self.left().nnz[by * 4 + 3])
        } else {
            None
        };
        let b = if by > 0 {
            Some(self.cur.nnz[(by - 1) * 4 + bx])
        } else if self.avail_top() {
            Some(self.top().nnz[12 + bx])
        } else {
            None
        };
        (a, b)
    }

    fn neighbour_nnz_chroma(&self, comp: usize, bx: usize, by: usize) -> (Option<u8>, Option<u8>) {
        let base = 16 + 4 * comp;
        let a = if bx > 0 {
            Some(self.cur.nnz[base + by * 2])
        } else if self.avail_left() {
            Some(self.left().nnz[base + by * 2 + 1])
        } else {
            None
        };
        let b = if by > 0 {
            Some(self.cur.nnz[base + bx])
        } else if self.avail_top() {
            Some(self.top().nnz[base + 2 + bx])
        } else {
            None
        };
        (a, b)
    }

    fn nc_from(a: Option<u8>, b: Option<u8>) -> i32 {
        match (a, b) {
            (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
            (Some(a), None) => a as i32,
            (None, Some(b)) => b as i32,
            (None, None) => 0,
        }
    }

    fn read_residual(&mut self, res: &mut Res, cbp: u32, i16: bool) -> Result<()> {
        let t8 = self.cur.t8x8;
        if i16 {
            // Intra16x16DCLevel
            let mut lv = [0i32; 16];
            let n = self.block(0, 0, 0, 16, &mut lv, BlockKind::LumaDc)?;
            if n > 0 {
                self.cur.cbf_dc |= 1;
            }
            for (k, &p) in ZIGZAG4.iter().enumerate() {
                res.dc[p as usize] = lv[k];
            }
        }
        for i8 in 0..4usize {
            if cbp & (1 << i8) == 0 {
                continue;
            }
            if t8 {
                self.read_luma8x8(res, i8)?;
                continue;
            }
            for i4 in 0..4usize {
                let (bx, by) = ((i8 & 1) * 2 + (i4 & 1), (i8 >> 1) * 2 + (i4 >> 1));
                let mut lv = [0i32; 16];
                let (kind, max) = if i16 { (BlockKind::LumaAc, 15) } else { (BlockKind::Luma4x4, 16) };
                let n = self.block(0, bx, by, max, &mut lv, kind)?;
                self.cur.nnz[by * 4 + bx] = n as u8;
                let start = if i16 { 1 } else { 0 };
                for k in 0..max {
                    res.luma[(by * 4 + bx) * 16 + ZIGZAG4[k + start] as usize] = lv[k];
                }
            }
        }
        let chroma = cbp >> 4;
        if chroma > 0 {
            for comp in 0..2usize {
                let mut lv = [0i32; 16];
                let n = self.block(comp + 1, 0, 0, 4, &mut lv, BlockKind::ChromaDc)?;
                if n > 0 {
                    self.cur.cbf_dc |= 2 << comp;
                }
                res.cdc[comp].copy_from_slice(&lv[..4]);
            }
        }
        if chroma == 2 {
            for comp in 0..2usize {
                for blk in 0..4usize {
                    let (bx, by) = (blk & 1, blk >> 1);
                    let mut lv = [0i32; 16];
                    let n = self.block(comp + 1, bx, by, 15, &mut lv, BlockKind::ChromaAc)?;
                    self.cur.nnz[16 + 4 * comp + by * 2 + bx] = n as u8;
                    for k in 0..15 {
                        res.cac[comp * 4 + blk][ZIGZAG4[k + 1] as usize] = lv[k];
                    }
                }
            }
        }
        Ok(())
    }

    fn read_luma8x8(&mut self, res: &mut Res, i8: usize) -> Result<()> {
        let (bx0, by0) = ((i8 & 1) * 2, (i8 >> 1) * 2);
        let cavlc_mode = matches!(self.rd, Reader::Cavlc(_));
        match cavlc_mode {
            true => {
                for j in 0..4usize {
                    let (bx, by) = (bx0 + (j & 1), by0 + (j >> 1));
                    let mut lv = [0i32; 16];
                    let n = self.block(0, bx, by, 16, &mut lv, BlockKind::Luma4x4)?;
                    self.cur.nnz[by * 4 + bx] = n as u8;
                    for k in 0..16 {
                        res.luma[i8 * 64 + ZIGZAG_DIRECT[4 * k + j] as usize] = lv[k];
                    }
                }
            }
            false => {
                let mut lv = [0i32; 64];
                let n = self.cabac_block(0, 0, 0, 64, BlockKind::Luma8x8, &mut lv)?;
                for (k, &v) in lv.iter().enumerate() {
                    res.luma[i8 * 64 + ZIGZAG_DIRECT[k] as usize] = v;
                }
                for j in 0..4usize {
                    self.cur.nnz[(by0 + (j >> 1)) * 4 + bx0 + (j & 1)] = n.min(255) as u8;
                }
            }
        }
        Ok(())
    }

    /// One 4x4-style block through whichever entropy coder the slice uses. Returns the number of
    /// non-zero coefficients; levels come back in scan order.
    fn block(&mut self, comp: usize, bx: usize, by: usize, max: usize, out: &mut [i32; 16], kind: BlockKind) -> Result<usize> {
        match self.rd {
            Reader::Cavlc(_) => {
                let nc = match kind {
                    BlockKind::ChromaDc => -1,
                    BlockKind::ChromaAc => {
                        let (a, b) = self.neighbour_nnz_chroma(comp - 1, bx, by);
                        Self::nc_from(a, b)
                    }
                    _ => {
                        let (a, b) = self.neighbour_nnz_luma(bx, by);
                        Self::nc_from(a, b)
                    }
                };
                let Reader::Cavlc(r) = &mut self.rd else { unreachable!() };
                cavlc::residual_block(r, nc, max, out)
            }
            Reader::Cabac(_) => {
                let mut big = [0i32; 64];
                let n = self.cabac_block(comp, bx, by, max, kind, &mut big)?;
                out.copy_from_slice(&big[..16]);
                Ok(n)
            }
        }
    }

    // ---- CABAC residual ----------------------------------------------------------------------------

    fn cabac_block(&mut self, comp: usize, bx: usize, by: usize, max: usize, kind: BlockKind, out: &mut [i32; 64]) -> Result<usize> {
        out.fill(0);
        let cat = match kind {
            BlockKind::LumaDc => 0,
            BlockKind::LumaAc => 1,
            BlockKind::Luma4x4 => 2,
            BlockKind::ChromaDc => 3,
            BlockKind::ChromaAc => 4,
            BlockKind::Luma8x8 => 5,
        };
        if kind != BlockKind::Luma8x8 {
            // coded_block_flag; an unavailable neighbour counts as coded in an intra macroblock
            let un = self.cur.kind != Kind::Inter;
            let (a, b) = match kind {
                BlockKind::LumaDc => {
                    let f = |avail: bool, m: &MbInfo| if !avail { un } else { m.kind == Kind::IPcm || m.cbf_dc & 1 != 0 };
                    (f(self.avail_left(), self.left_or_cur()), f(self.avail_top(), self.top_or_cur()))
                }
                BlockKind::ChromaDc => {
                    let bit = 2u8 << (comp - 1);
                    let f = |avail: bool, m: &MbInfo| if !avail { un } else { m.kind == Kind::IPcm || m.cbf_dc & bit != 0 };
                    (f(self.avail_left(), self.left_or_cur()), f(self.avail_top(), self.top_or_cur()))
                }
                BlockKind::ChromaAc => {
                    let (a, b) = self.neighbour_nnz_chroma(comp - 1, bx, by);
                    (a.map_or(un, |v| v > 0), b.map_or(un, |v| v > 0))
                }
                _ => {
                    let (a, b) = self.neighbour_nnz_luma(bx, by);
                    (a.map_or(un, |v| v > 0), b.map_or(un, |v| v > 0))
                }
            };
            let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
            let cat_off = [0usize, 4, 8, 12, 16][cat];
            if c.decision(85 + cat_off + a as usize + 2 * b as usize) == 0 {
                return Ok(0);
            }
        }
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        let mut sig = [0usize; 64];
        let mut n = 0;
        let last_idx = max - 1;
        let mut i = 0;
        while i < last_idx {
            let (sctx, lctx) = if cat == 5 {
                (SIG_OFF[0][5] as usize + SIG_OFF_8X8[0][i] as usize, LAST_OFF[0][5] as usize + LAST_OFF_8X8[i] as usize)
            } else {
                let inc = if cat == 3 { i.min(2) } else { i };
                (SIG_OFF[0][cat] as usize + inc, LAST_OFF[0][cat] as usize + inc)
            };
            if c.decision(sctx) == 1 {
                sig[n] = i;
                n += 1;
                if c.decision(lctx) == 1 {
                    break;
                }
            }
            i += 1;
        }
        if i == last_idx {
            // reaching the last position means that coefficient is significant
            sig[n] = last_idx;
            n += 1;
        }
        let (mut eq1, mut gt1) = (0usize, 0usize);
        for k in (0..n).rev() {
            let base = ABS_OFF[cat] as usize;
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let mut abs;
            if c.decision(base + inc0) == 0 {
                abs = 1;
                eq1 += 1;
            } else {
                let incn = 5 + gt1.min(4 - (cat == 3) as usize);
                let mut prefix = 1;
                while prefix < 14 && c.decision(base + incn) == 1 {
                    prefix += 1;
                }
                abs = prefix + 1;
                if prefix == 14 {
                    let mut kk = 0;
                    let mut v = 0i32;
                    while c.bypass() == 1 {
                        v += 1 << kk;
                        kk += 1;
                        if kk > 24 {
                            return Err(Error::invalid_input("H.264 CABAC level suffix is too long"));
                        }
                    }
                    while kk > 0 {
                        kk -= 1;
                        v += (c.bypass() as i32) << kk;
                    }
                    abs += v;
                }
                gt1 += 1;
            }
            let neg = c.bypass() == 1;
            out[sig[k]] = if neg { -abs } else { abs };
        }
        Ok(n)
    }

    /// The neighbouring macroblock's info, or the current one when asked about "inside" (never
    /// reached for the MB-level neighbours used by the DC flags).
    fn left_or_cur(&self) -> &MbInfo {
        if self.mbx > 0 {
            self.left()
        } else {
            &self.cur
        }
    }
    fn top_or_cur(&self) -> &MbInfo {
        if self.mby > 0 {
            self.top()
        } else {
            &self.cur
        }
    }

    // ---- reconstruction ----------------------------------------------------------------------------

    fn luma_stride(&self) -> usize {
        self.pic.mb_w * 16
    }

    fn pix(&self, plane: usize, x: usize, y: usize) -> i32 {
        let s = if plane == 0 { self.pic.mb_w * 16 } else { self.pic.mb_w * 8 };
        self.pic.planes[plane][y * s + x] as i32
    }

    fn reconstruct(&mut self, res: &Res, i16_mode: u8, _cbp: u32) -> Result<()> {
        let qp = self.qp;
        let (mbx, mby) = (self.mbx, self.mby);
        let (la, ta, tla) = (self.avail_left(), self.avail_top(), self.avail_topleft());
        let ls = self.luma_stride();
        match self.cur.kind {
            Kind::I16x16 => {
                if (i16_mode == 0 && !ta) || (i16_mode == 1 && !la) || (i16_mode == 3 && !(ta && la && tla)) {
                    return Err(Error::invalid_input("H.264 Intra16x16 prediction uses neighbours that are not available"));
                }
                let (x0, y0) = (mbx * 16, mby * 16);
                let mut top = [0i32; 16];
                let mut left = [0i32; 16];
                if ta {
                    for (i, t) in top.iter_mut().enumerate() {
                        *t = self.pix(0, x0 + i, y0 - 1);
                    }
                }
                if la {
                    for (j, l) in left.iter_mut().enumerate() {
                        *l = self.pix(0, x0 - 1, y0 + j);
                    }
                }
                let tl = if tla { self.pix(0, x0 - 1, y0 - 1) } else { 0 };
                let mut pred = [0u8; 256];
                predict_16x16(i16_mode, &top, &left, tl, ta, la, &mut pred);
                for y in 0..16 {
                    self.pic.planes[0][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
                }
                // DC: inverse Hadamard, then scale
                let f = hadamard4(&res.dc);
                let lsd = self.scale.l4[0][(qp % 6) as usize][0];
                let mut dcy = [0i32; 16];
                for k in 0..16 {
                    dcy[k] = if qp >= 36 { f[k].wrapping_mul(lsd) << (qp / 6 - 6) } else { (f[k].wrapping_mul(lsd) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                }
                for b in 0..16 {
                    let mut c = self.scale4(&res.luma[b * 16..b * 16 + 16], 0, qp, true);
                    c[0] = dcy[b];
                    if c.iter().any(|&v| v != 0) {
                        let (bx, by) = (b % 4, b / 4);
                        idct4_add(&c, &mut self.pic.planes[0][(y0 + by * 4) * ls + x0 + bx * 4..], ls);
                    }
                }
            }
            Kind::I4x4 => {
                for n in 0..16 {
                    let (bx, by) = block_pos(n);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let e = self.edges_4x4(bx, by);
                    check_mode(mode, &e)?;
                    let mut pred = [0u8; 16];
                    predict_nxn(4, mode, &e, &mut pred);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..4 {
                        self.pic.planes[0][(y + r) * ls + x..(y + r) * ls + x + 4].copy_from_slice(&pred[r * 4..r * 4 + 4]);
                    }
                    let c = self.scale4(&res.luma[(by * 4 + bx) * 16..(by * 4 + bx) * 16 + 16], 0, qp, false);
                    if c.iter().any(|&v| v != 0) {
                        idct4_add(&c, &mut self.pic.planes[0][y * ls + x..], ls);
                    }
                }
            }
            Kind::I8x8 => {
                for i8 in 0..4 {
                    let (bx, by) = ((i8 & 1) * 2, (i8 >> 1) * 2);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let mut e = self.edges_8x8(i8);
                    check_mode(mode, &e)?;
                    filter_8x8_edges(&mut e);
                    let mut pred = [0u8; 64];
                    predict_nxn(8, mode, &e, &mut pred);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..8 {
                        self.pic.planes[0][(y + r) * ls + x..(y + r) * ls + x + 8].copy_from_slice(&pred[r * 8..r * 8 + 8]);
                    }
                    let mut c = [0i32; 64];
                    let lsv = &self.scale.l8[0][(qp % 6) as usize];
                    for k in 0..64 {
                        let v = res.luma[i8 * 64 + k];
                        if v != 0 {
                            c[k] = if qp >= 36 { v.wrapping_mul(lsv[k]) << (qp / 6 - 6) } else { (v.wrapping_mul(lsv[k]) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                        }
                    }
                    if c.iter().any(|&v| v != 0) {
                        idct8_add(&c, &mut self.pic.planes[0][y * ls + x..], ls);
                    }
                }
            }
            _ => {}
        }
        self.reconstruct_chroma(res)
    }

    /// Scales one 4x4 block's levels. `list` selects the matrix (0 = intra Y, 1 = Cb, 2 = Cr);
    /// with `skip_dc` the DC position is left for the caller.
    fn scale4(&self, lv: &[i32], list: usize, qp: i32, skip_dc: bool) -> [i32; 16] {
        let mut c = [0i32; 16];
        let ls = &self.scale.l4[list][(qp % 6) as usize];
        for k in (skip_dc as usize)..16 {
            let v = lv[k];
            if v != 0 {
                c[k] = if qp >= 24 { v.wrapping_mul(ls[k]) << (qp / 6 - 4) } else { (v.wrapping_mul(ls[k]) + (1 << (3 - qp / 6))) >> (4 - qp / 6) };
            }
        }
        c
    }

    fn reconstruct_chroma(&mut self, res: &Res) -> Result<()> {
        let (mbx, mby) = (self.mbx, self.mby);
        let (la, ta, tla) = (self.avail_left(), self.avail_top(), self.avail_topleft());
        let mode = self.cur.chroma_mode;
        let inter = self.cur.kind == Kind::Inter;
        if !inter && ((mode == 1 && !la) || (mode == 2 && !ta) || (mode == 3 && !(la && ta && tla))) {
            return Err(Error::invalid_input("H.264 chroma prediction uses neighbours that are not available"));
        }
        let cs = self.pic.mb_w * 8;
        let (x0, y0) = (mbx * 8, mby * 8);
        let chroma_cbp = self.cur.cbp >> 4;
        for comp in 0..2usize {
            let plane = 1 + comp;
            if !inter {
                let mut top = [0i32; 8];
                let mut left = [0i32; 8];
                if ta {
                    for (i, t) in top.iter_mut().enumerate() {
                        *t = self.pix(plane, x0 + i, y0 - 1);
                    }
                }
                if la {
                    for (j, l) in left.iter_mut().enumerate() {
                        *l = self.pix(plane, x0 - 1, y0 + j);
                    }
                }
                let tl = if tla { self.pix(plane, x0 - 1, y0 - 1) } else { 0 };
                let mut pred = [0u8; 64];
                predict_chroma(mode, &top, &left, tl, ta, la, &mut pred);
                for y in 0..8 {
                    self.pic.planes[plane][(y0 + y) * cs + x0..(y0 + y) * cs + x0 + 8].copy_from_slice(&pred[y * 8..y * 8 + 8]);
                }
            }
            let list = if inter { 4 + comp } else { 1 + comp };
            if chroma_cbp == 0 {
                continue;
            }
            let qpc = chroma_qp(self.qp, self.pps.chroma_qp_offset[comp]);
            // DC: 2x2 transform, then scale
            let c = &res.cdc[comp];
            let f = [c[0] + c[1] + c[2] + c[3], c[0] - c[1] + c[2] - c[3], c[0] + c[1] - c[2] - c[3], c[0] - c[1] - c[2] + c[3]];
            let lsd = self.scale.l4[list][(qpc % 6) as usize][0];
            let mut dc = [0i32; 4];
            for k in 0..4 {
                dc[k] = (f[k].wrapping_mul(lsd) << (qpc / 6)) >> 5;
            }
            for blk in 0..4usize {
                let mut cf = self.scale4(&res.cac[comp * 4 + blk], list, qpc, true);
                cf[0] = dc[blk];
                if cf.iter().any(|&v| v != 0) {
                    let (bx, by) = (blk & 1, blk >> 1);
                    idct4_add(&cf, &mut self.pic.planes[plane][(y0 + by * 4) * cs + x0 + bx * 4..], cs);
                }
            }
        }
        Ok(())
    }

    fn edges_4x4(&self, bx: usize, by: usize) -> Edges {
        let (la, ta, tla, tra) = (self.avail_left(), self.avail_top(), self.avail_topleft(), self.avail_topright());
        let have_left = bx > 0 || la;
        let have_top = by > 0 || ta;
        let have_tl = match (bx > 0, by > 0) {
            (true, true) => true,
            (false, true) => la,
            (true, false) => ta,
            (false, false) => tla,
        };
        let have_tr = if by == 0 {
            if bx < 3 {
                ta
            } else {
                tra
            }
        } else if bx == 3 {
            false
        } else {
            zorder(bx + 1, by - 1) < zorder(bx, by)
        };
        let (x0, y0) = (self.mbx * 16 + bx * 4, self.mby * 16 + by * 4);
        let mut e = Edges { top: [0; 16], left: [0; 8], topleft: 0, have_top, have_left, have_topleft: have_tl };
        if have_top {
            for i in 0..4 {
                e.top[i] = self.pix(0, x0 + i, y0 - 1);
            }
            for i in 4..8 {
                e.top[i] = if have_tr { self.pix(0, x0 + i, y0 - 1) } else { e.top[3] };
            }
        }
        if have_left {
            for j in 0..4 {
                e.left[j] = self.pix(0, x0 - 1, y0 + j);
            }
        }
        if have_tl {
            e.topleft = self.pix(0, x0 - 1, y0 - 1);
        }
        e
    }

    fn edges_8x8(&self, i8: usize) -> Edges {
        let (la, ta, tla, tra) = (self.avail_left(), self.avail_top(), self.avail_topleft(), self.avail_topright());
        let (bx, by) = (i8 & 1, i8 >> 1);
        let have_left = bx > 0 || la;
        let have_top = by > 0 || ta;
        let have_tl = match (bx > 0, by > 0) {
            (true, true) => true,
            (false, true) => la,
            (true, false) => ta,
            (false, false) => tla,
        };
        let have_tr = match i8 {
            0 => ta,
            1 => tra,
            2 => true,
            _ => false,
        };
        let (x0, y0) = (self.mbx * 16 + bx * 8, self.mby * 16 + by * 8);
        let mut e = Edges { top: [0; 16], left: [0; 8], topleft: 0, have_top, have_left, have_topleft: have_tl };
        if have_top {
            for i in 0..8 {
                e.top[i] = self.pix(0, x0 + i, y0 - 1);
            }
            for i in 8..16 {
                e.top[i] = if have_tr { self.pix(0, x0 + i, y0 - 1) } else { e.top[7] };
            }
        }
        if have_left {
            for j in 0..8 {
                e.left[j] = self.pix(0, x0 - 1, y0 + j);
            }
        }
        if have_tl {
            e.topleft = self.pix(0, x0 - 1, y0 - 1);
        }
        e
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    LumaDc,
    LumaAc,
    Luma4x4,
    ChromaDc,
    ChromaAc,
    Luma8x8,
}

/// A directional mode may only use neighbours the stream provides.
fn check_mode(mode: u8, e: &Edges) -> Result<()> {
    let ok = match mode {
        V_PRED | DDL_PRED | VL_PRED => e.have_top,
        H_PRED | HU_PRED => e.have_left,
        DC_PRED => true,
        DDR_PRED | VR_PRED | HD_PRED => e.have_top && e.have_left && e.have_topleft,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::invalid_input("H.264 intra prediction mode uses neighbours that are not available"))
    }
}
