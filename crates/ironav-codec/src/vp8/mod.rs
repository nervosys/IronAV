//! VP8 video decoder (RFC 6386), ported from FFmpeg's `vp8.c`.
//!
//! Every macroblock is predicted and reconstructed into an unfiltered frame first and the loop
//! filter then runs over the whole frame, which is what the format specifies: intra prediction
//! reads unfiltered pixels, while later frames reference the filtered ones.

mod dsp;
pub(crate) mod enc;
mod pred;
mod tables;

pub use enc::Vp8Encoder;

use std::sync::Arc;

use dsp::RefPlane;
use ironav_core::{Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame};
use pred::*;
use tables::*;

const MODE_I4X4: u8 = 4;
const MV_ZERO: u8 = 5;
const MV_MV: u8 = 6;
const MV_SPLIT: u8 = 7;

const SPLIT_16X8: u8 = 0;
const SPLIT_8X8: u8 = 2;
const SPLIT_4X4: u8 = 3;
const SPLIT_NONE: u8 = 4;

const PRED16X16_TREE_INTRA: [[i8; 2]; 4] = [[-4, 1], [2, 3], [0, -2], [-1, -3]];
const PRED16X16_TREE_INTER: [[i8; 2]; 4] = [[0, 1], [2, 3], [-2, -1], [-3, -4]];
const PRED8X8C_TREE: [[i8; 2]; 3] = [[0, 1], [-2, 2], [-1, -3]];
const PRED4X4_TREE: [[i8; 2]; 9] = [[-2, 1], [-9, 2], [0, 3], [4, 6], [-1, 5], [-4, -5], [-3, 7], [-7, 8], [-6, -8]];

const PRED16X16_PROB_INTRA: [u8; 4] = [145, 156, 163, 128];
const PRED16X16_PROB_INTER: [u8; 4] = [112, 86, 140, 37];
const PRED8X8C_PROB_INTRA: [u8; 3] = [142, 114, 183];
const PRED8X8C_PROB_INTER: [u8; 3] = [162, 101, 204];
const PRED4X4_PROB_INTER: [u8; 9] = [120, 90, 79, 133, 87, 85, 80, 111, 151];

const MODE_CONTEXTS: [[u8; 4]; 6] = [[7, 1, 1, 143], [14, 18, 14, 107], [135, 64, 57, 68], [60, 56, 128, 65], [159, 134, 128, 34], [234, 188, 128, 28]];

const MBSPLITS: [[u8; 16]; 5] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
    [0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1],
    [0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [0; 16],
];
const MBFIRSTIDX: [[u8; 16]; 4] = [
    [0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 2, 8, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
];
const MBSPLIT_COUNT: [usize; 4] = [2, 2, 4, 16];
const MBSPLIT_PROB: [u8; 3] = [110, 111, 150];
const SUBMV_PROB: [[u8; 3]; 5] = [[147, 136, 18], [106, 145, 1], [179, 121, 1], [223, 1, 34], [208, 1, 1]];

const COEFF_BAND: [usize; 16] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7];
const COEFF_BAND_INDEXES: [&[usize]; 8] = [&[0], &[1], &[2], &[3], &[5], &[6], &[4, 7, 8, 9, 10, 11, 12, 13, 14], &[15]];
const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

const DCT_CAT1_PROB: [u8; 1] = [159];
const DCT_CAT2_PROB: [u8; 2] = [165, 145];
const DCT_CAT_PROB: [&[u8]; 4] = [&[173, 148, 140], &[176, 155, 140, 135], &[180, 157, 141, 134, 130], &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129]];

const HEV_THRESH: [[u8; 64]; 2] = {
    let mut t = [[0u8; 64]; 2];
    let mut i = 0;
    while i < 64 {
        t[0][i] = if i < 15 {
            0
        } else if i < 20 {
            1
        } else if i < 40 {
            2
        } else {
            3
        };
        t[1][i] = if i < 15 {
            0
        } else if i < 40 {
            1
        } else {
            2
        };
        i += 1;
    }
    t
};

/// Dequantisation factors for base index `base` and the deltas `[y dc, y2 dc, y2 ac, uv dc, uv ac]`.
fn quant_factors(base: i32, d: [i32; 5]) -> Quant {
    let q = |d: i32| (base + d).clamp(0, 127) as usize;
    Quant {
        y: [DC_QLOOKUP[q(d[0])] as i32, AC_QLOOKUP[q(0)] as i32],
        y2: [DC_QLOOKUP[q(d[1])] as i32 * 2, ((AC_QLOOKUP[q(d[2])] as i32 * 101581) >> 16).max(8)],
        uv: [(DC_QLOOKUP[q(d[3])] as i32).min(132), AC_QLOOKUP[q(d[4])] as i32],
    }
}

/// Puts the candidate vectors in the order the mode contexts expect, nearest first.
fn sort_near(near: &mut [Mv; 4], cnt: &mut [usize; 4]) {
    // with three distinct vectors the first and last may still match
    if cnt[3] != 0 && near[1] == near[3] {
        cnt[1] += 1;
    }
    if cnt[2] > cnt[1] {
        cnt.swap(1, 2);
        near.swap(1, 2);
    }
}

/// The boolean entropy decoder shared by every VP8 partition.
struct BoolDecoder<'a> {
    buf: &'a [u8],
    pos: usize,
    value: u32,
    range: u32,
    bit_count: u32,
}

impl<'a> BoolDecoder<'a> {
    fn new(buf: &'a [u8]) -> Self {
        let b = |i: usize| buf.get(i).copied().unwrap_or(0) as u32;
        Self { buf, pos: 2, value: (b(0) << 8) | b(1), range: 255, bit_count: 0 }
    }

