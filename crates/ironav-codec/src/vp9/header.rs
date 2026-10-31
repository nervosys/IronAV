//! VP9 uncompressed and compressed frame headers, and the probability contexts they update.

use super::rac::Rac;
use super::tables::*;

pub const TX_4X4: u8 = 0;
pub const TX_SWITCHABLE: u8 = 4;
pub const FILTER_SWITCHABLE: u8 = 4;
pub const PRED_SINGLEREF: u8 = 0;
pub const PRED_COMPREF: u8 = 1;
pub const PRED_SWITCHABLE: u8 = 2;

#[derive(Clone, Copy, Default, Debug)]
pub struct SegFeat {
    pub q_enabled: bool,
    pub q_val: i32,
    pub lf_enabled: bool,
    pub lf_val: i32,
    pub ref_enabled: bool,
    pub ref_val: u8,
    pub skip_enabled: bool,
    /// [luma/chroma][dc/ac]
    pub qmul: [[i32; 2]; 2],
    /// [reference][mode]
    pub lflvl: [[u8; 2]; 4],
}

#[derive(Clone, Default, Debug)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal: bool,
    pub absolute_vals: bool,
    pub prob: [u8; 7],
    pub pred_prob: [u8; 3],
    pub feat: [SegFeat; 8],
}

#[derive(Clone, Copy, Debug)]
pub struct MvCompProbs {
    pub sign: u8,
    pub classes: [u8; 10],
    pub class0: u8,
    pub bits: [u8; 10],
    pub class0_fp: [[u8; 3]; 2],
    pub fp: [u8; 3],
    pub class0_hp: u8,
    pub hp: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Probs {
    pub y_mode: [[u8; 9]; 4],
    pub uv_mode: [[u8; 9]; 10],
    pub filter: [[u8; 2]; 4],
    pub mv_mode: [[u8; 3]; 7],
    pub intra: [u8; 4],
    pub comp: [u8; 5],
    pub single_ref: [[u8; 2]; 5],
    pub comp_ref: [u8; 5],
    pub tx32p: [[u8; 3]; 2],
    pub tx16p: [[u8; 2]; 2],
    pub tx8p: [u8; 2],
    pub skip: [u8; 3],
    pub mv_joint: [u8; 3],
    pub mv_comp: [MvCompProbs; 2],
    pub partition: [[[u8; 3]; 4]; 4],
}

/// Coefficient probabilities as stored between frames (three model probabilities).
pub type CoefCtx = [[[[[[u8; 3]; 6]; 6]; 2]; 2]; 4];
/// Coefficient probabilities expanded with the Pareto tail (eleven per context).
pub type CoefProbs = [[[[[[u8; 11]; 6]; 6]; 2]; 2]; 4];

impl Probs {
    pub fn default_probs() -> Probs {
        let blank = MvCompProbs {
            sign: 0,
            classes: [0; 10],
            class0: 0,
            bits: [0; 10],
            class0_fp: [[0; 3]; 2],
            fp: [0; 3],
            class0_hp: 0,
            hp: 0,
        };
        let mut mv = [blank; 2];
        for i in 0..2 {
            mv[i] = MvCompProbs {
                sign: DEFAULT_MV_SIGN_PROBS[i],
                classes: DEFAULT_MV_CLASSES_PROBS[i],
                class0: DEFAULT_MV_CLASS0_PROBS[i],
                bits: DEFAULT_MV_BITS_PROBS[i],
                class0_fp: DEFAULT_MV_CLASS0_FP_PROBS[i],
                fp: DEFAULT_MV_FP_PROBS[i],
                class0_hp: DEFAULT_MV_CLASS0_HP_PROBS[i],
                hp: DEFAULT_MV_HP_PROBS[i],
            };
        }
        Probs {
            y_mode: DEFAULT_Y_MODE_PROBS,
            uv_mode: DEFAULT_UV_MODE_PROBS,
            filter: DEFAULT_FILTER_PROBS,
            mv_mode: DEFAULT_MV_MODE_PROBS,
            intra: DEFAULT_INTRA_PROBS,
            comp: DEFAULT_COMP_PROBS,
            single_ref: DEFAULT_SINGLE_REF_PROBS,
            comp_ref: DEFAULT_COMP_REF_PROBS,
            tx32p: DEFAULT_TX32P_PROBS,
            tx16p: DEFAULT_TX16P_PROBS,
            tx8p: DEFAULT_TX8P_PROBS,
            skip: DEFAULT_SKIP_PROBS,
            mv_joint: DEFAULT_MV_JOINT_PROBS,
            mv_comp: mv,
            partition: DEFAULT_PARTITION_PROBS,
        }
    }
}

#[derive(Clone)]
pub struct ProbContext {
    pub p: Probs,
    pub coef: CoefCtx,
}

impl ProbContext {
    pub fn default_ctx() -> Self {
        ProbContext { p: Probs::default_probs(), coef: DEFAULT_COEF_PROBS }
    }
}

/// What the header parser needs to know about a reference slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefInfo {
    pub w: usize,
    pub h: usize,
    pub ss_h: bool,
    pub ss_v: bool,
    pub bpp: u32,
}

