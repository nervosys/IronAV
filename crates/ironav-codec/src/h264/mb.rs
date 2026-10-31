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
use ironav_core::{Error, Result};

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
    /// Non-zero coefficient counts: 16 luma blocks in raster order, then 8 Cb, then 8 Cr (4 of each
    /// for 4:2:0).
    pub nnz: [u8; 48],
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
    /// Samples the loop filter must leave alone: I_PCM and lossless (transform bypass) macroblocks.
    pub keep: bool,
    /// B_Skip or B_Direct_16x16.
    pub bdirect: bool,
    /// 8x8 blocks predicted in direct mode (bit per block).
    pub direct8: u8,
    /// Absolute motion vector differences (clipped) per list and 4x4 block, for CABAC contexts.
    pub mvd: [[[u8; 2]; 16]; 2],
}

impl Default for MbInfo {
    fn default() -> Self {
        MbInfo {
            slice: 0,
            kind: Kind::None,
            qp: 0,
            nnz: [0; 48],
            cbf_dc: 0,
            cbp: 0,
            chroma_mode: 0,
            t8x8: false,
            ipm: [2; 16],
            mv: [[[0; 2]; 16]; 2],
            ref_idx: [[-1; 4]; 2],
            ref_id: [[-1; 4]; 2],
            skip: false,
            keep: false,
            bdirect: false,
            direct8: 0,
            mvd: [[[0; 2]; 16]; 2],
        }
    }
}