    fn get(&mut self, prob: u8) -> bool {
        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        let big = split << 8;
        let bit = if self.value >= big {
            self.range -= split;
            self.value -= big;
            true
        } else {
            self.range = split;
            false
        };
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.bit_count += 1;
            if self.bit_count == 8 {
                self.bit_count = 0;
                self.value |= self.buf.get(self.pos).copied().unwrap_or(0) as u32;
                self.pos += 1;
            }
        }
        bit
    }

    fn bit(&mut self) -> bool {
        self.get(128)
    }

    fn uint(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| (v << 1) | self.bit() as u32)
    }

    fn sint(&mut self, n: u32) -> i32 {
        if !self.bit() {
            return 0;
        }
        let v = self.uint(n) as i32;
        if self.bit() {
            -v
        } else {
            v
        }
    }

    fn tree(&mut self, tree: &[[i8; 2]], probs: &[u8]) -> u8 {
        let mut i = 0i8;
        loop {
            i = tree[i as usize][self.get(probs[i as usize]) as usize];
            if i <= 0 {
                return (-i) as u8;
            }
        }
    }

    /// True once the decoder has read well past the end of its data.
    fn overrun(&self) -> bool {
        self.pos > self.buf.len() + 16
    }
}

#[derive(Clone)]
struct Probs {
    token: [[[[u8; 11]; 3]; 16]; 4],
    pred16x16: [u8; 4],
    pred8x8c: [u8; 3],
    mvc: [[u8; 19]; 2],
    segmentid: [u8; 3],
    mbskip: u8,
    intra: u8,
    last: u8,
    golden: u8,
}

impl Probs {
    fn new() -> Self {
        let mut p = Self {
            token: [[[[128; 11]; 3]; 16]; 4],
            pred16x16: PRED16X16_PROB_INTER,
            pred8x8c: PRED8X8C_PROB_INTER,
            mvc: MV_DEFAULT_PROB,
            segmentid: [255; 3],
            mbskip: 0,
            intra: 0,
            last: 0,
            golden: 0,
        };
        p.reset_tokens();
        p
    }