#[derive(Clone, Debug, Default)]
pub struct FrameHeader {
    pub profile: u8,
    pub show_existing: bool,
    pub show_existing_idx: usize,
    pub keyframe: bool,
    pub invisible: bool,
    pub errorres: bool,
    pub intraonly: bool,
    pub resetctx: u32,
    pub use_last_frame_mvs: bool,
    pub refreshrefmask: u8,
    pub refidx: [usize; 3],
    pub signbias: [bool; 3],
    pub highprecisionmvs: bool,
    pub filtermode: u8,
    pub allowcompinter: bool,
    pub fixcompref: usize,
    pub varcompref: [usize; 2],
    pub refreshctx: bool,
    pub parallelmode: bool,
    pub framectxid: usize,
    pub width: usize,
    pub height: usize,
    pub bpp: u32,
    pub ss_h: bool,
    pub ss_v: bool,
    pub color_space: u8,
    pub full_range: bool,
    pub lf_level: u8,
    pub lf_sharpness: u8,
    pub lf_delta_enabled: bool,
    pub yac_qi: i32,
    pub ydc_qdelta: i32,
    pub uvdc_qdelta: i32,
    pub uvac_qdelta: i32,
    pub lossless: bool,
    pub seg: Segmentation,
    pub log2_tile_cols: u32,
    pub log2_tile_rows: u32,
    pub compressed_size: usize,
    pub uncompressed_size: usize,
    pub txfmmode: u8,
    pub comppredmode: u8,
    /// Reference scaling per active reference: (x scale, y scale) in 14-bit fixed point, 0 = same size.
    pub mvscale: [[i32; 2]; 3],
    pub mvstep: [[i32; 2]; 3],
}