pub struct Picture {
    pub mb_w: usize,
    pub mb_h: usize,
    /// chroma_format_idc (0 monochrome, 1 4:2:0, 2 4:2:2) and the height of a macroblock's chroma.
    pub cf: u8,
    /// Width and height of a macroblock's chroma samples.
    pub cw: usize,
    pub ch: usize,
    pub planes: [Vec<u16>; 3],
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
    pub fn new(mb_w: usize, mb_h: usize, cf: u8, bd: u32) -> Self {
        let ch = if cf >= 2 { 16 } else { 8 };
        let cw = if cf == 3 { 16 } else { 8 };
        // a monochrome picture keeps chroma planes at mid-grey, as other decoders output it
        let grey = 1u16 << (bd - 1);
        Picture {
            mb_w,
            mb_h,
            cf,
            cw,
            ch,
            planes: [vec![0; mb_w * mb_h * 256], vec![grey; mb_w * mb_h * cw * ch], vec![grey; mb_w * mb_h * cw * ch]],
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
        for list in 0..6 {
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

/// QPc for a luma QP (8.5.8); negative values are possible with high bit depth.
pub fn chroma_qp(qp: i32, offset: i32, bd: u32) -> i32 {
    let qpi = (qp + offset).clamp(-6 * (bd as i32 - 8), 51);
    if qpi < 30 {
        qpi
    } else {
        CHROMA_QP[(qpi - 30) as usize] as i32
    }
}

/// Coefficients of one macroblock, as coded levels at raster positions within each block.
struct Res {
    /// 16 blocks of 4x4 (raster block order), or 4 blocks of 8x8, in the same 256 entries.
    luma: [[i32; 256]; 3],
    dc: [[i32; 16]; 3],
    cdc: [[i32; 8]; 2],
    /// Chroma AC: [component * 8 + block][raster position within the 4x4 block].
    cac: [[i32; 16]; 16],
}

impl Res {
    fn new() -> Self {
        Res { luma: [[0; 256]; 3], dc: [[0; 16]; 3], cdc: [[0; 8]; 2], cac: [[0; 16]; 16] }
    }
}

/// Intra residual DPCM of lossless coding (8.5.15): residuals accumulate along the prediction direction.
fn dpcm(r: &mut [i32], w: usize, h: usize, hor: bool) {
    if hor {
        for y in 0..h {
            for x in 1..w {
                r[y * w + x] += r[y * w + x - 1];
            }
        }
    } else {
        for y in 1..h {
            for x in 0..w {
                r[y * w + x] += r[(y - 1) * w + x];
            }
        }
    }
}

/// Adds a `w` x `h` residual to the samples at the start of `buf`.
fn add_residual(buf: &mut [u16], stride: usize, w: usize, h: usize, r: &[i32], maxv: i32) {
    for y in 0..h {
        for x in 0..w {
            let v = buf[y * stride + x] as i32 + r[y * w + x];
            buf[y * stride + x] = v.clamp(0, maxv) as u16;
        }
    }
}

// The CABAC state is held inline on purpose: boxing it would add an indirection to every bin read.
#[allow(clippy::large_enum_variant)]
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
    done: [u16; 2],
    cur_poc: i32,
    /// Bit depth, largest sample value and QpBdOffset (luma; chroma depth must match).
    bd: u32,
    maxv: i32,
    qp_off: i32,
}

/// Motion derived by direct prediction for a whole macroblock.
struct Direct {
    ref_idx: [[i8; 4]; 2],
    mv: [[[i16; 2]; 16]; 2],
}

/// Appends the motion partitions of the 8x8 block at (x0, y0): shape 0 = 8x8, 1 = 8x4, 2 = 4x8, 3 = 4x4.
#[allow(clippy::type_complexity)]
fn sub_parts(v: &mut Vec<(usize, usize, usize, usize, u8, u8, bool)>, x0: usize, y0: usize, mask: u8, shape: usize) {
    match shape {
        0 => v.push((x0, y0, 2, 2, mask, 0, false)),
        1 => v.extend([(x0, y0, 2, 1, mask, 0, false), (x0, y0 + 1, 2, 1, mask, 0, false)]),
        2 => v.extend([(x0, y0, 1, 2, mask, 0, false), (x0 + 1, y0, 1, 2, mask, 0, false)]),
        _ => v.extend((0..4).map(|j| (x0 + (j & 1), y0 + (j >> 1), 1, 1, mask, 0, false))),
    }
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
    pub fn new(sps: &'a Sps, pps: &'a Pps, hdr: &'a SliceHeader, pic: &'a mut Picture, scale: &'a LevelScale, slice_id: u16, rbsp: &'a [u8], refs: &'a [RefList; 2], cur_poc: i32) -> Result<Self> {
        let rd = if pps.cabac {
            // cabac_alignment_one_bit up to the next byte
            let start = hdr.data_pos.div_ceil(8);
            Reader::Cabac(Cabac::new(rbsp, start, hdr.slice_type == SliceType::I, hdr.cabac_init_idc, hdr.qp))
        } else {
            let mut r = BitReader::new(rbsp);
            r.skip(hdr.data_pos);
            Reader::Cavlc(r)
        };
        Ok(SliceDec { sps, pps, hdr, pic, scale, slice_id, qp: hdr.qp, last_dqp: false, mbx: 0, mby: 0, cur: MbInfo::default(), rd, refs, done: [0; 2], cur_poc, bd: sps.bit_depth_luma, maxv: (1 << sps.bit_depth_luma) - 1, qp_off: 6 * (sps.bit_depth_luma as i32 - 8) })
    }

    fn start_mb(&mut self, addr: usize) -> Result<()> {
        if addr >= self.pic.mb_w * self.pic.mb_h {
            return Err(Error::invalid_input("H.264 slice runs past the end of the picture"));
        }
        self.mbx = addr % self.pic.mb_w;
        self.mby = addr / self.pic.mb_w;
        self.cur = MbInfo { slice: self.slice_id, ..MbInfo::default() };
        self.done = [0; 2];
        Ok(())
    }

    fn finish_mb(&mut self, addr: usize) {
        self.cur.qp = if self.cur.kind == Kind::IPcm { 0 } else { self.qp as i8 };
        self.cur.keep = self.cur.kind == Kind::IPcm || self.bypass();
        self.pic.mbs[addr] = self.cur;
    }

    pub fn decode(&mut self) -> Result<()> {
        let inter_slice = self.hdr.slice_type != SliceType::I;
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
    /// Neighbour availability for intra sample and mode prediction: with constrained intra
    /// prediction, inter-coded neighbours do not count.
    fn intra_usable(&self, idx: usize) -> bool {
        !self.pps.constrained_intra_pred || self.pic.mbs[idx].kind != Kind::Inter
    }
    fn avail_left_i(&self) -> bool {
        self.avail_left() && self.intra_usable(self.idx() - 1)
    }
    fn avail_top_i(&self) -> bool {
        self.avail_top() && self.intra_usable(self.idx() - self.pic.mb_w)
    }
    fn avail_topleft_i(&self) -> bool {
        self.avail_topleft() && self.intra_usable(self.idx() - self.pic.mb_w - 1)
    }
    fn avail_topright_i(&self) -> bool {
        self.avail_topright() && self.intra_usable(self.idx() - self.pic.mb_w + 1)
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
        if self.hdr.slice_type != SliceType::I {
            let off = if self.hdr.slice_type == SliceType::P { 5 } else { 23 };
            if mb_type < off {
                return self.decode_inter_mb(mb_type);
            }
            mb_type -= off;
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
            if matches!(self.pic.cf, 1 | 2) {
                self.cur.chroma_mode = self.read_chroma_pred_mode()?;
            }
            cbp = self.read_cbp()?;
        } else if mb_type <= 24 {
            self.cur.kind = Kind::I16x16;
            let t = mb_type - 1;
            i16_mode = (t % 4) as u8;
            let chroma = if matches!(self.pic.cf, 1 | 2) { (t / 4) % 3 } else { 0 };
            cbp = (chroma << 4) | if t >= 12 { 15 } else { 0 };
            if matches!(self.pic.cf, 1 | 2) {
                self.cur.chroma_mode = self.read_chroma_pred_mode()?;
            }
        } else {
            return Err(Error::invalid_input("H.264 macroblock type is out of range for an I slice"));
        }
        self.cur.cbp = cbp as u8;
        if cbp > 0 || self.cur.kind == Kind::I16x16 {
            let dqp = self.read_qp_delta()?;
            self.qp = (self.qp + dqp + 52 + 2 * self.qp_off).rem_euclid(52 + self.qp_off) - self.qp_off;
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
        if self.hdr.slice_type == SliceType::B && matches!(self.rd, Reader::Cabac(_)) {
            return Ok(self.cabac_b_mb_type());
        }
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
        let gray = matches!(self.pic.cf, 0 | 3);
        match &mut self.rd {
            Reader::Cavlc(r) => {
                let k = r.ue()?;
                if k > if gray { 15 } else { 47 } {
                    return Err(Error::invalid_input("H.264 coded_block_pattern code is out of range"));
                }
                Ok(if gray { GOLOMB_TO_INTRA4X4_CBP_GRAY[k as usize] } else { GOLOMB_TO_INTRA4X4_CBP[k as usize] } as u32)
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
                if !gray && c.decision(77 + ca as usize + 2 * cb as usize) == 1 {
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
                if !(-(26 + self.qp_off / 2)..=25 + self.qp_off / 2).contains(&v) {
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
        self.cur.nnz = [16; 48];
        self.cur.cbf_dc = 7;
        self.cur.cbp = 0x2F;
        self.last_dqp = false;
        let mut samples = [0u16; 768];
        let nsamples = 256 + 2 * [0, 64, 128, 256][self.pic.cf as usize];
        let bd = self.bd;
        match &mut self.rd {
            Reader::Cavlc(r) => {
                r.align();
                for s in samples[..nsamples].iter_mut() {
                    *s = r.bits(bd)? as u16;
                }
            }
            Reader::Cabac(c) => {
                let start = c.pcm_byte_pos();
                let len = nsamples * bd as usize / 8;
                let d = c.data();
                let src = d.get(start..start + len).ok_or_else(|| Error::invalid_input("H.264 I_PCM samples are cut short"))?;
                let mut r = BitReader::new(src);
                for s in samples[..nsamples].iter_mut() {
                    *s = r.bits(bd)? as u16;
                }
                c.restart_at(start + len);
            }
        }
        let (mbx, mby) = (self.mbx, self.mby);
        let ys = self.pic.mb_w * 16;
        for y in 0..16 {
            let o = (mby * 16 + y) * ys + mbx * 16;
            self.pic.planes[0][o..o + 16].copy_from_slice(&samples[y * 16..y * 16 + 16]);
        }
        let (cw, chh) = (self.pic.cw, self.pic.ch);
        let cs = self.pic.mb_w * cw;
        if self.pic.cf != 0 {
            for c in 0..2 {
                for y in 0..chh {
                    let o = (mby * chh + y) * cs + mbx * cw;
                    let s = 256 + c * chh * cw + y * cw;
                    self.pic.planes[1 + c][o..o + cw].copy_from_slice(&samples[s..s + cw]);
                }
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
                if self.done[l] & (1 << (y * 4 + x)) == 0 {
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
                self.done[l] |= 1 << (y * 4 + x);
            }
        }
    }

    fn mark_done(&mut self, l: usize, bx: usize, by: usize, w: usize, h: usize) {
        for y in by..by + h {
            for x in bx..bx + w {
                self.done[l] |= 1 << (y * 4 + x);
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

    // ---- direct prediction (8.4.1.2) ---------------------------------------------------------------

    /// Motion of the co-located block in RefPicList1[0] for the 4x4 block (bx, by):
    /// (reference index in the co-located picture's own list, vector, id of the picture it names).
    fn col_block(&self, bx: usize, by: usize) -> Result<Option<(i8, [i16; 2], i32)>> {
        let Some(Some(e)) = self.refs[1].first() else {
            return Err(Error::invalid_input("H.264 direct prediction needs a picture in reference list 1"));
        };
        let m = match e.pic.mbs.get(self.idx()) {
            Some(m) => m,
            None => return Ok(None),
        };
        let (cx, cy) = if self.sps.direct_8x8_inference { ((bx >> 1) * 3, (by >> 1) * 3) } else { (bx, by) };
        if m.kind != Kind::Inter {
            return Ok(None);
        }
        let b8 = (cy >> 1) * 2 + (cx >> 1);
        for l in 0..2 {
            if m.ref_idx[l][b8] >= 0 {
                return Ok(Some((m.ref_idx[l][b8], m.mv[l][cy * 4 + cx], m.ref_id[l][b8])));
            }
        }
        Ok(None)
    }

    fn direct(&self) -> Result<Direct> {
        if self.hdr.direct_spatial_mv_pred {
            self.direct_spatial()
        } else {
            self.direct_temporal()
        }
    }

    fn direct_spatial(&self) -> Result<Direct> {
        let mut refs = [-1i8; 2];
        for (l, rf) in refs.iter_mut().enumerate() {
            let a = self.nb_motion(l, -1, 0);
            let b = self.nb_motion(l, 0, -1);
            let c = self.nb_motion(l, 4, -1).or_else(|| self.nb_motion(l, -1, -1));
            let r = |n: Option<(i8, [i16; 2])>| n.map_or(-1, |v| v.0);
            let minpos = |x: i8, y: i8| if x >= 0 && y >= 0 { x.min(y) } else { x.max(y) };
            *rf = minpos(r(a), minpos(r(b), r(c)));
        }
        let mut d = Direct { ref_idx: [[-1; 4]; 2], mv: [[[0; 2]; 16]; 2] };
        if refs[0] < 0 && refs[1] < 0 {
            d.ref_idx = [[0; 4]; 2];
            // still need the co-located picture to exist for a conforming stream
            return Ok(d);
        }
        for l in 0..2 {
            if refs[l] >= 0 {
                d.ref_idx[l] = [refs[l]; 4];
                let mv = self.pred_mv(l, 0, 0, 4, 0, refs[l]);
                d.mv[l] = [mv; 16];
            }
        }
        let l1_short = matches!(self.refs[1].first(), Some(Some(e)) if !e.long);
        if l1_short {
            for by in 0..4 {
                for bx in 0..4 {
                    if let Some((r, mv, _)) = self.col_block(bx, by)? {
                        if r == 0 && mv[0].abs() <= 1 && mv[1].abs() <= 1 {
                            for l in 0..2 {
                                if refs[l] == 0 {
                                    d.mv[l][by * 4 + bx] = [0, 0];
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(d)
    }

    fn direct_temporal(&self) -> Result<Direct> {
        let Some(Some(e1)) = self.refs[1].first() else {
            return Err(Error::invalid_input("H.264 direct prediction needs a picture in reference list 1"));
        };
        let mut d = Direct { ref_idx: [[0; 4]; 2], mv: [[[0; 2]; 16]; 2] };
        for i8 in 0..4usize {
            let (bx0, by0) = ((i8 & 1) * 2, (i8 >> 1) * 2);
            let col = self.col_block(bx0, by0)?;
            let ref0 = match col {
                Some((_, _, id)) => match self.refs[0].iter().position(|r| matches!(r, Some(e) if e.pic.id == id)) {
                    Some(p) => p,
                    None => return Err(Error::invalid_input("H.264 temporal direct prediction names a picture that is not in reference list 0")),
                },
                None => 0,
            };
            let Some(Some(e0)) = self.refs[0].get(ref0) else {
                return Err(Error::invalid_input("H.264 temporal direct prediction needs a picture in reference list 0"));
            };
            d.ref_idx[0][i8] = ref0 as i8;
            d.ref_idx[1][i8] = 0;
            let tb = (self.cur_poc - e0.pic.poc).clamp(-128, 127);
            let td = (e1.pic.poc - e0.pic.poc).clamp(-128, 127);
            for j in 0..4usize {
                let (bx, by) = (bx0 + (j & 1), by0 + (j >> 1));
                let mvcol = match self.col_block(bx, by)? {
                    Some((_, mv, _)) => mv,
                    None => [0, 0],
                };
                let (m0, m1) = if e0.long || td == 0 {
                    (mvcol, [0, 0])
                } else {
                    let tx = (16384 + (td / 2).abs()) / td;
                    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                    let s = |v: i16| ((dsf * v as i32 + 128) >> 8) as i16;
                    let m0 = [s(mvcol[0]), s(mvcol[1])];
                    (m0, [m0[0].wrapping_sub(mvcol[0]), m0[1].wrapping_sub(mvcol[1])])
                };
                d.mv[0][by * 4 + bx] = m0;
                d.mv[1][by * 4 + bx] = m1;
            }
        }
        Ok(d)
    }

    /// Copies the derived direct motion of the 8x8 blocks in `mask` into the macroblock.
    fn put_direct(&mut self, d: &Direct, mask: u8) {
        for i8 in 0..4usize {
            if mask & (1 << i8) == 0 {
                continue;
            }
            let (bx0, by0) = ((i8 & 1) * 2, (i8 >> 1) * 2);
            for l in 0..2 {
                self.cur.ref_idx[l][i8] = d.ref_idx[l][i8];
                for j in 0..4usize {
                    let i = (by0 + (j >> 1)) * 4 + bx0 + (j & 1);
                    self.cur.mv[l][i] = if d.ref_idx[l][i8] >= 0 { d.mv[l][i] } else { [0, 0] };
                }
            }
        }
    }

    // ---- CABAC syntax elements of inter macroblocks --------------------------------------------------

    fn read_skip_flag(&mut self) -> bool {
        let a = self.avail_left() && !self.left().skip;
        let b = self.avail_top() && !self.top().skip;
        let base = if self.hdr.slice_type == SliceType::B { 24 } else { 11 };
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        c.decision(base + a as usize + b as usize) == 1
    }

    /// Whether the neighbouring partition at (x, y) (4x4 units, outside or inside the current
    /// macroblock) uses a reference index above zero in list `l` (CABAC ref_idx context).
    fn nb_ref_gt0(&self, l: usize, x: i32, y: i32) -> bool {
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
            (&self.cur, x, y)
        };
        let b8 = ((by >> 1) * 2 + (bx >> 1)) as usize;
        m.kind == Kind::Inter && m.direct8 & (1 << b8) == 0 && m.ref_idx[l][b8] > 0
    }

    /// Sum of the absolute mvd values of the neighbours (CABAC mvd context).
    fn nb_mvd(&self, l: usize, x: i32, y: i32, comp: usize) -> u32 {
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
        m.mvd[l][(by * 4 + bx) as usize][comp] as u32
    }

    fn cabac_ref_idx(&mut self, l: usize, bx: usize, by: usize, max: u32) -> Result<i8> {
        let a = self.nb_ref_gt0(l, bx as i32 - 1, by as i32);
        let b = self.nb_ref_gt0(l, bx as i32, by as i32 - 1);
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
        Ok(v as i8)
    }

    fn cabac_mvd_comp(&mut self, l: usize, bx: usize, by: usize, comp: usize) -> Result<i32> {
        let sum = self.nb_mvd(l, bx as i32 - 1, by as i32, comp) + self.nb_mvd(l, bx as i32, by as i32 - 1, comp);
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

    /// B-slice macroblock type bins (9.3.2.5); returns the CAVLC numbering (23 and up: intra).
    fn cabac_b_mb_type(&mut self) -> u32 {
        let a = self.avail_left() && !self.left().bdirect;
        let b = self.avail_top() && !self.top().bdirect;
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        if c.decision(27 + a as usize + b as usize) == 0 {
            return 0;
        }
        if c.decision(27 + 3) == 0 {
            return 1 + c.decision(27 + 5);
        }
        let mut bits = c.decision(27 + 4) << 3;
        bits |= c.decision(27 + 5) << 2;
        bits |= c.decision(27 + 5) << 1;
        bits |= c.decision(27 + 5);
        match bits {
            0..=7 => bits + 3,
            13 => {
                // intra macroblock type with the suffix contexts
                if c.decision(32) == 0 {
                    return 23;
                }
                if c.terminate() == 1 {
                    return 23 + 25;
                }
                let mut t = 1;
                t += 12 * c.decision(32 + 1);
                if c.decision(32 + 2) == 1 {
                    t += 4 + 4 * c.decision(32 + 2);
                }
                t += 2 * c.decision(32 + 3);
                t += c.decision(32 + 3);
                23 + t
            }
            14 => 11,
            15 => 22,
            _ => ((bits << 1) | c.decision(27 + 5)) - 4,
        }
    }

    fn cabac_b_sub_type(&mut self) -> u32 {
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        if c.decision(36) == 0 {
            return 0;
        }
        if c.decision(37) == 0 {
            return 1 + c.decision(39);
        }
        let mut t = 3;
        if c.decision(38) == 1 {
            if c.decision(39) == 1 {
                return 11 + c.decision(39);
            }
            t += 4;
        }
        t += 2 * c.decision(39);
        t += c.decision(39);
        t
    }

    // ---- inter macroblock syntax ---------------------------------------------------------------------

    fn decode_skip(&mut self) -> Result<()> {
        self.cur.kind = Kind::Inter;
        self.cur.skip = true;
        self.last_dqp = false;
        if self.hdr.slice_type == SliceType::B {
            self.cur.bdirect = true;
            self.cur.direct8 = 0xF;
            let d = self.direct()?;
            self.put_direct(&d, 0xF);
        } else {
            // P_Skip motion (8.4.1.1)
            let a = self.nb_motion(0, -1, 0);
            let b = self.nb_motion(0, 0, -1);
            let still = |n: Option<(i8, [i16; 2])>| n.is_some_and(|v| v.0 == 0 && v.1 == [0, 0]);
            let mv = if a.is_none() || b.is_none() || still(a) || still(b) { [0, 0] } else { self.pred_mv(0, 0, 0, 4, 0, 0) };
            self.set_ref(0, 0, 0, 4, 4, 0);
            self.set_motion(0, 0, 0, 4, 4, mv);
        }
        self.resolve_refs()?;
        self.mc_mb()
    }

    fn read_ref_idx(&mut self, l: usize, bx: usize, by: usize, max: u32) -> Result<i8> {
        if max == 0 {
            return Ok(0);
        }
        if matches!(self.rd, Reader::Cabac(_)) {
            return self.cabac_ref_idx(l, bx, by, max);
        }
        let Reader::Cavlc(r) = &mut self.rd else { unreachable!() };
        let v = r.te(max)?;
        if v > max {
            return Err(Error::invalid_input("H.264 reference index is out of range"));
        }
        Ok(v as i8)
    }

    fn read_mvd(&mut self, l: usize, bx: usize, by: usize) -> Result<[i32; 2]> {
        if let Reader::Cavlc(r) = &mut self.rd {
            return Ok([r.se()?, r.se()?]);
        }
        Ok([self.cabac_mvd_comp(l, bx, by, 0)?, self.cabac_mvd_comp(l, bx, by, 1)?])
    }

    fn read_partition_mv(&mut self, l: usize, bx: usize, by: usize, w: usize, h: usize, dir: u8) -> Result<()> {
        let refi = self.cur.ref_idx[l][(by >> 1) * 2 + (bx >> 1)];
        let p = self.pred_mv(l, bx as i32, by as i32, w as i32, dir, refi);
        let d = self.read_mvd(l, bx, by)?;
        let clip = |v: i32| v.unsigned_abs().min(64) as u8;
        for y in by..by + h {
            for x in bx..bx + w {
                self.cur.mvd[l][y * 4 + x] = [clip(d[0]), clip(d[1])];
            }
        }
        let mv = [(p[0] as i32 + d[0]) as i16, (p[1] as i32 + d[1]) as i16];
        self.set_motion(l, bx, by, w, h, mv);
        Ok(())
    }

    fn read_sub_type(&mut self, b: bool) -> Result<u32> {
        let t = if b && matches!(self.rd, Reader::Cabac(_)) {
            self.cabac_b_sub_type()
        } else {
            self.sub_type_basic()?
        };
        if t > if b { 12 } else { 3 } {
            return Err(Error::invalid_input("H.264 sub-macroblock type is out of range"));
        }
        Ok(t)
    }

    fn sub_type_basic(&mut self) -> Result<u32> {
        let t = match &mut self.rd {
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
        Ok(t)
    }

    fn decode_inter_mb(&mut self, t: u32) -> Result<()> {
        self.cur.kind = Kind::Inter;
        let b = self.hdr.slice_type == SliceType::B;
        let maxr = [self.hdr.num_ref_idx_active[0].saturating_sub(1), self.hdr.num_ref_idx_active[1].saturating_sub(1)];
        // (bx, by, w, h, list mask) where bit 0 is list 0 and bit 1 list 1
        let mut ref_parts: Vec<(usize, usize, usize, usize, u8)> = Vec::new();
        // (bx, by, w, h, list mask, prediction direction, direct)
        let mut mv_parts: Vec<(usize, usize, usize, usize, u8, u8, bool)> = Vec::new();
        let mut direct_mask = 0u8;
        let mut sub8_ok = true;
        let mut ref0 = false;
        if !b {
            match t {
                0 => {
                    ref_parts.push((0, 0, 4, 4, 1));
                    mv_parts.push((0, 0, 4, 4, 1, 0, false));
                }
                1 => {
                    ref_parts.extend([(0, 0, 4, 2, 1), (0, 2, 4, 2, 1)]);
                    mv_parts.extend([(0, 0, 4, 2, 1, 1, false), (0, 2, 4, 2, 1, 1, false)]);
                }
                2 => {
                    ref_parts.extend([(0, 0, 2, 4, 1), (2, 0, 2, 4, 1)]);
                    mv_parts.extend([(0, 0, 2, 4, 1, 2, false), (2, 0, 2, 4, 1, 2, false)]);
                }
                _ => {
                    ref0 = t == 4;
                    let mut sub = [0u32; 4];
                    for s in sub.iter_mut() {
                        *s = self.read_sub_type(false)?;
                        sub8_ok &= *s == 0;
                    }
                    for (i, &s) in sub.iter().enumerate() {
                        let (x0, y0) = ((i & 1) * 2, (i >> 1) * 2);
                        ref_parts.push((x0, y0, 2, 2, 1));
                        sub_parts(&mut mv_parts, x0, y0, 1, s as usize);
                    }
                }
            }
        } else if t == 0 {
            direct_mask = 0xF;
            self.cur.bdirect = true;
        } else if t <= 3 {
            ref_parts.push((0, 0, 4, 4, t as u8));
            mv_parts.push((0, 0, 4, 4, t as u8, 0, false));
        } else if t < 22 {
            let idx = t - 4;
            let masks: [(u8, u8); 9] = [(1, 1), (2, 2), (1, 2), (2, 1), (1, 3), (2, 3), (3, 1), (3, 2), (3, 3)];
            let (m0, m1) = masks[(idx / 2) as usize];
            if idx.is_multiple_of(2) {
                ref_parts.extend([(0, 0, 4, 2, m0), (0, 2, 4, 2, m1)]);
                mv_parts.extend([(0, 0, 4, 2, m0, 1, false), (0, 2, 4, 2, m1, 1, false)]);
            } else {
                ref_parts.extend([(0, 0, 2, 4, m0), (2, 0, 2, 4, m1)]);
                mv_parts.extend([(0, 0, 2, 4, m0, 2, false), (2, 0, 2, 4, m1, 2, false)]);
            }
        } else if t == 22 {
            let mut sub = [0u32; 4];
            for s in sub.iter_mut() {
                *s = self.read_sub_type(true)?;
            }
            for (i, &s) in sub.iter().enumerate() {
                let (x0, y0) = ((i & 1) * 2, (i >> 1) * 2);
                if s == 0 {
                    direct_mask |= 1 << i;
                    mv_parts.push((x0, y0, 2, 2, 0, 0, true));
                    continue;
                }
                // (list mask, partition shape: 0 = 8x8, 1 = 8x4, 2 = 4x8, 3 = 4x4)
                let (mask, shape) = match s {
                    1..=3 => (s as u8, 0),
                    4 => (1, 1),
                    5 => (1, 2),
                    6 => (2, 1),
                    7 => (2, 2),
                    8 => (3, 1),
                    9 => (3, 2),
                    10 => (1, 3),
                    11 => (2, 3),
                    _ => (3, 3),
                };
                sub8_ok &= shape == 0;
                ref_parts.push((x0, y0, 2, 2, mask));
                sub_parts(&mut mv_parts, x0, y0, mask, shape);
            }
        } else {
            return Err(Error::invalid_input("H.264 B macroblock type is out of range"));
        }
        // direct motion first: later partitions may predict from the earlier direct ones
        self.cur.direct8 = direct_mask;
        if direct_mask != 0 {
            let d = self.direct()?;
            self.put_direct(&d, direct_mask);
        }
        for l in 0..2usize {
            for &(bx, by, w, h, mask) in &ref_parts {
                if mask & (1 << l) == 0 {
                    continue;
                }
                let r = if ref0 { 0 } else { self.read_ref_idx(l, bx, by, maxr[l])? };
                self.set_ref(l, bx, by, w, h, r);
            }
        }
        for l in 0..2usize {
            self.done[l] = 0;
            for &(bx, by, w, h, mask, dir, direct) in &mv_parts {
                if direct {
                    self.mark_done(l, bx, by, w, h);
                } else if mask & (1 << l) != 0 {
                    self.read_partition_mv(l, bx, by, w, h, dir)?;
                } else {
                    self.set_motion(l, bx, by, w, h, [0, 0]);
                }
            }
        }
        self.resolve_refs()?;
        let cbp = if let Reader::Cavlc(r) = &mut self.rd {
            let k = r.ue()?;
            let gray = matches!(self.pic.cf, 0 | 3);
            if k > if gray { 15 } else { 47 } {
                return Err(Error::invalid_input("H.264 coded_block_pattern code is out of range"));
            }
            (if gray { GOLOMB_TO_INTER_CBP_GRAY[k as usize] } else { GOLOMB_TO_INTER_CBP[k as usize] }) as u32
        } else {
            self.read_cbp()?
        };
        self.cur.cbp = cbp as u8;
        let direct_ok = direct_mask == 0 || self.sps.direct_8x8_inference;
        if cbp & 15 != 0 && self.pps.transform_8x8 && sub8_ok && direct_ok {
            self.cur.t8x8 = self.read_t8x8_flag()?;
        }
        let mut res = Res::new();
        if cbp > 0 {
            let dqp = self.read_qp_delta()?;
            self.qp = (self.qp + dqp + 52 + 2 * self.qp_off).rem_euclid(52 + self.qp_off) - self.qp_off;
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
            (0..2).all(|l| {
                let first = this.cur.mv[l][by * 4 + bx];
                (0..n).all(|dy| (0..n).all(|dx| this.cur.mv[l][(by + dy) * 4 + bx + dx] == first))
            })
        };
        let same_ref = (0..2).all(|l| self.cur.ref_idx[l].iter().all(|&r| r == self.cur.ref_idx[l][0]));
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

    /// Predicts a block of `w` x `h` 4x4 units at (bx, by) from one or both lists.
    fn mc_block(&mut self, bx: usize, by: usize, w: usize, h: usize) -> Result<()> {
        let b8 = (by >> 1) * 2 + (bx >> 1);
        let (pw, ph) = (self.pic.mb_w * 16, self.pic.mb_h * 16);
        let (x, y) = ((self.mbx * 16 + bx * 4) as i32, (self.mby * 16 + by * 4) as i32);
        let (bw, bh) = (w * 4, h * 4);
        let c422 = self.pic.cf == 2;
        let c444 = self.pic.cf == 3;
        let (cw, ch) = (if c444 { bw } else { bw / 2 }, if c422 || c444 { bh } else { bh / 2 });
        let (cpw, cph) = (if c444 { pw } else { pw / 2 }, if c422 || c444 { ph } else { ph / 2 });
        let (cx, cy) = (if c444 { x } else { x / 2 }, if c422 || c444 { y } else { y / 2 });
        let mut luma = [[0u16; 256]; 2];
        let mut chroma = [[[0u16; 256]; 2]; 2];
        let mut used = [false; 2];
        let mut pics: [Option<(std::sync::Arc<super::dpb::DecodedPic>, bool)>; 2] = [None, None];
        for l in 0..2usize {
            let r = self.cur.ref_idx[l][b8];
            if r < 0 {
                continue;
            }
            let entry = match self.refs[l].get(r as usize) {
                Some(Some(e)) => e.clone(),
                _ => return Err(Error::invalid_input("H.264 macroblock refers to a reference picture that is not available")),
            };
            let mv = self.cur.mv[l][by * 4 + bx];
            inter::luma_mc(&entry.pic.planes[0], pw, ph, x, y, mv[0] as i32, mv[1] as i32, bw, bh, &mut luma[l], self.maxv);
            if c444 {
                // 4:4:4 chroma is interpolated exactly like luma
                for c in 0..2usize {
                    inter::luma_mc(&entry.pic.planes[1 + c], cpw, cph, cx, cy, mv[0] as i32, mv[1] as i32, cw, ch, &mut chroma[l][c], self.maxv);
                }
            } else if self.pic.cf != 0 {
                // 4:2:2 chroma has full vertical resolution, so its vertical vector is in quarter samples
                let my = if c422 { mv[1] as i32 * 2 } else { mv[1] as i32 };
                for c in 0..2usize {
                    inter::chroma_mc(&entry.pic.planes[1 + c], cpw, cph, x / 2, cy, mv[0] as i32, my, cw, ch, &mut chroma[l][c]);
                }
            }
            used[l] = true;
            pics[l] = Some((entry.pic.clone(), entry.long));
        }
        if !used[0] && !used[1] {
            return Err(Error::invalid_input("H.264 inter macroblock uses no reference picture"));
        }
        let (r0, r1) = (self.cur.ref_idx[0][b8].max(0) as usize, self.cur.ref_idx[1][b8].max(0) as usize);
        let explicit = self.hdr.weights.as_ref();
        if used[0] && used[1] {
            // weights as (log2 denominator, w0, w1, offset) for luma and each chroma plane
            let mut wts: Option<[(u32, i32, i32, i32); 3]> = None;
            if let Some(wt) = explicit {
                let (e0, e1) = (wt.entries[0].get(r0).copied(), wt.entries[1].get(r1).copied());
                if let (Some(e0), Some(e1)) = (e0, e1) {
                    wts = Some([
                        (wt.luma_log2_denom, e0[0], e1[0], ((e0[1] + e1[1] + 1) >> 1) << (self.bd - 8)),
                        (wt.chroma_log2_denom, e0[2], e1[2], ((e0[3] + e1[3] + 1) >> 1) << (self.bd - 8)),
                        (wt.chroma_log2_denom, e0[4], e1[4], ((e0[5] + e1[5] + 1) >> 1) << (self.bd - 8)),
                    ]);
                }
            } else if self.pps.weighted_bipred_idc == 2 {
                let (p0, l0) = pics[0].as_ref().map(|p| (p.0.poc, p.1)).unwrap();
                let (p1, l1) = pics[1].as_ref().map(|p| (p.0.poc, p.1)).unwrap();
                let (mut w0, mut w1) = (32, 32);
                let tb = (self.cur_poc - p0).clamp(-128, 127);
                let td = (p1 - p0).clamp(-128, 127);
                if td != 0 && !l0 && !l1 {
                    let tx = (16384 + (td / 2).abs()) / td;
                    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                    if (-64..=128).contains(&(dsf >> 2)) {
                        w1 = dsf >> 2;
                        w0 = 64 - w1;
                    }
                }
                wts = Some([(5, w0, w1, 0); 3]);
            }
            let (l0, l1) = luma.split_at_mut(1);
            let (a, bb) = (&mut l0[0][..bw * bh], &l1[0][..bw * bh]);
            match wts {
                Some(wt) => inter::weight_bi(a, bb, wt[0].0, wt[0].1, wt[0].2, wt[0].3, self.maxv),
                None => inter::average(a, bb),
            }
            for c in 0..if self.pic.cf != 0 { 2usize } else { 0 } {
                let (c0, c1) = chroma.split_at_mut(1);
                let (a, bb) = (&mut c0[0][c][..cw * ch], &c1[0][c][..cw * ch]);
                match wts {
                    Some(wt) => inter::weight_bi(a, bb, wt[1 + c].0, wt[1 + c].1, wt[1 + c].2, wt[1 + c].3, self.maxv),
                    None => inter::average(a, bb),
                }
            }
        } else {
            let l = if used[0] { 0 } else { 1 };
            let r = if l == 0 { r0 } else { r1 };
            if let Some(wt) = explicit {
                if let Some(e) = wt.entries[l].get(r) {
                    inter::weight_uni(&mut luma[l][..bw * bh], wt.luma_log2_denom, e[0], e[1] << (self.bd - 8), self.maxv);
                    for c in 0..if self.pic.cf != 0 { 2usize } else { 0 } {
                        inter::weight_uni(&mut chroma[l][c][..cw * ch], wt.chroma_log2_denom, e[2 + 2 * c], e[3 + 2 * c] << (self.bd - 8), self.maxv);
                    }
                }
            }
            if l == 1 {
                luma[0] = luma[1];
                chroma[0] = chroma[1];
            }
        }
        for row in 0..bh {
            let o = (y as usize + row) * pw + x as usize;
            self.pic.planes[0][o..o + bw].copy_from_slice(&luma[0][row * bw..row * bw + bw]);
        }
        for c in 0..if self.pic.cf != 0 { 2usize } else { 0 } {
            for row in 0..ch {
                let o = (cy as usize + row) * cpw + cx as usize;
                self.pic.planes[1 + c][o..o + cw].copy_from_slice(&chroma[0][c][row * cw..row * cw + cw]);
            }
        }
        Ok(())
    }

    /// Adds the decoded residual of an inter macroblock to its prediction.
    fn add_inter_residual(&mut self, res: &Res) -> Result<()> {
        for c in 0..self.ncomp() {
            self.add_inter_residual_comp(res, c);
        }
        self.reconstruct_chroma(res)
    }

    fn add_inter_residual_comp(&mut self, res: &Res, c: usize) {
        if self.bypass() {
            self.add_inter_residual_bypass(res, c);
            return;
        }
        let qp = self.comp_qp(c);
        let ls = self.stride(c);
        let (x0, y0) = (self.mbx * 16, self.mby * 16);
        let cbp = self.cur.cbp as u32;
        if self.cur.t8x8 {
            for i8 in 0..4usize {
                if cbp & (1 << i8) == 0 {
                    continue;
                }
                let mut co = [0i32; 64];
                let lsv = &self.scale.l8[2 * c + 1][(qp % 6) as usize];
                for k in 0..64 {
                    let v = res.luma[c][i8 * 64 + k];
                    if v != 0 {
                        co[k] = if qp >= 36 { v.wrapping_mul(lsv[k]) << (qp / 6 - 6) } else { (v.wrapping_mul(lsv[k]) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                    }
                }
                if co.iter().any(|&v| v != 0) {
                    let (x, y) = (x0 + (i8 & 1) * 8, y0 + (i8 >> 1) * 8);
                    idct8_add(&co, &mut self.pic.planes[c][y * ls + x..], ls, self.maxv);
                }
            }
        } else {
            for b in 0..16usize {
                if cbp & (1 << ((b / 8) * 2 + (b % 4) / 2)) == 0 {
                    continue;
                }
                let co = self.scale4(&res.luma[c][b * 16..b * 16 + 16], 3 + c, qp, false);
                if co.iter().any(|&v| v != 0) {
                    let (bx, by) = (b % 4, b / 4);
                    idct4_add(&co, &mut self.pic.planes[c][(y0 + by * 4) * ls + x0 + bx * 4..], ls, self.maxv);
                }
            }
        }
    }

    // ---- prediction modes ------------------------------------------------------------------------

    fn assign_pred_modes(&mut self, syn: &[(bool, u8); 16]) -> Result<()> {
        let (la, ta) = (self.avail_left_i(), self.avail_top_i());
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
    fn neighbour_nnz_luma(&self, c: usize, bx: usize, by: usize) -> (Option<u8>, Option<u8>) {
        let o = c * 16;
        let a = if bx > 0 {
            Some(self.cur.nnz[o + by * 4 + bx - 1])
        } else if self.avail_left() {
            Some(self.left().nnz[o + by * 4 + 3])
        } else {
            None
        };
        let b = if by > 0 {
            Some(self.cur.nnz[o + (by - 1) * 4 + bx])
        } else if self.avail_top() {
            Some(self.top().nnz[o + 12 + bx])
        } else {
            None
        };
        (a, b)
    }

    fn neighbour_nnz_chroma(&self, comp: usize, bx: usize, by: usize) -> (Option<u8>, Option<u8>) {
        let base = 16 + 8 * comp;
        let last_row = self.pic.ch / 4 - 1;
        let a = if bx > 0 {
            Some(self.cur.nnz[base + by * 2])
        } else if self.avail_left() {
            Some(self.left().nnz[base + by * 2 + 1])
        } else {
            None
        };
        let b = if by > 0 {
            Some(self.cur.nnz[base + (by - 1) * 2 + bx])
        } else if self.avail_top() {
            Some(self.top().nnz[base + last_row * 2 + bx])
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

    /// Residual of a component coded like luma: luma itself, or Cb or Cr in 4:4:4.
    fn read_residual_luma(&mut self, res: &mut Res, cbp: u32, i16: bool, c: usize) -> Result<()> {
        let t8 = self.cur.t8x8;
        if i16 {
            // Intra16x16DCLevel
            let mut lv = [0i32; 16];
            let n = self.block(c, 0, 0, 16, &mut lv, BlockKind::LumaDc)?;
            if n > 0 {
                self.cur.cbf_dc |= 1 << c;
            }
            for (k, &p) in ZIGZAG4.iter().enumerate() {
                res.dc[c][p as usize] = lv[k];
            }
        }
        for i8 in 0..4usize {
            if cbp & (1 << i8) == 0 {
                continue;
            }
            if t8 {
                self.read_luma8x8(res, i8, c)?;
                continue;
            }
            for i4 in 0..4usize {
                let (bx, by) = ((i8 & 1) * 2 + (i4 & 1), (i8 >> 1) * 2 + (i4 >> 1));
                let mut lv = [0i32; 16];
                let (kind, max) = if i16 { (BlockKind::LumaAc, 15) } else { (BlockKind::Luma4x4, 16) };
                let n = self.block(c, bx, by, max, &mut lv, kind)?;
                self.cur.nnz[c * 16 + by * 4 + bx] = n as u8;
                let start = if i16 { 1 } else { 0 };
                for k in 0..max {
                    res.luma[c][(by * 4 + bx) * 16 + ZIGZAG4[k + start] as usize] = lv[k];
                }
            }
        }
        Ok(())
    }

    fn read_residual(&mut self, res: &mut Res, cbp: u32, i16: bool) -> Result<()> {
        for c in 0..self.ncomp() {
            self.read_residual_luma(res, cbp, i16, c)?;
        }
        let chroma = if matches!(self.pic.cf, 1 | 2) { cbp >> 4 } else { 0 };
        let nblk = self.pic.ch / 2;
        if chroma > 0 {
            for comp in 0..2usize {
                let mut lv = [0i32; 16];
                let n = self.block(comp + 1, 0, 0, nblk, &mut lv, BlockKind::ChromaDc)?;
                if n > 0 {
                    self.cur.cbf_dc |= 2 << comp;
                }
                res.cdc[comp][..nblk].copy_from_slice(&lv[..nblk]);
            }
        }
        if chroma == 2 {
            for comp in 0..2usize {
                for blk in 0..nblk {
                    let (bx, by) = (blk & 1, blk >> 1);
                    let mut lv = [0i32; 16];
                    let n = self.block(comp + 1, bx, by, 15, &mut lv, BlockKind::ChromaAc)?;
                    self.cur.nnz[16 + 8 * comp + by * 2 + bx] = n as u8;
                    for k in 0..15 {
                        res.cac[comp * 8 + blk][ZIGZAG4[k + 1] as usize] = lv[k];
                    }
                }
            }
        }
        Ok(())
    }

    fn read_luma8x8(&mut self, res: &mut Res, i8: usize, c: usize) -> Result<()> {
        let (bx0, by0) = ((i8 & 1) * 2, (i8 >> 1) * 2);
        let cavlc_mode = matches!(self.rd, Reader::Cavlc(_));
        match cavlc_mode {
            true => {
                for j in 0..4usize {
                    let (bx, by) = (bx0 + (j & 1), by0 + (j >> 1));
                    let mut lv = [0i32; 16];
                    let n = self.block(c, bx, by, 16, &mut lv, BlockKind::Luma4x4)?;
                    self.cur.nnz[c * 16 + by * 4 + bx] = n as u8;
                    for k in 0..16 {
                        res.luma[c][i8 * 64 + ZIGZAG_DIRECT[4 * k + j] as usize] = lv[k];
                    }
                }
            }
            false => {
                let mut lv = [0i32; 64];
                let n = self.cabac_block(c, bx0, by0, 64, BlockKind::Luma8x8, &mut lv)?;
                for (k, &v) in lv.iter().enumerate() {
                    res.luma[c][i8 * 64 + ZIGZAG_DIRECT[k] as usize] = v;
                }
                for j in 0..4usize {
                    self.cur.nnz[c * 16 + (by0 + (j >> 1)) * 4 + bx0 + (j & 1)] = n.min(255) as u8;
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
                    BlockKind::ChromaDc => {
                        if self.pic.cf == 2 {
                            -2
                        } else {
                            -1
                        }
                    }
                    BlockKind::ChromaAc => {
                        let (a, b) = self.neighbour_nnz_chroma(comp - 1, bx, by);
                        Self::nc_from(a, b)
                    }
                    _ => {
                        let (a, b) = self.neighbour_nnz_luma(comp, bx, by);
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
        let cc = comp.min(2);
        let cat = match kind {
            BlockKind::LumaDc => [0, 6, 10][cc],
            BlockKind::LumaAc => [1, 7, 11][cc],
            BlockKind::Luma4x4 => [2, 8, 12][cc],
            BlockKind::ChromaDc => 3,
            BlockKind::ChromaAc => 4,
            BlockKind::Luma8x8 => [5, 9, 13][cc],
        };
        let is8 = matches!(cat, 5 | 9 | 13);
        // 8x8 blocks carry a coded_block_flag only in 4:4:4
        if !is8 || self.pic.cf == 3 {
            // coded_block_flag; an unavailable neighbour counts as coded in an intra macroblock
            let un = self.cur.kind != Kind::Inter;
            let (a, b) = match kind {
                BlockKind::LumaDc => {
                    let bit = 1u8 << comp;
                    let f = |avail: bool, m: &MbInfo| if !avail { un } else { m.kind == Kind::IPcm || m.cbf_dc & bit != 0 };
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
                BlockKind::Luma8x8 => {
                    // a neighbouring macroblock only counts if it also uses the 8x8 transform (or is PCM)
                    let (a, b) = self.neighbour_nnz_luma(comp, bx, by);
                    let other = |m: &MbInfo| !(m.t8x8 || m.kind == Kind::IPcm);
                    let skip_a = bx == 0 && a.is_some() && other(self.left());
                    let skip_b = by == 0 && b.is_some() && other(self.top());
                    (a.map_or(un, |v| v > 0 && !skip_a), b.map_or(un, |v| v > 0 && !skip_b))
                }
                _ => {
                    let (a, b) = self.neighbour_nnz_luma(comp, bx, by);
                    (a.map_or(un, |v| v > 0), b.map_or(un, |v| v > 0))
                }
            };
            let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
            let base = match cat {
                0..=4 => 85 + [0usize, 4, 8, 12, 16][cat],
                5 => 1012,
                6..=8 => 460 + 4 * (cat - 6),
                9 => 1016,
                10..=12 => 472 + 4 * (cat - 10),
                _ => 1020,
            };
            if c.decision(base + a as usize + 2 * b as usize) == 0 {
                return Ok(0);
            }
        }
        let Reader::Cabac(c) = &mut self.rd else { unreachable!() };
        let mut sig = [0usize; 64];
        let mut n = 0;
        let last_idx = max - 1;
        let mut i = 0;
        while i < last_idx {
            let (sctx, lctx) = if is8 {
                (SIG_OFF[0][cat] as usize + SIG_OFF_8X8[0][i] as usize, LAST_OFF[0][cat] as usize + LAST_OFF_8X8[i] as usize)
            } else {
                let inc = if cat == 3 { (i / (self.pic.ch / 8)).min(2) } else { i };
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

    fn stride(&self, plane: usize) -> usize {
        if plane == 0 {
            self.pic.mb_w * 16
        } else {
            self.pic.mb_w * self.pic.cw
        }
    }

    /// Colour components coded like luma: three for 4:4:4, otherwise one.
    fn ncomp(&self) -> usize {
        if self.pic.cf == 3 {
            3
        } else {
            1
        }
    }

    /// QP' of luma-style component `c` (chroma QP for Cb and Cr).
    fn comp_qp(&self, c: usize) -> i32 {
        if c == 0 {
            self.qp + self.qp_off
        } else {
            chroma_qp(self.qp, self.pps.chroma_qp_offset[c - 1], self.bd) + self.qp_off
        }
    }

    fn pix(&self, plane: usize, x: usize, y: usize) -> i32 {
        let s = self.stride(plane);
        self.pic.planes[plane][y * s + x] as i32
    }

    fn reconstruct(&mut self, res: &Res, i16_mode: u8, _cbp: u32) -> Result<()> {
        for c in 0..self.ncomp() {
            self.reconstruct_comp(res, i16_mode, c)?;
        }
        self.reconstruct_chroma(res)
    }

    /// Lossless coding: qpprime_y_zero_transform_bypass_flag with QP'Y equal to 0.
    fn bypass(&self) -> bool {
        self.sps.transform_bypass && self.qp + self.qp_off == 0
    }

    /// `reconstruct_comp` for a lossless macroblock: levels are residual samples, with DPCM along
    /// horizontal and vertical intra prediction.
    fn reconstruct_comp_bypass(&mut self, res: &Res, i16_mode: u8, c: usize) -> Result<()> {
        let (mbx, mby) = (self.mbx, self.mby);
        let (la, ta, tla) = (self.avail_left_i(), self.avail_top_i(), self.avail_topleft_i());
        let ls = self.stride(c);
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
                        *t = self.pix(c, x0 + i, y0 - 1);
                    }
                }
                if la {
                    for (j, l) in left.iter_mut().enumerate() {
                        *l = self.pix(c, x0 - 1, y0 + j);
                    }
                }
                let tl = if tla { self.pix(c, x0 - 1, y0 - 1) } else { 0 };
                let mut pred = [0u16; 256];
                predict_16x16(i16_mode, &top, &left, tl, ta, la, &mut pred, self.bd);
                for y in 0..16 {
                    self.pic.planes[c][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
                }
                let mut r = [0i32; 256];
                for b in 0..16 {
                    let (bx, by) = (b % 4, b / 4);
                    for k in 0..16 {
                        r[(by * 4 + k / 4) * 16 + bx * 4 + k % 4] = if k == 0 { res.dc[c][b] } else { res.luma[c][b * 16 + k] };
                    }
                }
                if i16_mode < 2 {
                    dpcm(&mut r, 16, 16, i16_mode == 1);
                }
                add_residual(&mut self.pic.planes[c][y0 * ls + x0..], ls, 16, 16, &r, self.maxv);
            }
            Kind::I4x4 => {
                for n in 0..16 {
                    let (bx, by) = block_pos(n);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let e = self.edges_4x4(c, bx, by);
                    check_mode(mode, &e)?;
                    let mut pred = [0u16; 16];
                    predict_nxn(4, mode, &e, &mut pred, self.bd);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..4 {
                        self.pic.planes[c][(y + r) * ls + x..(y + r) * ls + x + 4].copy_from_slice(&pred[r * 4..r * 4 + 4]);
                    }
                    let mut r = [0i32; 16];
                    r.copy_from_slice(&res.luma[c][(by * 4 + bx) * 16..(by * 4 + bx) * 16 + 16]);
                    if mode < 2 {
                        dpcm(&mut r, 4, 4, mode == H_PRED);
                    }
                    add_residual(&mut self.pic.planes[c][y * ls + x..], ls, 4, 4, &r, self.maxv);
                }
            }
            Kind::I8x8 => {
                for i8 in 0..4 {
                    let (bx, by) = ((i8 & 1) * 2, (i8 >> 1) * 2);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let mut e = self.edges_8x8(c, i8);
                    check_mode(mode, &e)?;
                    filter_8x8_edges(&mut e);
                    let mut pred = [0u16; 64];
                    predict_nxn(8, mode, &e, &mut pred, self.bd);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..8 {
                        self.pic.planes[c][(y + r) * ls + x..(y + r) * ls + x + 8].copy_from_slice(&pred[r * 8..r * 8 + 8]);
                    }
                    let mut r = [0i32; 64];
                    r.copy_from_slice(&res.luma[c][i8 * 64..i8 * 64 + 64]);
                    if mode < 2 {
                        dpcm(&mut r, 8, 8, mode == H_PRED);
                    }
                    add_residual(&mut self.pic.planes[c][y * ls + x..], ls, 8, 8, &r, self.maxv);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Residual of an inter macroblock in lossless coding.
    fn add_inter_residual_bypass(&mut self, res: &Res, c: usize) {
        let ls = self.stride(c);
        let (x0, y0) = (self.mbx * 16, self.mby * 16);
        let cbp = self.cur.cbp as u32;
        if self.cur.t8x8 {
            for i8 in 0..4usize {
                if cbp & (1 << i8) != 0 {
                    let (x, y) = (x0 + (i8 & 1) * 8, y0 + (i8 >> 1) * 8);
                    add_residual(&mut self.pic.planes[c][y * ls + x..], ls, 8, 8, &res.luma[c][i8 * 64..i8 * 64 + 64], self.maxv);
                }
            }
        } else {
            for b in 0..16usize {
                if cbp & (1 << ((b / 8) * 2 + (b % 4) / 2)) != 0 {
                    let (x, y) = (x0 + (b % 4) * 4, y0 + (b / 4) * 4);
                    add_residual(&mut self.pic.planes[c][y * ls + x..], ls, 4, 4, &res.luma[c][b * 16..b * 16 + 16], self.maxv);
                }
            }
        }
    }

    fn reconstruct_comp(&mut self, res: &Res, i16_mode: u8, c: usize) -> Result<()> {
        if self.bypass() {
            return self.reconstruct_comp_bypass(res, i16_mode, c);
        }
        let qp = self.comp_qp(c);
        let (mbx, mby) = (self.mbx, self.mby);
        let (la, ta, tla) = (self.avail_left_i(), self.avail_top_i(), self.avail_topleft_i());
        let ls = self.stride(c);
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
                        *t = self.pix(c, x0 + i, y0 - 1);
                    }
                }
                if la {
                    for (j, l) in left.iter_mut().enumerate() {
                        *l = self.pix(c, x0 - 1, y0 + j);
                    }
                }
                let tl = if tla { self.pix(c, x0 - 1, y0 - 1) } else { 0 };
                let mut pred = [0u16; 256];
                predict_16x16(i16_mode, &top, &left, tl, ta, la, &mut pred, self.bd);
                for y in 0..16 {
                    self.pic.planes[c][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
                }
                // DC: inverse Hadamard, then scale
                let f = hadamard4(&res.dc[c]);
                let lsd = self.scale.l4[c][(qp % 6) as usize][0];
                let mut dcy = [0i32; 16];
                for k in 0..16 {
                    dcy[k] = if qp >= 36 { f[k].wrapping_mul(lsd) << (qp / 6 - 6) } else { (f[k].wrapping_mul(lsd) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                }
                for b in 0..16 {
                    let mut co = self.scale4(&res.luma[c][b * 16..b * 16 + 16], c, qp, true);
                    co[0] = dcy[b];
                    if co.iter().any(|&v| v != 0) {
                        let (bx, by) = (b % 4, b / 4);
                        idct4_add(&co, &mut self.pic.planes[c][(y0 + by * 4) * ls + x0 + bx * 4..], ls, self.maxv);
                    }
                }
            }
            Kind::I4x4 => {
                for n in 0..16 {
                    let (bx, by) = block_pos(n);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let e = self.edges_4x4(c, bx, by);
                    check_mode(mode, &e)?;
                    let mut pred = [0u16; 16];
                    predict_nxn(4, mode, &e, &mut pred, self.bd);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..4 {
                        self.pic.planes[c][(y + r) * ls + x..(y + r) * ls + x + 4].copy_from_slice(&pred[r * 4..r * 4 + 4]);
                    }
                    let co = self.scale4(&res.luma[c][(by * 4 + bx) * 16..(by * 4 + bx) * 16 + 16], c, qp, false);
                    if co.iter().any(|&v| v != 0) {
                        idct4_add(&co, &mut self.pic.planes[c][y * ls + x..], ls, self.maxv);
                    }
                }
            }
            Kind::I8x8 => {
                for i8 in 0..4 {
                    let (bx, by) = ((i8 & 1) * 2, (i8 >> 1) * 2);
                    let mode = self.cur.ipm[by * 4 + bx];
                    let mut e = self.edges_8x8(c, i8);
                    check_mode(mode, &e)?;
                    filter_8x8_edges(&mut e);
                    let mut pred = [0u16; 64];
                    predict_nxn(8, mode, &e, &mut pred, self.bd);
                    let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
                    for r in 0..8 {
                        self.pic.planes[c][(y + r) * ls + x..(y + r) * ls + x + 8].copy_from_slice(&pred[r * 8..r * 8 + 8]);
                    }
                    let mut co = [0i32; 64];
                    let lsv = &self.scale.l8[2 * c][(qp % 6) as usize];
                    for k in 0..64 {
                        let v = res.luma[c][i8 * 64 + k];
                        if v != 0 {
                            co[k] = if qp >= 36 { v.wrapping_mul(lsv[k]) << (qp / 6 - 6) } else { (v.wrapping_mul(lsv[k]) + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
                        }
                    }
                    if co.iter().any(|&v| v != 0) {
                        idct8_add(&co, &mut self.pic.planes[c][y * ls + x..], ls, self.maxv);
                    }
                }
            }
            _ => {}
        }
        Ok(())
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
        if matches!(self.pic.cf, 0 | 3) {
            return Ok(());
        }
        let (mbx, mby) = (self.mbx, self.mby);
        let (la, ta, tla) = (self.avail_left_i(), self.avail_top_i(), self.avail_topleft_i());
        let mode = self.cur.chroma_mode;
        let inter = self.cur.kind == Kind::Inter;
        if !inter && ((mode == 1 && !la) || (mode == 2 && !ta) || (mode == 3 && !(la && ta && tla))) {
            return Err(Error::invalid_input("H.264 chroma prediction uses neighbours that are not available"));
        }
        let cs = self.pic.mb_w * 8;
        let chh = self.pic.ch;
        let (x0, y0) = (mbx * 8, mby * chh);
        let chroma_cbp = self.cur.cbp >> 4;
        for comp in 0..2usize {
            let plane = 1 + comp;
            if !inter {
                let mut top = [0i32; 8];
                let mut left = [0i32; 16];
                if ta {
                    for (i, t) in top.iter_mut().enumerate() {
                        *t = self.pix(plane, x0 + i, y0 - 1);
                    }
                }
                if la {
                    for (j, l) in left.iter_mut().enumerate().take(chh) {
                        *l = self.pix(plane, x0 - 1, y0 + j);
                    }
                }
                let tl = if tla { self.pix(plane, x0 - 1, y0 - 1) } else { 0 };
                let mut pred = [0u16; 128];
                predict_chroma(mode, &top, &left, tl, ta, la, &mut pred, self.bd, chh);
                for y in 0..chh {
                    self.pic.planes[plane][(y0 + y) * cs + x0..(y0 + y) * cs + x0 + 8].copy_from_slice(&pred[y * 8..y * 8 + 8]);
                }
            }
            let list = if inter { 4 + comp } else { 1 + comp };
            if chroma_cbp == 0 {
                continue;
            }
            if self.bypass() {
                // levels are residual samples; the DC levels sit at the first position of each block
                let c = &res.cdc[comp];
                let mut dc = [0i32; 8];
                if chh == 8 {
                    dc[..4].copy_from_slice(&c[..4]);
                } else {
                    let m = [[c[0], c[2]], [c[1], c[5]], [c[3], c[6]], [c[4], c[7]]];
                    for r in 0..4 {
                        dc[2 * r] = m[r][0];
                        dc[2 * r + 1] = m[r][1];
                    }
                }
                let mut r = [0i32; 128];
                for blk in 0..chh / 2 {
                    let (bx, by) = (blk & 1, blk >> 1);
                    for k in 0..16 {
                        r[(by * 4 + k / 4) * 8 + bx * 4 + k % 4] = if k == 0 { dc[blk] } else { res.cac[comp * 8 + blk][k] };
                    }
                }
                if !inter && (mode == 1 || mode == 2) {
                    dpcm(&mut r, 8, chh, mode == 1);
                }
                add_residual(&mut self.pic.planes[plane][y0 * cs + x0..], cs, 8, chh, &r, self.maxv);
                continue;
            }
            let qpc = chroma_qp(self.qp, self.pps.chroma_qp_offset[comp], self.bd) + self.qp_off;
            let c = &res.cdc[comp];
            let mut dc = [0i32; 8];
            if chh == 8 {
                // 2x2 transform, then scale
                let f = [c[0] + c[1] + c[2] + c[3], c[0] - c[1] + c[2] - c[3], c[0] + c[1] - c[2] - c[3], c[0] - c[1] - c[2] + c[3]];
                let lsd = self.scale.l4[list][(qpc % 6) as usize][0];
                for k in 0..4 {
                    dc[k] = (f[k].wrapping_mul(lsd) << (qpc / 6)) >> 5;
                }
            } else {
                // 2x4 transform of the DC levels, which are coded in a fixed non-raster order
                let m = [[c[0], c[2]], [c[1], c[5]], [c[3], c[6]], [c[4], c[7]]];
                const A: [[i32; 4]; 4] = [[1, 1, 1, 1], [1, 1, -1, -1], [1, -1, -1, 1], [1, -1, 1, -1]];
                let qdc = qpc + 3;
                let lsd = self.scale.l4[list][(qdc % 6) as usize][0];
                for r in 0..4 {
                    let (mut t0, mut t1) = (0i32, 0i32);
                    for k in 0..4 {
                        t0 += A[r][k] * m[k][0];
                        t1 += A[r][k] * m[k][1];
                    }
                    for (j, f) in [t0 + t1, t0 - t1].into_iter().enumerate() {
                        dc[2 * r + j] = if qdc >= 36 { f.wrapping_mul(lsd) << (qdc / 6 - 6) } else { (f.wrapping_mul(lsd) + (1 << (5 - qdc / 6))) >> (6 - qdc / 6) };
                    }
                }
            }
            for blk in 0..chh / 2 {
                let mut cf = self.scale4(&res.cac[comp * 8 + blk], list, qpc, true);
                cf[0] = dc[blk];
                if cf.iter().any(|&v| v != 0) {
                    let (bx, by) = (blk & 1, blk >> 1);
                    idct4_add(&cf, &mut self.pic.planes[plane][(y0 + by * 4) * cs + x0 + bx * 4..], cs, self.maxv);
                }
            }
        }
        Ok(())
    }

    fn edges_4x4(&self, c: usize, bx: usize, by: usize) -> Edges {
        let (la, ta, tla, tra) = (self.avail_left_i(), self.avail_top_i(), self.avail_topleft_i(), self.avail_topright_i());
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
                e.top[i] = self.pix(c, x0 + i, y0 - 1);
            }
            for i in 4..8 {
                e.top[i] = if have_tr { self.pix(c, x0 + i, y0 - 1) } else { e.top[3] };
            }
        }
        if have_left {
            for j in 0..4 {
                e.left[j] = self.pix(c, x0 - 1, y0 + j);
            }
        }
        if have_tl {
            e.topleft = self.pix(c, x0 - 1, y0 - 1);
        }
        e
    }

    fn edges_8x8(&self, c: usize, i8: usize) -> Edges {
        let (la, ta, tla, tra) = (self.avail_left_i(), self.avail_top_i(), self.avail_topleft_i(), self.avail_topright_i());
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
                e.top[i] = self.pix(c, x0 + i, y0 - 1);
            }
            for i in 8..16 {
                e.top[i] = if have_tr { self.pix(c, x0 + i, y0 - 1) } else { e.top[7] };
            }
        }
        if have_left {
            for j in 0..8 {
                e.left[j] = self.pix(c, x0 - 1, y0 + j);
            }
        }
        if have_tl {
            e.topleft = self.pix(c, x0 - 1, y0 - 1);
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