    fn reset_tokens(&mut self) {
        for i in 0..4 {
            for j in 0..16 {
                self.token[i][j] = TOKEN_DEFAULT_PROBS[i][COEFF_BAND[j]];
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Segmentation {
    enabled: bool,
    update_map: bool,
    absolute: bool,
    base_quant: [i32; 4],
    filter_level: [i32; 4],
}

#[derive(Clone, Copy, Default)]
struct LfDelta {
    enabled: bool,
    refs: [i32; 4],
    mode: [i32; 8],
}

/// Dequantisation factors as `[dc, ac]`.
#[derive(Clone, Copy, Default)]
struct Quant {
    y: [i32; 2],
    y2: [i32; 2],
    uv: [i32; 2],
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Mv {
    x: i16,
    y: i16,
}

impl Mv {
    fn is_zero(self) -> bool {
        self.x == 0 && self.y == 0
    }
}

#[derive(Clone, Copy)]
struct Mb {
    mode: u8,
    /// 0 intra, 1 last, 2 golden, 3 altref
    ref_frame: u8,
    partitioning: u8,
    segment: u8,
    skip: bool,
    chroma_mode: u8,
    mv: Mv,
    bmv: [Mv; 16],
    i4: [u8; 16],
}

impl Default for Mb {
    fn default() -> Self {
        Self { mode: 0, ref_frame: 0, partitioning: SPLIT_16X8, segment: 0, skip: false, chroma_mode: 0, mv: Mv::default(), bmv: [Mv::default(); 16], i4: [B_DC; 16] }
    }
}

struct Picture {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
struct FilterStrength {
    level: i32,
    inner_limit: i32,
    inner: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefUpdate {
    None,
    Current,
    Previous,
    Golden,
    Altref,
}

pub struct Vp8Decoder {
    width: usize,
    height: usize,
    mbw: usize,
    mbh: usize,
    probs: Probs,
    seg: Segmentation,
    lf: LfDelta,
    qmat: [Quant; 4],
    sign_bias: [bool; 4],
    refs: [Option<Arc<Picture>>; 4],
    mbs: Vec<Mb>,
    seg_map: Vec<u8>,
    have_key: bool,
}

impl Vp8Decoder {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            mbw: 0,
            mbh: 0,
            probs: Probs::new(),
            seg: Segmentation::default(),
            lf: LfDelta::default(),
            qmat: [Quant::default(); 4],
            sign_bias: [false; 4],
            refs: [None, None, None, None],
            mbs: Vec::new(),
            seg_map: Vec::new(),
            have_key: false,
        }
    }

    fn resize(&mut self, w: usize, h: usize) {
        self.width = w;
        self.height = h;
        self.mbw = w.div_ceil(16);
        self.mbh = h.div_ceil(16);
        // one border row above and one border column to the left, never written
        self.mbs = vec![Mb::default(); (self.mbw + 1) * (self.mbh + 1)];
        self.seg_map = vec![0; self.mbw * self.mbh];
        self.refs = [None, None, None, None];
    }

    fn decode_frame(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
        if data.len() < 3 {
            return Err(Error::invalid_input("VP8 frame is shorter than its header"));
        }
        let keyframe = data[0] & 1 == 0;
        let profile = (data[0] >> 1) & 7;
        let show = data[0] & 0x10 != 0;
        let header_size = (u32::from_le_bytes([data[0], data[1], data[2], 0]) >> 5) as usize;
        let mut buf = &data[3..];
        if header_size + 7 * keyframe as usize > buf.len() {
            return Err(Error::invalid_input("VP8 first partition is larger than the frame"));
        }
        if keyframe {
            if buf[..3] != [0x9d, 0x01, 0x2a] {
                return Err(Error::invalid_input("VP8 keyframe has no start code"));
            }
            let w = (u16::from_le_bytes([buf[3], buf[4]]) & 0x3fff) as usize;
            let h = (u16::from_le_bytes([buf[5], buf[6]]) & 0x3fff) as usize;
            if w == 0 || h == 0 {
                return Err(Error::invalid_input("VP8 keyframe has a zero dimension"));
            }
            buf = &buf[7..];
            if w != self.width || h != self.height || self.mbs.is_empty() {
                self.resize(w, h);
            }
            self.probs.reset_tokens();
            self.probs.pred16x16 = PRED16X16_PROB_INTER;
            self.probs.pred8x8c = PRED8X8C_PROB_INTER;
            self.probs.mvc = MV_DEFAULT_PROB;
            self.seg = Segmentation::default();
            self.lf = LfDelta::default();
        } else if !self.have_key {
            return Ok(None);
        }
        let mut rc = BoolDecoder::new(&buf[..header_size]);
        let rest = &buf[header_size..];
        if keyframe {
            rc.bit(); // colour space
            rc.bit(); // clamping type; the decoder always clamps
        }

        self.seg.enabled = rc.bit();
        if self.seg.enabled {
            self.seg.update_map = rc.bit();
            let update_data = rc.bit();
            if update_data {
                self.seg.absolute = rc.bit();
                for i in 0..4 {
                    self.seg.base_quant[i] = rc.sint(7);
                }
                for i in 0..4 {
                    self.seg.filter_level[i] = rc.sint(6);
                }
            }
            if self.seg.update_map {
                for i in 0..3 {
                    self.probs.segmentid[i] = if rc.bit() { rc.uint(8) as u8 } else { 255 };
                }
            }
        } else {
            self.seg.update_map = false;
        }

        let simple = rc.bit();
        let filter_level = rc.uint(6) as i32;
        let sharpness = rc.uint(3) as i32;

        self.lf.enabled = rc.bit();
        if self.lf.enabled && rc.bit() {
            for i in 0..4 {
                if rc.bit() {
                    let v = rc.uint(6) as i32;
                    self.lf.refs[i] = if rc.bit() { -v } else { v };
                }
            }
            for i in 4..8 {
                if rc.bit() {
                    let v = rc.uint(6) as i32;
                    self.lf.mode[i] = if rc.bit() { -v } else { v };
                }
            }
        }

        // token partitions
        let nparts = 1usize << rc.uint(2);
        let table = 3 * (nparts - 1);
        if rest.len() < table {
            return Err(Error::invalid_input("VP8 partition table is truncated"));
        }
        let mut parts = Vec::with_capacity(nparts);
        let mut at = table;
        for i in 0..nparts - 1 {
            let size = u32::from_le_bytes([rest[3 * i], rest[3 * i + 1], rest[3 * i + 2], 0]) as usize;
            if at + size > rest.len() {
                return Err(Error::invalid_input("VP8 token partition is larger than the frame"));
            }
            parts.push(BoolDecoder::new(&rest[at..at + size]));
            at += size;
        }
        parts.push(BoolDecoder::new(&rest[at..]));

        // quantisers
        let yac_qi = rc.uint(7) as i32;
        let ydc = rc.sint(4);
        let y2dc = rc.sint(4);
        let y2ac = rc.sint(4);
        let uvdc = rc.sint(4);
        let uvac = rc.sint(4);
        for i in 0..4 {
            let mut base = yac_qi;
            if self.seg.enabled {
                base = self.seg.base_quant[i] + if self.seg.absolute { 0 } else { yac_qi };
            }
            self.qmat[i] = quant_factors(base, [ydc, y2dc, y2ac, uvdc, uvac]);
        }

        // reference buffer updates
        let (mut update_golden, mut update_altref) = (RefUpdate::Current, RefUpdate::Current);
        if !keyframe {
            let g = rc.bit();
            let a = rc.bit();
            let pick = |rc: &mut BoolDecoder, flag: bool, other: RefUpdate| {
                if flag {
                    RefUpdate::Current
                } else {
                    match rc.uint(2) {
                        1 => RefUpdate::Previous,
                        2 => other,
                        _ => RefUpdate::None,
                    }
                }
            };
            update_golden = pick(&mut rc, g, RefUpdate::Altref);
            update_altref = pick(&mut rc, a, RefUpdate::Golden);
            self.sign_bias[2] = rc.bit();
            self.sign_bias[3] = rc.bit();
        }

        let update_probabilities = rc.bit();
        let saved = if update_probabilities { None } else { Some(self.probs.clone()) };
        let update_last = keyframe || rc.bit();

        for i in 0..4 {
            for j in 0..8 {
                for k in 0..3 {
                    for l in 0..11 {
                        if rc.get(TOKEN_UPDATE_PROBS[i][j][k][l]) {
                            let p = rc.uint(8) as u8;
                            for &m in COEFF_BAND_INDEXES[j] {
                                self.probs.token[i][m][k][l] = p;
                            }
                        }
                    }
                }
            }
        }

        let mbskip_enabled = rc.bit();
        if mbskip_enabled {
            self.probs.mbskip = rc.uint(8) as u8;
        }
        if !keyframe {
            self.probs.intra = rc.uint(8) as u8;
            self.probs.last = rc.uint(8) as u8;
            self.probs.golden = rc.uint(8) as u8;
            if rc.bit() {
                for i in 0..4 {
                    self.probs.pred16x16[i] = rc.uint(8) as u8;
                }
            }
            if rc.bit() {
                for i in 0..3 {
                    self.probs.pred8x8c[i] = rc.uint(8) as u8;
                }
            }
            for i in 0..2 {
                for j in 0..19 {
                    if rc.get(MV_UPDATE_PROB[i][j]) {
                        let v = (rc.uint(7) << 1) as u8;
                        self.probs.mvc[i][j] = v + (v == 0) as u8;
                    }
                }
            }
        }

        if !keyframe && (self.refs[1].is_none() || self.refs[2].is_none() || self.refs[3].is_none()) {
            if let Some(p) = saved {
                self.probs = p;
            }
            return Ok(None);
        }

        let result = self.decode_macroblocks(&mut rc, &mut parts, keyframe, profile, simple, filter_level, sharpness, mbskip_enabled);
        if let Some(p) = saved {
            self.probs = p;
        }
        let cur = Arc::new(result?);

        // golden and altref may swap, so every source is read from the old set
        let old = self.refs.clone();
        let from = |u: RefUpdate, keep: usize| match u {
            RefUpdate::None => old[keep].clone(),
            RefUpdate::Current => Some(cur.clone()),
            RefUpdate::Previous => old[1].clone(),
            RefUpdate::Golden => old[2].clone(),
            RefUpdate::Altref => old[3].clone(),
        };
        self.refs[3] = from(update_altref, 3);
        self.refs[2] = from(update_golden, 2);
        if update_last {
            self.refs[1] = Some(cur.clone());
        }
        self.have_key = true;

        if !show {
            return Ok(None);
        }
        let (w, h) = (self.width, self.height);
        let crop = |src: &[u8], stride: usize, cw: usize, ch: usize| {
            let mut out = Vec::with_capacity(cw * ch);
            for row in 0..ch {
                out.extend_from_slice(&src[row * stride..row * stride + cw]);
            }
            out
        };
        let layout = PixelFormat::Yuv420p.planes(w, h);
        let planes = vec![
            crop(&cur.y, self.mbw * 16, layout[0].0, layout[0].1),
            crop(&cur.u, self.mbw * 8, layout[1].0, layout[1].1),
            crop(&cur.v, self.mbw * 8, layout[2].0, layout[2].1),
        ];
        Ok(Some(VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes }))
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_macroblocks(
        &mut self,
        rc: &mut BoolDecoder,
        parts: &mut [BoolDecoder],
        keyframe: bool,
        profile: u8,
        simple: bool,
        filter_level: i32,
        sharpness: i32,
        mbskip_enabled: bool,
    ) -> Result<Picture> {
        let (mbw, mbh) = (self.mbw, self.mbh);
        let (ys, cs) = (mbw * 16, mbw * 8);
        let mut cur = Picture { y: vec![0; ys * mbh * 16], u: vec![0; cs * mbh * 8], v: vec![0; cs * mbh * 8] };
        let mut top_nnz = vec![[0u8; 9]; mbw];
        let mut top_modes = vec![B_DC; mbw * 4];
        let mut strengths = vec![FilterStrength::default(); mbw * mbh];
        let stride = mbw + 1;
        let nparts = parts.len();

        for mb_y in 0..mbh {
            let mut left_nnz = [0u8; 9];
            let mut left_modes = [B_DC; 4];
            for mb_x in 0..mbw {
                if rc.overrun() {
                    return Err(Error::invalid_input("VP8 mode partition ended inside the frame"));
                }
                let idx = (mb_y + 1) * stride + mb_x + 1;
                let mut mb = Mb::default();

                // segment and skip flag
                let seg_at = mb_y * mbw + mb_x;
                if self.seg.update_map {
                    let b = rc.get(self.probs.segmentid[0]) as usize;
                    self.seg_map[seg_at] = (rc.get(self.probs.segmentid[1 + b]) as usize + 2 * b) as u8;
                } else if !self.seg.enabled {
                    self.seg_map[seg_at] = 0;
                }
                mb.segment = self.seg_map[seg_at];
                mb.skip = mbskip_enabled && rc.get(self.probs.mbskip);

                if keyframe {
                    mb.mode = rc.tree(&PRED16X16_TREE_INTRA, &PRED16X16_PROB_INTRA);
                    if mb.mode == MODE_I4X4 {
                        for y in 0..4 {
                            for x in 0..4 {
                                let ctx = &PRED4X4_PROB_INTRA[top_modes[mb_x * 4 + x] as usize][left_modes[y] as usize];
                                let m = rc.tree(&PRED4X4_TREE, ctx);
                                mb.i4[y * 4 + x] = m;
                                left_modes[y] = m;
                                top_modes[mb_x * 4 + x] = m;
                            }
                        }
                    } else {
                        let m = match mb.mode {
                            DC_PRED8 => B_DC,
                            VERT_PRED8 => B_VERT,
                            HOR_PRED8 => B_HOR,
                            _ => B_TM,
                        };
                        top_modes[mb_x * 4..mb_x * 4 + 4].fill(m);
                        left_modes = [m; 4];
                    }
                    mb.chroma_mode = rc.tree(&PRED8X8C_TREE, &PRED8X8C_PROB_INTRA);
                    mb.partitioning = SPLIT_NONE;
                } else if rc.get(self.probs.intra) {
                    mb.ref_frame = if rc.get(self.probs.last) {
                        if rc.get(self.probs.golden) {
                            3
                        } else {
                            2
                        }
                    } else {
                        1
                    };
                    self.decode_mvs(rc, &mut mb, idx, stride, mb_x, mb_y);
                } else {
                    mb.mode = rc.tree(&PRED16X16_TREE_INTER, &self.probs.pred16x16);
                    if mb.mode == MODE_I4X4 {
                        for i in 0..16 {
                            mb.i4[i] = rc.tree(&PRED4X4_TREE, &PRED4X4_PROB_INTER);
                        }
                    }
                    mb.chroma_mode = rc.tree(&PRED8X8C_TREE, &self.probs.pred8x8c);
                    mb.partitioning = SPLIT_NONE;
                }

                // coefficients
                let mut blocks = [[0i16; 16]; 24];
                let t_nnz = &mut top_nnz[mb_x];
                if !mb.skip {
                    let pc = &mut parts[mb_y & (nparts - 1)];
                    if pc.overrun() {
                        return Err(Error::invalid_input("VP8 token partition ended inside the frame"));
                    }
                    self.decode_coeffs(pc, &mut mb, t_nnz, &mut left_nnz, &mut blocks);
                } else {
                    // a skipped macroblock leaves the Y2 context alone when it has no Y2 block
                    let keep = (mb.mode == MODE_I4X4 || mb.mode == MV_SPLIT).then(|| (left_nnz[8], t_nnz[8]));
                    left_nnz = [0; 9];
                    *t_nnz = [0; 9];
                    if let Some((l, t)) = keep {
                        left_nnz[8] = l;
                        t_nnz[8] = t;
                    }
                }

                // reconstruction
                if mb.ref_frame == 0 {
                    self.intra_predict(&mut cur, &mb, &blocks, mb_x, mb_y);
                } else {
                    self.inter_predict(&mut cur, &mb, mb_x, mb_y, profile)?;
                }
                if !mb.skip {
                    if mb.mode != MODE_I4X4 {
                        for b in 0..16 {
                            let off = (mb_y * 16 + (b / 4) * 4) * ys + mb_x * 16 + (b % 4) * 4;
                            dsp::idct_add(&mut cur.y, off, ys, &blocks[b]);
                        }
                    }
                    for b in 0..4 {
                        let off = (mb_y * 8 + (b / 2) * 4) * cs + mb_x * 8 + (b % 2) * 4;
                        dsp::idct_add(&mut cur.u, off, cs, &blocks[16 + b]);
                        dsp::idct_add(&mut cur.v, off, cs, &blocks[20 + b]);
                    }
                }

                strengths[seg_at] = self.filter_strength(&mb, filter_level, sharpness);
                self.mbs[idx] = mb;
            }
        }

        if filter_level != 0 {
            self.loop_filter(&mut cur, &strengths, simple, keyframe);
        }
        Ok(cur)
    }

    /// Clamps a vector so that the macroblock at (`mb_x`, `mb_y`) stays within a margin of the frame.
    fn clamp_mv(&self, mb_x: usize, mb_y: usize, m: Mv) -> Mv {
        let (mbw, mbh) = (self.mbw as i32, self.mbh as i32);
        const MARGIN: i32 = 16 << 2;
        let min_x = -MARGIN - 64 * mb_x as i32;
        let max_x = ((mbw - 1) << 6) + MARGIN - 64 * mb_x as i32;
        let min_y = -MARGIN - 64 * mb_y as i32;
        let max_y = ((mbh - 1) << 6) + MARGIN - 64 * mb_y as i32;
        Mv { x: (m.x as i32).clamp(min_x, max_x) as i16, y: (m.y as i32).clamp(min_y, max_y) as i16 }
    }

    /// The candidate vectors from the above, left and above-left macroblocks (the first is zero), and
    /// their weights, for a macroblock at `idx` predicting from `ref_frame`.
    fn near_mvs(&self, idx: usize, stride: usize, ref_frame: u8) -> ([Mv; 4], [usize; 4]) {
        let edges = [self.mbs[idx - stride], self.mbs[idx - 1], self.mbs[idx - stride - 1]];
        let mut near = [Mv::default(); 4];
        let mut cnt = [0usize; 4];
        let mut at = 0usize;
        let cur_bias = self.sign_bias[ref_frame as usize];
        for (n, e) in edges.iter().enumerate() {
            if e.ref_frame == 0 {
                continue;
            }
            let weight = 1 + (n != 2) as usize;
            if e.mv.is_zero() {
                cnt[0] += weight;
                continue;
            }
            let mut mv = e.mv;
            if cur_bias != self.sign_bias[e.ref_frame as usize] {
                mv = Mv { x: mv.x.wrapping_neg(), y: mv.y.wrapping_neg() };
            }
            if n == 0 || mv != near[at] {
                at += 1;
                near[at] = mv;
            }
            cnt[at] += weight;
        }
        (near, cnt)
    }

    fn decode_mvs(&mut self, rc: &mut BoolDecoder, mb: &mut Mb, idx: usize, stride: usize, mb_x: usize, mb_y: usize) {
        let (mut near, mut cnt) = self.near_mvs(idx, stride, mb.ref_frame);
        let edges = [self.mbs[idx - stride], self.mbs[idx - 1], self.mbs[idx - stride - 1]];

        mb.partitioning = SPLIT_NONE;
        if rc.get(MODE_CONTEXTS[cnt[0]][0]) {
            mb.mode = MV_MV;
            sort_near(&mut near, &mut cnt);
            if rc.get(MODE_CONTEXTS[cnt[1]][1]) {
                if rc.get(MODE_CONTEXTS[cnt[2]][2]) {
                    mb.mv = self.clamp_mv(mb_x, mb_y, near[(cnt[1] >= cnt[0]) as usize]);
                    let split = |m: &Mb| (m.mode == MV_SPLIT) as usize;
                    let c = (split(&edges[1]) + split(&edges[0])) * 2 + split(&edges[2]);
                    if rc.get(MODE_CONTEXTS[c][3]) {
                        mb.mode = MV_SPLIT;
                        let n = self.decode_split_mvs(rc, mb, idx, stride);
                        mb.mv = mb.bmv[n - 1];
                    } else {
                        mb.mv.y = mb.mv.y.wrapping_add(self.read_mv_component(rc, 0));
                        mb.mv.x = mb.mv.x.wrapping_add(self.read_mv_component(rc, 1));
                        mb.bmv[0] = mb.mv;
                    }
                } else {
                    mb.mv = self.clamp_mv(mb_x, mb_y, near[2]);
                    mb.bmv[0] = mb.mv;
                }
            } else {
                mb.mv = self.clamp_mv(mb_x, mb_y, near[1]);
                mb.bmv[0] = mb.mv;
            }
        } else {
            mb.mode = MV_ZERO;
            mb.mv = Mv::default();
            mb.bmv[0] = mb.mv;
        }
    }

    fn read_mv_component(&self, rc: &mut BoolDecoder, comp: usize) -> i16 {
        let p = &self.probs.mvc[comp];
        let mut x: i32 = 0;
        if rc.get(p[0]) {
            for i in 0..3 {
                x += (rc.get(p[9 + i]) as i32) << i;
            }
            for i in (4..=9).rev() {
                x += (rc.get(p[9 + i]) as i32) << i;
            }
            if x & 0xfff0 == 0 || rc.get(p[12]) {
                x += 8;
            }
        } else {
            let mut ps = 2;
            let bit = rc.get(p[ps]) as usize;
            ps += 1 + 3 * bit;
            x += 4 * bit as i32;
            let bit = rc.get(p[ps]) as usize;
            ps += 1 + bit;
            x += 2 * bit as i32;
            x += rc.get(p[ps]) as i32;
        }
        if x != 0 && rc.get(p[1]) {
            -x as i16
        } else {
            x as i16
        }
    }

    fn decode_split_mvs(&self, rc: &mut BoolDecoder, mb: &mut Mb, idx: usize, stride: usize) -> usize {
        let left_mb = &self.mbs[idx - 1];
        let top_mb = &self.mbs[idx - stride];
        let splits_left = &MBSPLITS[left_mb.partitioning as usize];
        let splits_top = &MBSPLITS[top_mb.partitioning as usize];

        let part = if rc.get(MBSPLIT_PROB[0]) {
            if rc.get(MBSPLIT_PROB[1]) {
                SPLIT_16X8 + rc.get(MBSPLIT_PROB[2]) as u8
            } else {
                SPLIT_8X8
            }
        } else {
            SPLIT_4X4
        } as usize;
        let num = MBSPLIT_COUNT[part];
        let splits_cur = &MBSPLITS[part];
        mb.partitioning = part as u8;

        for n in 0..num {
            let k = MBFIRSTIDX[part][n] as usize;
            let left = if k & 3 == 0 { left_mb.bmv[splits_left[k + 3] as usize] } else { mb.bmv[splits_cur[k - 1] as usize] };
            let above = if k <= 3 { top_mb.bmv[splits_top[k + 12] as usize] } else { mb.bmv[splits_cur[k - 4] as usize] };
            let sp = if left == above {
                &SUBMV_PROB[4 - !left.is_zero() as usize]
            } else if above.is_zero() {
                &SUBMV_PROB[2]
            } else {
                &SUBMV_PROB[1 - !left.is_zero() as usize]
            };
            mb.bmv[n] = if !rc.get(sp[0]) {
                left
            } else if !rc.get(sp[1]) {
                above
            } else if !rc.get(sp[2]) {
                Mv::default()
            } else {
                let y = mb.mv.y.wrapping_add(self.read_mv_component(rc, 0));
                let x = mb.mv.x.wrapping_add(self.read_mv_component(rc, 1));
                Mv { x, y }
            };
        }
        num
    }

    /// Reads one block's tokens. Returns 0 if none were coded, otherwise the index of the last
    /// coefficient plus one.
    fn block_coeffs(rc: &mut BoolDecoder, block: &mut [i16; 16], probs: &[[[u8; 11]; 3]; 16], start: usize, ctx: usize, qmul: [i32; 2]) -> usize {
        let mut i = start;
        let mut tp = &probs[i][ctx];
        if !rc.get(tp[0]) {
            return 0;
        }
        loop {
            if !rc.get(tp[1]) {
                // DCT_0: no end-of-block check follows
                i += 1;
                if i == 16 {
                    break;
                }
                tp = &probs[i][0];
                continue;
            }
            let coeff: i32;
            if !rc.get(tp[2]) {
                coeff = 1;
                tp = &probs[(i + 1).min(15)][1];
            } else {
                if !rc.get(tp[3]) {
                    let mut c = rc.get(tp[4]) as i32;
                    if c != 0 {
                        c += rc.get(tp[5]) as i32;
                    }
                    coeff = c + 2;
                } else if !rc.get(tp[6]) {
                    if !rc.get(tp[7]) {
                        coeff = 5 + rc.get(DCT_CAT1_PROB[0]) as i32;
                    } else {
                        let hi = rc.get(DCT_CAT2_PROB[0]) as i32;
                        coeff = 7 + (hi << 1) + rc.get(DCT_CAT2_PROB[1]) as i32;
                    }
                } else {
                    let a = rc.get(tp[8]) as usize;
                    let b = rc.get(tp[9 + a]) as usize;
                    let cat = (a << 1) + b;
                    let extra = DCT_CAT_PROB[cat].iter().fold(0, |v, &p| (v << 1) + rc.get(p) as i32);
                    coeff = 3 + (8 << cat) + extra;
                }
                tp = &probs[(i + 1).min(15)][2];
            }
            let signed = if rc.bit() { -coeff } else { coeff };
            let v = signed * qmul[(i != 0) as usize];
            block[ZIGZAG[i]] = v as i16;
            i += 1;
            if i == 16 || !rc.get(tp[0]) {
                break;
            }
        }
        i
    }

    fn decode_coeffs(&self, rc: &mut BoolDecoder, mb: &mut Mb, t: &mut [u8; 9], l: &mut [u8; 9], blocks: &mut [[i16; 16]; 24]) {
        let q = self.qmat[mb.segment as usize];
        let mut total = 0usize;
        let (mut luma_start, mut luma_ctx) = (0usize, 3usize);
        if mb.mode != MODE_I4X4 && mb.mode != MV_SPLIT {
            let mut dc = [0i16; 16];
            let nnz = Self::block_coeffs(rc, &mut dc, &self.probs.token[1], 0, (t[8] + l[8]) as usize, q.y2);
            t[8] = (nnz != 0) as u8;
            l[8] = t[8];
            if nnz != 0 {
                total += nnz;
                dsp::luma_dc_wht(&mut blocks[..16], &dc);
            }
            luma_start = 1;
            luma_ctx = 0;
        }
        for y in 0..4 {
            for x in 0..4 {
                let nnz = Self::block_coeffs(rc, &mut blocks[y * 4 + x], &self.probs.token[luma_ctx], luma_start, (l[y] + t[x]) as usize, q.y);
                t[x] = (nnz != 0) as u8;
                l[y] = t[x];
                total += nnz;
            }
        }
        for i in 4..6 {
            for y in 0..2 {
                for x in 0..2 {
                    let nnz = Self::block_coeffs(rc, &mut blocks[(i - 4) * 4 + 16 + y * 2 + x], &self.probs.token[2], 0, (l[i + 2 * y] + t[i + 2 * x]) as usize, q.uv);
                    t[i + 2 * x] = (nnz != 0) as u8;
                    l[i + 2 * y] = t[i + 2 * x];
                    total += nnz;
                }
            }
        }
        // no coded coefficients despite the flag: treat as skipped so the inner loop filter is off
        if total == 0 {
            mb.skip = true;
        }
    }

    /// The edges sub-block (`x`, `y`) of a macroblock predicts from: eight pixels above (the last
    /// four above-right), four to the left and the corner.
    fn subblock_edges(&self, p: &[u8], mb_x: usize, mb_y: usize, x: usize, y: usize) -> ([u8; 8], [u8; 4], u8) {
        let ys = self.mbw * 16;
        let px = mb_x * 16 + 4 * x;
        let py = mb_y * 16 + 4 * y;
        let mut a = [127u8; 8];
        if py > 0 {
            for i in 0..4 {
                a[i] = p[(py - 1) * ys + px + i];
            }
            if x < 3 {
                for i in 0..4 {
                    a[4 + i] = p[(py - 1) * ys + px + 4 + i];
                }
            } else if mb_y > 0 {
                let row = (mb_y * 16 - 1) * ys;
                for i in 0..4 {
                    a[4 + i] = if mb_x == self.mbw - 1 { p[row + mb_x * 16 + 15] } else { p[row + mb_x * 16 + 16 + i] };
                }
            }
        }
        let mut l = [129u8; 4];
        if px > 0 {
            for r in 0..4 {
                l[r] = p[(py + r) * ys + px - 1];
            }
        }
        let tl = if py == 0 {
            127
        } else if px == 0 {
            129
        } else {
            p[(py - 1) * ys + px - 1]
        };
        (a, l, tl)
    }

    fn intra_predict(&self, cur: &mut Picture, mb: &Mb, blocks: &[[i16; 16]; 24], mb_x: usize, mb_y: usize) {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        if mb.mode != MODE_I4X4 {
            predict_block(&mut cur.y, ys, mb_x * 16, mb_y * 16, 16, mb.mode, mb_x, mb_y);
        } else {
            for y in 0..4 {
                for x in 0..4 {
                    let px = mb_x * 16 + 4 * x;
                    let py = mb_y * 16 + 4 * y;
                    let (a, l, tl) = self.subblock_edges(&cur.y, mb_x, mb_y, x, y);
                    let o = predict4x4(mb.i4[y * 4 + x], a, l, tl);
                    for r in 0..4 {
                        cur.y[(py + r) * ys + px..(py + r) * ys + px + 4].copy_from_slice(&o[r * 4..r * 4 + 4]);
                    }
                    if !mb.skip {
                        dsp::idct_add(&mut cur.y, py * ys + px, ys, &blocks[y * 4 + x]);
                    }
                }
            }
        }
        predict_block(&mut cur.u, cs, mb_x * 8, mb_y * 8, 8, mb.chroma_mode, mb_x, mb_y);
        predict_block(&mut cur.v, cs, mb_x * 8, mb_y * 8, 8, mb.chroma_mode, mb_x, mb_y);
    }

    fn inter_predict(&self, cur: &mut Picture, mb: &Mb, mb_x: usize, mb_y: usize, profile: u8) -> Result<()> {
        let r = self.refs[mb.ref_frame as usize].as_ref().ok_or_else(|| Error::invalid_input("VP8 frame references a missing buffer"))?;
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        let (lw, lh) = (self.mbw * 16, self.mbh * 16);
        let bilinear = profile != 0;
        let yref = RefPlane { data: &r.y, stride: ys, w: lw, h: lh };
        let uref = RefPlane { data: &r.u, stride: cs, w: lw / 2, h: lh / 2 };
        let vref = RefPlane { data: &r.v, stride: cs, w: lw / 2, h: lh / 2 };
        let (x0, y0) = (mb_x * 16, mb_y * 16);

        let luma = |cur: &mut Picture, bx: usize, by: usize, bw: usize, bh: usize, mv: Mv| {
            let (mx, my) = (((mv.x as i32 * 2) & 7) as usize, ((mv.y as i32 * 2) & 7) as usize);
            let x = (x0 + bx) as i64 + (mv.x as i32 >> 2) as i64;
            let y = (y0 + by) as i64 + (mv.y as i32 >> 2) as i64;
            dsp::mc(&mut cur.y, (y0 + by) * ys + x0 + bx, ys, &yref, x, y, mx, my, bw, bh, bilinear);
        };
        let chroma = |cur: &mut Picture, bx: usize, by: usize, bw: usize, bh: usize, mut mv: (i32, i32)| {
            if profile == 3 {
                mv = (mv.0 & !7, mv.1 & !7);
            }
            let (mx, my) = ((mv.0 & 7) as usize, (mv.1 & 7) as usize);
            let x = (x0 / 2 + bx) as i64 + (mv.0 >> 3) as i64;
            let y = (y0 / 2 + by) as i64 + (mv.1 >> 3) as i64;
            let off = (y0 / 2 + by) * cs + x0 / 2 + bx;
            dsp::mc(&mut cur.u, off, cs, &uref, x, y, mx, my, bw, bh, bilinear);
            dsp::mc(&mut cur.v, off, cs, &vref, x, y, mx, my, bw, bh, bilinear);
        };
        let part = |cur: &mut Picture, bx: usize, by: usize, bw: usize, bh: usize, mv: Mv| {
            luma(cur, bx, by, bw, bh, mv);
            chroma(cur, bx / 2, by / 2, bw / 2, bh / 2, (mv.x as i32, mv.y as i32));
        };

        match mb.partitioning {
            SPLIT_NONE => part(cur, 0, 0, 16, 16, mb.mv),
            SPLIT_4X4 => {
                for y in 0..4 {
                    for x in 0..4 {
                        luma(cur, 4 * x, 4 * y, 4, 4, mb.bmv[4 * y + x]);
                    }
                }
                for y in 0..2 {
                    for x in 0..2 {
                        let at = |dy: usize, dx: usize| mb.bmv[(2 * y + dy) * 4 + 2 * x + dx];
                        let (mut sx, mut sy) = (0i32, 0i32);
                        for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            sx += at(dy, dx).x as i32;
                            sy += at(dy, dx).y as i32;
                        }
                        let round = |s: i32| (s + 2 + (s >> 31)) >> 2;
                        chroma(cur, 4 * x, 4 * y, 4, 4, (round(sx), round(sy)));
                    }
                }
            }
            SPLIT_16X8 => {
                part(cur, 0, 0, 16, 8, mb.bmv[0]);
                part(cur, 0, 8, 16, 8, mb.bmv[1]);
            }
            1 => {
                part(cur, 0, 0, 8, 16, mb.bmv[0]);
                part(cur, 8, 0, 8, 16, mb.bmv[1]);
            }
            _ => {
                part(cur, 0, 0, 8, 8, mb.bmv[0]);
                part(cur, 8, 0, 8, 8, mb.bmv[1]);
                part(cur, 0, 8, 8, 8, mb.bmv[2]);
                part(cur, 8, 8, 8, 8, mb.bmv[3]);
            }
        }
        Ok(())
    }

    fn filter_strength(&self, mb: &Mb, frame_level: i32, sharpness: i32) -> FilterStrength {
        let mut level = if self.seg.enabled {
            let l = self.seg.filter_level[mb.segment as usize];
            if self.seg.absolute {
                l
            } else {
                l + frame_level
            }
        } else {
            frame_level
        };
        if self.lf.enabled {
            level += self.lf.refs[mb.ref_frame as usize];
            level += self.lf.mode[mb.mode as usize];
        }
        let level = level.clamp(0, 63);
        let mut inner = level;
        if sharpness != 0 {
            inner >>= (sharpness + 3) >> 2;
            inner = inner.min(9 - sharpness);
        }
        FilterStrength { level, inner_limit: inner.max(1), inner: !mb.skip || mb.mode == MODE_I4X4 || mb.mode == MV_SPLIT }
    }

    fn loop_filter(&self, cur: &mut Picture, strengths: &[FilterStrength], simple: bool, keyframe: bool) {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        for mb_y in 0..self.mbh {
            for mb_x in 0..self.mbw {
                let f = strengths[mb_y * self.mbw + mb_x];
                if f.level == 0 {
                    continue;
                }
                let yo = mb_y * 16 * ys + mb_x * 16;
                let co = mb_y * 8 * cs + mb_x * 8;
                let bedge = f.level * 2 + f.inner_limit;
                let mbedge = bedge + 4;
                if simple {
                    if mb_x > 0 {
                        dsp::loop_filter_simple(&mut cur.y, yo, 1, ys, mbedge);
                    }
                    if f.inner {
                        for d in [4, 8, 12] {
                            dsp::loop_filter_simple(&mut cur.y, yo + d, 1, ys, bedge);
                        }
                    }
                    if mb_y > 0 {
                        dsp::loop_filter_simple(&mut cur.y, yo, ys, 1, mbedge);
                    }
                    if f.inner {
                        for d in [4, 8, 12] {
                            dsp::loop_filter_simple(&mut cur.y, yo + d * ys, ys, 1, bedge);
                        }
                    }
                    continue;
                }
                let hev = HEV_THRESH[keyframe as usize][f.level as usize] as i32;
                let i = f.inner_limit;
                if mb_x > 0 {
                    dsp::loop_filter(&mut cur.y, yo, 1, ys, 16, mbedge, i, hev, true);
                    dsp::loop_filter(&mut cur.u, co, 1, cs, 8, mbedge, i, hev, true);
                    dsp::loop_filter(&mut cur.v, co, 1, cs, 8, mbedge, i, hev, true);
                }
                if f.inner {
                    for d in [4, 8, 12] {
                        dsp::loop_filter(&mut cur.y, yo + d, 1, ys, 16, bedge, i, hev, false);
                    }
                    dsp::loop_filter(&mut cur.u, co + 4, 1, cs, 8, bedge, i, hev, false);
                    dsp::loop_filter(&mut cur.v, co + 4, 1, cs, 8, bedge, i, hev, false);
                }
                if mb_y > 0 {
                    dsp::loop_filter(&mut cur.y, yo, ys, 1, 16, mbedge, i, hev, true);
                    dsp::loop_filter(&mut cur.u, co, cs, 1, 8, mbedge, i, hev, true);
                    dsp::loop_filter(&mut cur.v, co, cs, 1, 8, mbedge, i, hev, true);
                }
                if f.inner {
                    for d in [4, 8, 12] {
                        dsp::loop_filter(&mut cur.y, yo + d * ys, ys, 1, 16, bedge, i, hev, false);
                    }
                    dsp::loop_filter(&mut cur.u, co + 4 * cs, cs, 1, 8, bedge, i, hev, false);
                    dsp::loop_filter(&mut cur.v, co + 4 * cs, cs, 1, 8, bedge, i, hev, false);
                }
            }
        }
    }
}

impl Default for Vp8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for Vp8Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .decode_frame(&packet.data)?
            .map(|mut v| {
                v.pts = packet.pts;
                Frame::Video(v)
            })
            .into_iter()
            .collect())
    }
}