#[derive(Debug)]
pub enum HeaderError {
    Invalid(&'static str),
    Unsupported(&'static str),
}

type R<T> = Result<T, HeaderError>;

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> R<u32> {
        let byte = self.pos >> 3;
        if byte >= self.d.len() {
            return Err(HeaderError::Invalid("truncated header"));
        }
        let b = (self.d[byte] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }
    fn bits(&mut self, n: u32) -> R<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn flag(&mut self) -> R<bool> {
        Ok(self.bit()? != 0)
    }
    fn sbits_inv(&mut self, n: u32) -> R<i32> {
        let v = self.bits(n)? as i32;
        Ok(if self.flag()? { -v } else { v })
    }
}

/// Header state that persists from frame to frame.
pub struct HeaderState {
    pub prob_ctx: [ProbContext; 4],
    pub lf_ref: [i32; 4],
    pub lf_mode: [i32; 2],
    pub feat: [SegFeat; 8],
    pub seg_abs: bool,
    pub last_invisible: bool,
    pub last_size: Option<(usize, usize)>,
    pub bpp: u32,
    pub ss_h: bool,
    pub ss_v: bool,
}

impl HeaderState {
    pub fn new() -> Self {
        HeaderState {
            prob_ctx: [ProbContext::default_ctx(), ProbContext::default_ctx(), ProbContext::default_ctx(), ProbContext::default_ctx()],
            lf_ref: [1, 0, -1, -1],
            lf_mode: [0, 0],
            feat: [SegFeat::default(); 8],
            seg_abs: false,
            last_invisible: false,
            last_size: None,
            bpp: 8,
            ss_h: true,
            ss_v: true,
        }
    }
}

fn colorspace(b: &mut Bits, profile: u8, h: &mut FrameHeader) -> R<()> {
    let bits = if profile <= 1 { 0 } else { 1 + b.bit()? };
    h.bpp = 8 + bits * 2;
    h.color_space = b.bits(3)? as u8;
    if h.color_space == 7 {
        h.ss_h = false;
        h.ss_v = false;
        h.full_range = true;
        if profile & 1 != 0 {
            if b.flag()? {
                return Err(HeaderError::Invalid("reserved bit set in RGB"));
            }
        } else {
            return Err(HeaderError::Invalid("RGB not supported in this profile"));
        }
    } else {
        h.full_range = b.flag()?;
        if profile & 1 != 0 {
            h.ss_h = b.flag()?;
            h.ss_v = b.flag()?;
            if h.ss_h && h.ss_v {
                return Err(HeaderError::Invalid("4:2:0 not allowed in this profile"));
            }
            if b.flag()? {
                return Err(HeaderError::Invalid("reserved bit set"));
            }
        } else {
            h.ss_h = true;
            h.ss_v = true;
        }
    }
    Ok(())
}

fn clip_u(v: i32, bits: u32) -> i32 {
    v.clamp(0, (1 << bits) - 1)
}

/// Parses the uncompressed header. `compressed_size` and `uncompressed_size` of the result locate
/// the compressed header.
pub fn parse_uncompressed(data: &[u8], st: &mut HeaderState, refs: &[Option<RefInfo>; 8]) -> R<FrameHeader> {
    let mut b = Bits { d: data, pos: 0 };
    let mut h = FrameHeader::default();
    if b.bits(2)? != 2 {
        return Err(HeaderError::Invalid("bad frame marker"));
    }
    let mut profile = b.bit()? as u8;
    profile |= (b.bit()? as u8) << 1;
    if profile == 3 {
        profile += b.bit()? as u8;
    }
    if profile > 3 {
        return Err(HeaderError::Unsupported("profile"));
    }
    h.profile = profile;
    if b.flag()? {
        h.show_existing = true;
        h.show_existing_idx = b.bits(3)? as usize;
        return Ok(h);
    }
    h.keyframe = !b.flag()?;
    let last_invisible = st.last_invisible;
    h.invisible = !b.flag()?;
    h.errorres = b.flag()?;
    h.use_last_frame_mvs = !h.errorres && !last_invisible;
    h.bpp = st.bpp;
    h.ss_h = st.ss_h;
    h.ss_v = st.ss_v;
    let (w, ht);
    if h.keyframe {
        if b.bits(24)? != 0x498342 {
            return Err(HeaderError::Invalid("bad sync code"));
        }
        colorspace(&mut b, profile, &mut h)?;
        h.refreshrefmask = 0xff;
        w = b.bits(16)? as usize + 1;
        ht = b.bits(16)? as usize + 1;
        if b.flag()? {
            b.bits(32)?;
        }
    } else {
        h.intraonly = if h.invisible { b.flag()? } else { false };
        h.resetctx = if h.errorres { 0 } else { b.bits(2)? };
        if h.intraonly {
            if b.bits(24)? != 0x498342 {
                return Err(HeaderError::Invalid("bad sync code"));
            }
            if profile >= 1 {
                colorspace(&mut b, profile, &mut h)?;
            } else {
                h.ss_h = true;
                h.ss_v = true;
                h.bpp = 8;
                h.color_space = 1;
            }
            h.refreshrefmask = b.bits(8)? as u8;
            w = b.bits(16)? as usize + 1;
            ht = b.bits(16)? as usize + 1;
            if b.flag()? {
                b.bits(32)?;
            }
        } else {
            h.refreshrefmask = b.bits(8)? as u8;
            for i in 0..3 {
                h.refidx[i] = b.bits(3)? as usize;
                h.signbias[i] = b.flag()? && !h.errorres;
            }
            let mut ri = [RefInfo { w: 0, h: 0, ss_h: false, ss_v: false, bpp: 0 }; 3];
            for i in 0..3 {
                ri[i] = refs[h.refidx[i]].ok_or(HeaderError::Invalid("missing reference"))?;
            }
            let (mut ww, mut hh) = (0, 0);
            let mut found = false;
            for r in &ri {
                if b.flag()? {
                    ww = r.w;
                    hh = r.h;
                    found = true;
                    break;
                }
            }
            if !found {
                ww = b.bits(16)? as usize + 1;
                hh = b.bits(16)? as usize + 1;
            }
            w = ww;
            ht = hh;
            h.use_last_frame_mvs &= st.last_size == Some((w, ht));
            if b.flag()? {
                b.bits(32)?;
            }
            h.highprecisionmvs = b.flag()?;
            h.filtermode = if b.flag()? { FILTER_SWITCHABLE } else { b.bits(2)? as u8 };
            h.allowcompinter = h.signbias[0] != h.signbias[1] || h.signbias[0] != h.signbias[2];
            if h.allowcompinter {
                if h.signbias[0] == h.signbias[1] {
                    h.fixcompref = 2;
                    h.varcompref = [0, 1];
                } else if h.signbias[0] == h.signbias[2] {
                    h.fixcompref = 1;
                    h.varcompref = [0, 2];
                } else {
                    h.fixcompref = 0;
                    h.varcompref = [1, 2];
                }
            }
            let mut valid = 0;
            for i in 0..3 {
                let r = ri[i];
                if r.bpp != st.bpp || r.ss_h != st.ss_h || r.ss_v != st.ss_v {
                    return Err(HeaderError::Invalid("reference pixel format mismatch"));
                }
                if r.w == w && r.h == ht {
                    h.mvscale[i] = [0, 0];
                } else {
                    if w * 2 < r.w || ht * 2 < r.h || w > 16 * r.w || ht > 16 * r.h {
                        h.mvscale[i] = [-1, -1];
                        continue;
                    }
                    h.mvscale[i] = [((r.w << 14) / w) as i32, ((r.h << 14) / ht) as i32];
                    h.mvstep[i] = [(16 * h.mvscale[i][0]) >> 14, (16 * h.mvscale[i][1]) >> 14];
                }
                valid += 1;
            }
            if valid == 0 {
                return Err(HeaderError::Invalid("no valid reference frame"));
            }
        }
    }
    if h.keyframe || h.intraonly {
        st.bpp = h.bpp;
        st.ss_h = h.ss_h;
        st.ss_v = h.ss_v;
    }
    h.width = w;
    h.height = ht;
    h.refreshctx = if h.errorres { false } else { b.flag()? };
    h.parallelmode = if h.errorres { true } else { b.flag()? };
    let c = b.bits(2)? as usize;
    h.framectxid = if h.keyframe || h.intraonly { 0 } else { c };

    if h.keyframe || h.errorres || h.intraonly {
        st.lf_ref = [1, 0, -1, -1];
        st.lf_mode = [0, 0];
        st.feat = [SegFeat::default(); 8];
    }
    h.lf_level = b.bits(6)? as u8;
    h.lf_sharpness = b.bits(3)? as u8;
    h.lf_delta_enabled = b.flag()?;
    if h.lf_delta_enabled && b.flag()? {
        for i in 0..4 {
            if b.flag()? {
                st.lf_ref[i] = b.sbits_inv(6)?;
            }
        }
        for i in 0..2 {
            if b.flag()? {
                st.lf_mode[i] = b.sbits_inv(6)?;
            }
        }
    }

    h.yac_qi = b.bits(8)? as i32;
    h.ydc_qdelta = if b.flag()? { b.sbits_inv(4)? } else { 0 };
    h.uvdc_qdelta = if b.flag()? { b.sbits_inv(4)? } else { 0 };
    h.uvac_qdelta = if b.flag()? { b.sbits_inv(4)? } else { 0 };
    h.lossless = h.yac_qi == 0 && h.ydc_qdelta == 0 && h.uvdc_qdelta == 0 && h.uvac_qdelta == 0;

    h.seg.enabled = b.flag()?;
    if h.seg.enabled {
        h.seg.update_map = b.flag()?;
        if h.seg.update_map {
            for i in 0..7 {
                h.seg.prob[i] = if b.flag()? { b.bits(8)? as u8 } else { 255 };
            }
            h.seg.temporal = b.flag()?;
            if h.seg.temporal {
                for i in 0..3 {
                    h.seg.pred_prob[i] = if b.flag()? { b.bits(8)? as u8 } else { 255 };
                }
            }
        }
        if b.flag()? {
            st.seg_abs = b.flag()?;
            for i in 0..8 {
                let f = &mut st.feat[i];
                f.q_enabled = b.flag()?;
                if f.q_enabled {
                    f.q_val = b.sbits_inv(8)?;
                }
                f.lf_enabled = b.flag()?;
                if f.lf_enabled {
                    f.lf_val = b.sbits_inv(6)?;
                }
                f.ref_enabled = b.flag()?;
                if f.ref_enabled {
                    f.ref_val = b.bits(2)? as u8;
                }
                f.skip_enabled = b.flag()?;
            }
        }
    }
    h.seg.absolute_vals = st.seg_abs;
    h.seg.feat = st.feat;

    let qtab = ((h.bpp - 8) / 2) as usize;
    for i in 0..(if h.seg.enabled { 8 } else { 1 }) {
        let f = h.seg.feat[i];
        let mut qyac = if h.seg.enabled && f.q_enabled {
            if h.seg.absolute_vals { clip_u(f.q_val, 8) } else { clip_u(h.yac_qi + f.q_val, 8) }
        } else {
            h.yac_qi
        };
        let qydc = clip_u(qyac + h.ydc_qdelta, 8);
        let quvdc = clip_u(qyac + h.uvdc_qdelta, 8);
        let quvac = clip_u(qyac + h.uvac_qdelta, 8);
        qyac = clip_u(qyac, 8);
        let fe = &mut h.seg.feat[i];
        fe.qmul[0][0] = DC_QLOOKUP[qtab][qydc as usize] as i32;
        fe.qmul[0][1] = AC_QLOOKUP[qtab][qyac as usize] as i32;
        fe.qmul[1][0] = DC_QLOOKUP[qtab][quvdc as usize] as i32;
        fe.qmul[1][1] = AC_QLOOKUP[qtab][quvac as usize] as i32;

        let sh = (h.lf_level >= 32) as i32;
        let lflvl = if h.seg.enabled && f.lf_enabled {
            if h.seg.absolute_vals { clip_u(f.lf_val, 6) } else { clip_u(h.lf_level as i32 + f.lf_val, 6) }
        } else {
            h.lf_level as i32
        };
        if h.lf_delta_enabled {
            let v = clip_u(lflvl + (st.lf_ref[0] << sh), 6) as u8;
            fe.lflvl[0] = [v, v];
            for j in 1..4 {
                fe.lflvl[j][0] = clip_u(lflvl + ((st.lf_ref[j] + st.lf_mode[0]) << sh), 6) as u8;
                fe.lflvl[j][1] = clip_u(lflvl + ((st.lf_ref[j] + st.lf_mode[1]) << sh), 6) as u8;
            }
        } else {
            fe.lflvl = [[lflvl as u8; 2]; 4];
        }
    }

    // tiling
    let sb_cols = w.div_ceil(64);
    h.log2_tile_cols = 0;
    while sb_cols > (64usize << h.log2_tile_cols) {
        h.log2_tile_cols += 1;
    }
    let mut max = 0;
    while (sb_cols >> max) >= 4 {
        max += 1;
    }
    let max = (max - 1).max(0) as u32;
    while max > h.log2_tile_cols {
        if b.flag()? {
            h.log2_tile_cols += 1;
        } else {
            break;
        }
    }
    h.log2_tile_rows = if b.flag()? { 1 + b.bit()? } else { 0 };

    if h.keyframe || h.errorres || (h.intraonly && h.resetctx == 3) {
        for i in 0..4 {
            st.prob_ctx[i] = ProbContext::default_ctx();
        }
    } else if h.intraonly && h.resetctx == 2 {
        st.prob_ctx[c] = ProbContext::default_ctx();
    }

    h.compressed_size = b.bits(16)? as usize;
    h.uncompressed_size = b.pos.div_ceil(8);
    if h.compressed_size == 0 || h.uncompressed_size + h.compressed_size > data.len() {
        return Err(HeaderError::Invalid("bad compressed header size"));
    }
    Ok(h)
}

fn inv_recenter_nonneg(v: i32, m: i32) -> i32 {
    if v > 2 * m {
        v
    } else if v & 1 != 0 {
        m - ((v + 1) >> 1)
    } else {
        m + (v >> 1)
    }
}

fn update_prob(c: &mut Rac, p: u8) -> u8 {
    let d = if !c.bit() {
        c.uint(4) as usize
    } else if !c.bit() {
        c.uint(4) as usize + 16
    } else if !c.bit() {
        c.uint(5) as usize + 32
    } else {
        let mut v = c.uint(7) as usize;
        if v >= 65 {
            v = (v << 1) - 65 + c.bit() as usize;
        }
        (v + 64).min(254)
    };
    let p = p as i32;
    let m = INV_MAP_TABLE[d] as i32;
    (if p <= 128 { 1 + inv_recenter_nonneg(m, p - 1) } else { 255 - inv_recenter_nonneg(m, 255 - p) }) as u8
}

fn upd(c: &mut Rac, p: &mut u8) {
    if c.get(252) {
        *p = update_prob(c, *p);
    }
}

fn upd_mv(c: &mut Rac, p: &mut u8) {
    if c.get(252) {
        *p = ((c.uint(7) << 1) | 1) as u8;
    }
}

/// Parses the compressed header, producing the working probabilities for this frame.
pub fn parse_compressed(data: &[u8], h: &mut FrameHeader, st: &HeaderState) -> R<(Probs, CoefProbs)> {
    let mut c = Rac::new(data).ok_or(HeaderError::Invalid("empty compressed header"))?;
    if c.get(128) {
        return Err(HeaderError::Invalid("marker bit set"));
    }
    let ctx = &st.prob_ctx[h.framectxid];
    let mut p = ctx.p;
    let mut coef: CoefProbs = [[[[[[0; 11]; 6]; 6]; 2]; 2]; 4];

    if h.lossless {
        h.txfmmode = TX_4X4;
    } else {
        h.txfmmode = c.uint(2) as u8;
        if h.txfmmode == 3 {
            h.txfmmode += c.bit() as u8;
        }
        if h.txfmmode == TX_SWITCHABLE {
            for i in 0..2 {
                upd(&mut c, &mut p.tx8p[i]);
            }
            for i in 0..2 {
                for j in 0..2 {
                    upd(&mut c, &mut p.tx16p[i][j]);
                }
            }
            for i in 0..2 {
                for j in 0..3 {
                    upd(&mut c, &mut p.tx32p[i][j]);
                }
            }
        }
    }

    for i in 0..4 {
        let rf = &ctx.coef[i];
        let update = c.bit();
        for j in 0..2 {
            for k in 0..2 {
                for l in 0..6 {
                    for m in 0..6 {
                        if m >= 3 && l == 0 {
                            break;
                        }
                        let mut q = rf[j][k][l][m];
                        if update {
                            for n in 0..3 {
                                if c.get(252) {
                                    q[n] = update_prob(&mut c, q[n]);
                                }
                            }
                        }
                        let o = &mut coef[i][j][k][l][m];
                        o[..3].copy_from_slice(&q);
                        o[3..].copy_from_slice(&MODEL_PARETO8[q[2] as usize]);
                    }
                }
            }
        }
        if h.txfmmode as usize == i {
            break;
        }
    }

    for i in 0..3 {
        upd(&mut c, &mut p.skip[i]);
    }
    if !h.keyframe && !h.intraonly {
        for i in 0..7 {
            for j in 0..3 {
                upd(&mut c, &mut p.mv_mode[i][j]);
            }
        }
        if h.filtermode == FILTER_SWITCHABLE {
            for i in 0..4 {
                for j in 0..2 {
                    upd(&mut c, &mut p.filter[i][j]);
                }
            }
        }
        for i in 0..4 {
            upd(&mut c, &mut p.intra[i]);
        }
        if h.allowcompinter {
            h.comppredmode = c.bit() as u8;
            if h.comppredmode != 0 {
                h.comppredmode += c.bit() as u8;
            }
            if h.comppredmode == PRED_SWITCHABLE {
                for i in 0..5 {
                    upd(&mut c, &mut p.comp[i]);
                }
            }
        } else {
            h.comppredmode = PRED_SINGLEREF;
        }
        if h.comppredmode != PRED_COMPREF {
            for i in 0..5 {
                upd(&mut c, &mut p.single_ref[i][0]);
                upd(&mut c, &mut p.single_ref[i][1]);
            }
        }
        if h.comppredmode != PRED_SINGLEREF {
            for i in 0..5 {
                upd(&mut c, &mut p.comp_ref[i]);
            }
        }
        for i in 0..4 {
            for j in 0..9 {
                upd(&mut c, &mut p.y_mode[i][j]);
            }
        }
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..3 {
                    upd(&mut c, &mut p.partition[3 - i][j][k]);
                }
            }
        }
        for i in 0..3 {
            upd_mv(&mut c, &mut p.mv_joint[i]);
        }
        for i in 0..2 {
            let m = &mut p.mv_comp[i];
            upd_mv(&mut c, &mut m.sign);
            for j in 0..10 {
                upd_mv(&mut c, &mut m.classes[j]);
            }
            upd_mv(&mut c, &mut m.class0);
            for j in 0..10 {
                upd_mv(&mut c, &mut m.bits[j]);
            }
        }
        for i in 0..2 {
            let m = &mut p.mv_comp[i];
            for j in 0..2 {
                for k in 0..3 {
                    upd_mv(&mut c, &mut m.class0_fp[j][k]);
                }
            }
            for j in 0..3 {
                upd_mv(&mut c, &mut m.fp[j]);
            }
        }
        if h.highprecisionmvs {
            for i in 0..2 {
                let m = &mut p.mv_comp[i];
                upd_mv(&mut c, &mut m.class0_hp);
                upd_mv(&mut c, &mut m.hp);
            }
        }
    }
    Ok((p, coef))
}
