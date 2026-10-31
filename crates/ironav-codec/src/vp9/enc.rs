//! A software VP9 encoder for profile 0 (8-bit 4:2:0).
//!
//! A key frame every `keyint` frames (250 by default), and inter frames between them. An inter frame predicts
//! from the frame before (LAST) and refreshes only that reference. One tile, 4x4 transforms, the regular 8-tap
//! filter, single reference, no probability updates and quarter-pixel motion vectors. An open-loop search on
//! the source splits each 64x64 superblock down to 8x8 blocks; each block then takes the cheapest of intra
//! (8x8 only), ZEROMV, NEARESTMV, NEARMV and NEWMV by a rate-distortion cost. The encoder reconstructs with the
//! decoder's own prediction, transform and loop filter code, so its pictures are the decoder's.

use std::sync::Arc;

use super::bad;
use super::dec::*;
use super::header::*;
use super::intrapred;
use super::itx;
use super::tables::*;
use crate::vp8::enc::boolenc::BoolEncoder;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use std::f64::consts::PI;

/// The quantiser when none is set (the H.264 encoder's default).
const DEFAULT_QP: u8 = 23;

/// Frames from one key frame to the next, when none is set.
const DEFAULT_KEYINT: u64 = 250;

/// The coefficient magnitude limit: the largest value the token tree codes.
const MAX_LEVEL: i32 = 67 + (1 << 14) - 1;

/// The probabilities of the 14 extra bits of the largest token category, most significant first.
const CAT6_PROBS: [u8; 14] = [254, 254, 254, 252, 249, 243, 230, 196, 177, 153, 140, 133, 130, 129];

/// Motion search steps: the eight neighbours of a position.
const NEIGHBOURS: [(i32, i32); 8] = [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1), (-1, 1), (1, -1)];

/// The open-loop cost of a block beyond its error, which makes splitting a block cost something.
const BLOCK_COST: u64 = 24;

/// The base quantiser index for a quantiser of 0 to 51 (as for H.264). Index 0 would be lossless.
fn base_q(qp: u8) -> i32 {
    (qp.min(51) as i32 * 255 / 51).max(1)
}

/// The source picture, extended to whole superblocks, and the dequantisers [dc, ac] of luma and chroma.
struct Input {
    planes: [Vec<i32>; 3],
    stride: [usize; 3],
    dq: [i32; 2],
}

pub struct Vp9Encoder {
    width: usize,
    height: usize,
    q: i32,
    keyint: u64,
    /// Frames since the key frame that started the group, the key frame included (0 before the first).
    since_key: u64,
    /// The source of the previous frame, for the open-loop search.
    prev_src: Option<Input>,
    /// The last reconstruction (the LAST reference) and the last key frame (GOLDEN and ALTREF).
    last: Option<Arc<Pic>>,
    key: Option<Arc<Pic>>,
}

impl Vp9Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, 0-51 as for H.264). A bit
    /// rate is not followed: the quantiser stays where `qp` puts it.
    pub fn new(params: &StreamParams, qp: Option<u8>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, .. } = *params else {
            return Err(Error::invalid_arg("the VP9 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software VP9 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        if width == 0 || height == 0 || width > 16383 || height > 16383 {
            return Err(Error::invalid_arg(format!("{width}x{height} is not a VP9 picture size")));
        }
        let q = base_q(qp.unwrap_or(DEFAULT_QP));
        Ok(Self { width: width as usize, height: height as usize, q, keyint: DEFAULT_KEYINT, since_key: 0, prev_src: None, last: None, key: None })
    }

    /// Key frames every `n` frames (1 makes every frame a key frame).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    fn input(&self, v: &VideoFrame) -> Result<Input> {
        let (w, h) = (self.width, self.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the VP9 encoder was set up for {w}x{h} yuv420p frames")));
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if v.planes[0].len() < w * h || v.planes[1].len() < cw * ch || v.planes[2].len() < cw * ch {
            return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
        }
        let (bw, bh) = (w.div_ceil(64) * 64, h.div_ceil(64) * 64);
        // samples beyond the picture repeat its edge
        let pad = |p: &[u8], pw: usize, ph: usize, bw: usize, bh: usize| -> Vec<i32> {
            let mut out = Vec::with_capacity(bw * bh);
            for y in 0..bh {
                let row = &p[y.min(ph - 1) * pw..][..pw];
                out.extend((0..bw).map(|x| row[x.min(pw - 1)] as i32));
            }
            out
        };
        let planes = [pad(&v.planes[0], w, h, bw, bh), pad(&v.planes[1], cw, ch, bw / 2, bh / 2), pad(&v.planes[2], cw, ch, bw / 2, bh / 2)];
        let q = self.q as usize;
        let dq = [DC_QLOOKUP[0][q] as i32, AC_QLOOKUP[0][q] as i32];
        Ok(Input { planes, stride: [bw, bw / 2, bw / 2], dq })
    }

    /// The loop filter level: a function of the quantiser, sharpness 0.
    fn lf_level(&self) -> u8 {
        (self.q / 8).min(63) as u8
    }

    /// The header of a frame, as the decoder will read it from the bits written by `uncompressed`.
    fn header(&self, key: bool) -> FrameHeader {
        let lf = self.lf_level();
        let mut hdr = FrameHeader {
            keyframe: key,
            width: self.width,
            height: self.height,
            ss_h: true,
            ss_v: true,
            bpp: 8,
            lf_level: lf,
            refreshrefmask: if key { 0xff } else { 1 },
            refidx: [0, 1, 2],
            use_last_frame_mvs: !key,
            filtermode: 1,
            ..Default::default()
        };
        for f in hdr.seg.feat.iter_mut() {
            f.lflvl = [[lf; 2]; 4];
        }
        hdr
    }

    /// Codes one key frame. Returns the frame and its reconstruction.
    fn code_key(&self, inp: &Input) -> Result<(Vec<u8>, Pic)> {
        let (w, h) = (self.width, self.height);
        let lf = self.lf_level();
        let mut hdr = self.header(true);
        let comp = compressed(true);
        let (probs, coef) = parse_compressed(&comp, &mut hdr, &HeaderState::new()).map_err(bad)?;

        let mut d = Dec::new(hdr, probs, coef, Pic::new(w, h, true, true, 8, 0));
        d.above_mode.fill(DC_PRED);
        let mut e = BoolEncoder::default();
        e.put(128, false);
        let mut row = 0;
        while row < d.rows {
            d.left_partition = [0; 8];
            d.left_skip = [0; 8];
            d.left_mode = [DC_PRED; 16];
            d.left_y_nnz = [0; 16];
            d.left_uv_nnz = [[0; 16]; 2];
            for col in (0..d.cols).step_by(8) {
                encode_sb(&mut d, &mut e, inp, row, col, 0);
            }
            row += 8;
        }
        if lf > 0 {
            d.loopfilter(0);
        }
        let data = frame_bytes(uncompressed(true, w, h, self.q, lf, comp.len()), &comp, e.finish());
        Ok((data, d.cur))
    }

    /// Codes one inter frame from the last reconstruction. Returns the frame and its reconstruction.
    fn code_inter(&self, inp: &Input) -> Result<(Vec<u8>, Pic)> {
        let (Some(prev), Some(last), Some(key)) = (self.prev_src.as_ref(), self.last.clone(), self.key.clone()) else {
            return Err(Error::invalid_input("the VP9 encoder has no reference for an inter frame"));
        };
        let (w, h) = (self.width, self.height);
        let lf = self.lf_level();
        let mut hdr = self.header(false);
        let comp = compressed(false);
        let (probs, coef) = parse_compressed(&comp, &mut hdr, &HeaderState::new()).map_err(bad)?;

        let mut d = Dec::new(hdr, probs, coef, Pic::new(w, h, true, true, 8, 0));
        d.refs = [Some(last.clone()), Some(key.clone()), Some(key)];
        d.prev = Some(last);
        d.above_mode.fill(NEARESTMV);
        let o = Open { cur: inp, prev, w, h, cols: d.cols, rows: d.rows };
        let mut plan = Plan::new(d.cols, d.rows);
        for row in (0..d.rows).step_by(8) {
            for col in (0..d.cols).step_by(8) {
                plan_node(&o, row, col, 0, (0, 0), &mut plan);
            }
        }

        let mut e = BoolEncoder::default();
        e.put(128, false);
        let mut row = 0;
        while row < d.rows {
            d.left_partition = [0; 8];
            d.left_skip = [0; 8];
            d.left_mode = [NEARESTMV; 16];
            d.left_y_nnz = [0; 16];
            d.left_uv_nnz = [[0; 16]; 2];
            for col in (0..d.cols).step_by(8) {
                encode_sb_inter(&mut d, &mut e, inp, &plan, row, col, 0);
            }
            row += 8;
        }
        if lf > 0 {
            d.loopfilter(0);
        }
        let data = frame_bytes(uncompressed(false, w, h, self.q, lf, comp.len()), &comp, e.finish());
        Ok((data, d.cur))
    }
}

impl Encoder for Vp9Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the VP9 encoder received an audio frame"));
        };
        let inp = self.input(v)?;
        let key = self.last.is_none() || self.since_key >= self.keyint;
        let (data, recon) = if key { self.code_key(&inp)? } else { self.code_inter(&inp)? };
        let pic = Arc::new(recon);
        if key {
            self.key = Some(pic.clone());
        }
        self.last = Some(pic);
        self.since_key = if key { 1 } else { self.since_key + 1 };
        self.prev_src = Some(inp);
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
}

/// The data of a frame: its uncompressed header, compressed header and tile, with a zero byte after a last
/// byte that would read as a superframe index.
fn frame_bytes(mut data: Vec<u8>, comp: &[u8], tile: Vec<u8>) -> Vec<u8> {
    data.extend_from_slice(comp);
    data.extend(tile);
    if data.last().is_some_and(|&b| b & 0xe0 == 0xc0) {
        data.push(0);
    }
    data
}

/// Bits, most significant first, packed into bytes at the end.
#[derive(Default)]
struct BitWriter(Vec<bool>);

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.0.push((v >> i) & 1 == 1);
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |acc, (i, &b)| acc | ((b as u8) << (7 - i)))).collect()
    }
}

/// The uncompressed header of a profile 0 frame of `w` x `h` at base quantiser `q` and loop filter level `lf`.
/// An inter frame refreshes LAST only, takes its size from it, and uses the regular 8-tap filter.
fn uncompressed(key: bool, w: usize, h: usize, q: i32, lf: u8, comp_len: usize) -> Vec<u8> {
    let mut b = BitWriter::default();
    b.put(2, 2); // frame marker
    b.put(0, 2); // profile 0
    b.put(0, 1); // not show_existing_frame
    b.put(!key as u32, 1); // frame type
    b.put(1, 1); // shown
    b.put(0, 1); // not error resilient
    if key {
        b.put(0x49_83_42, 24); // sync code
        b.put(0, 3); // colour space: unknown
        b.put(0, 1); // studio range
        b.put(w as u32 - 1, 16);
        b.put(h as u32 - 1, 16);
        b.put(0, 1); // render size is the frame size
    } else {
        b.put(0, 2); // no reset of the frame contexts
        b.put(1, 8); // refresh LAST, in slot 0
        for slot in 0..3 {
            b.put(slot, 3); // LAST, GOLDEN and ALTREF
            b.put(0, 1); // sign bias
        }
        b.put(1, 1); // the size of LAST
        b.put(0, 1); // render size is the frame size
        b.put(0, 1); // no high precision motion vectors
        b.put(0, 1); // filter not switchable
        b.put(1, 2); // the regular filter
    }
    b.put(0, 1); // no refresh of the frame context
    b.put(1, 1); // parallel decoding: no adaptation
    b.put(0, 2); // frame context 0
    b.put(lf as u32, 6);
    b.put(0, 3); // sharpness
    b.put(0, 1); // no loop filter deltas
    b.put(q as u32, 8);
    b.put(0, 3); // no quantiser deltas
    b.put(0, 1); // no segmentation
    let mut m = 0;
    while (w.div_ceil(64) >> m) >= 4 {
        m += 1;
    }
    if m > 1 {
        b.put(0, 1); // one tile column
    }
    b.put(0, 1); // one tile row
    b.put(comp_len as u32, 16);
    b.into_bytes()
}

/// The compressed header: the transform mode and the coefficient update flags, then (inter frames) the update
/// flags of the mode, reference, partition and motion vector probabilities. No probability is updated.
fn compressed(key: bool) -> Vec<u8> {
    let mut ce = BoolEncoder::default();
    ce.put(128, false);
    ce.literal(0, 2); // tx_mode ONLY_4X4
    ce.put(128, false); // no coefficient updates for 4x4
    for _ in 0..3 {
        ce.put(252, false); // no skip probability updates
    }
    if !key {
        for _ in 0..184 {
            ce.put(252, false); // no inter mode, reference, intra mode, partition or motion vector updates
        }
    }
    ce.finish()
}

/// Codes the superblock or block at (`row`, `col`) of size 2^(3 - `bl`) 8x8 blocks, as the decoder's
/// `decode_sb`: always split, so every block is 8x8. Key frames only.
fn encode_sb(d: &mut Dec, e: &mut BoolEncoder, inp: &Input, row: usize, col: usize, bl: usize) {
    let c = (((d.above_partition[col] >> (3 - bl)) & 1) as usize) | ((((d.left_partition[row & 7] >> (3 - bl)) & 1) as usize) << 1);
    let p = DEFAULT_KF_PARTITION_PROBS[bl][c];
    let hbs = 4usize >> bl;
    if bl == 3 {
        e.tree(&PARTITION_TREE, &p, 0);
        encode_block(d, e, inp, row, col);
    } else if col + hbs < d.cols {
        if row + hbs < d.rows {
            e.tree(&PARTITION_TREE, &p, 3);
            for (r, cc) in [(row, col), (row, col + hbs), (row + hbs, col), (row + hbs, col + hbs)] {
                encode_sb(d, e, inp, r, cc, bl + 1);
            }
        } else {
            e.put(p[1], true);
            encode_sb(d, e, inp, row, col, bl + 1);
            encode_sb(d, e, inp, row, col + hbs, bl + 1);
        }
    } else if row + hbs < d.rows {
        e.put(p[2], true);
        encode_sb(d, e, inp, row, col, bl + 1);
        encode_sb(d, e, inp, row + hbs, col, bl + 1);
    } else {
        encode_sb(d, e, inp, row, col, bl + 1);
    }
}

/// Codes one key frame 8x8 block: picks its modes, writes the skip flag, the modes and the tokens with the
/// decoder's contexts, and leaves the reconstruction in the picture.
fn encode_block(d: &mut Dec, e: &mut BoolEncoder, inp: &Input, row: usize, col: usize) {
    set_block(d, row, col, 9);
    d.b.intra = true;
    let (row7, ly) = (row & 7, (row & 7) * 2);
    let stride = d.cur.stride[0];
    let (px0, py0) = (col * 8, row * 8);

    // luma: each mode reconstructs the block in turn; the one with the least SAD stays
    let mut best: Option<(u64, u8, [[i32; 16]; 4], [usize; 4], Vec<u16>)> = None;
    for mode in [intrapred::VERT_PRED, intrapred::HOR_PRED, DC_PRED, intrapred::TM_VP8_PRED] {
        let (lv, eobs, sad) = code_luma(d, inp, mode);
        if best.as_ref().is_none_or(|b| sad < b.0) {
            let region: Vec<u16> = (0..8).flat_map(|y| d.cur.planes[0][(py0 + y) * stride + px0..][..8].to_vec()).collect();
            best = Some((sad, mode, lv, eobs, region));
        }
    }
    let (_, mode, lv, eobs, region) = best.expect("four modes were tried");
    for y in 0..8 {
        d.cur.planes[0][(py0 + y) * stride + px0..][..8].copy_from_slice(&region[y * 8..y * 8 + 8]);
    }
    let uv = [code_chroma(d, inp, 0), code_chroma(d, inp, 1)];

    let skip = eobs.iter().all(|&n| n == 0) && uv.iter().all(|u| u.1 == 0);
    d.b.skip = skip;
    d.b.mode = [mode; 4];
    let ctx = (d.left_skip[row7] + d.above_skip[col]) as usize;
    e.put(d.p.skip[ctx], skip);
    d.above_skip[col] = skip as u8;
    d.left_skip[row7] = skip as u8;

    let (am, lm) = (d.above_mode[col * 2] as usize, d.left_mode[ly] as usize);
    e.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[am][lm], mode);
    d.above_mode[col * 2] = mode;
    d.above_mode[col * 2 + 1] = mode;
    d.left_mode[ly] = mode;
    d.left_mode[ly + 1] = mode;
    e.tree(&INTRAMODE_TREE, &DEFAULT_KF_UVMODE_PROBS[mode as usize], DC_PRED);
    d.above_partition[col] = ABOVE_CTX[9];
    d.left_partition[row7] = LEFT_CTX[9];

    if skip {
        d.above_y_nnz[col * 2..col * 2 + 2].fill(0);
        d.left_y_nnz[ly..ly + 2].fill(0);
        for pl in 0..2 {
            d.above_uv_nnz[pl][col] = 0;
            d.left_uv_nnz[pl][row7] = 0;
        }
        d.lf_masks();
        return;
    }

    let txtp = INTRA_TXFM_TYPE[mode as usize] as usize;
    let mut a = [0u8; 20];
    let mut l = [0u8; 20];
    a[..16].copy_from_slice(&d.above_y_nnz[col * 2..col * 2 + 16]);
    l[..16 - ly].copy_from_slice(&d.left_y_nnz[ly..16]);
    for n in 0..4 {
        let (x, y) = (n & 1, n >> 1);
        let nnz = (a[x] + l[y]) as usize;
        write_tokens(e, &lv[n], eobs[n], nnz, SCANS[0][txtp], SCANS_NB[0][txtp], &d.coef[0][0][0]);
        a[x] = (eobs[n] > 0) as u8;
        l[y] = a[x];
    }
    d.above_y_nnz[col * 2..col * 2 + 16].copy_from_slice(&a[..16]);
    d.left_y_nnz[ly..16].copy_from_slice(&l[..16 - ly]);

    for pl in 0..2 {
        let mut a = [0u8; 20];
        let mut l = [0u8; 20];
        a[..16].copy_from_slice(&d.above_uv_nnz[pl][col..col + 16]);
        l[..16 - row7].copy_from_slice(&d.left_uv_nnz[pl][row7..16]);
        let nnz = (a[0] + l[0]) as usize;
        write_tokens(e, &uv[pl].0, uv[pl].1, nnz, SCANS[0][0], SCANS_NB[0][0], &d.coef[0][1][0]);
        a[0] = (uv[pl].1 > 0) as u8;
        l[0] = a[0];
        d.above_uv_nnz[pl][col..col + 16].copy_from_slice(&a[..16]);
        d.left_uv_nnz[pl][row7..16].copy_from_slice(&l[..16 - row7]);
    }
    d.lf_masks();
}

/// Predicts and reconstructs the four luma 4x4 blocks of the block at `d`'s position with `mode`.
/// Returns their levels and end-of-block positions and the SAD of the reconstruction.
fn code_luma(d: &mut Dec, inp: &Input, mode: u8) -> ([[i32; 16]; 4], [usize; 4], u64) {
    let txtp = INTRA_TXFM_TYPE[mode as usize] as usize;
    let (row, col) = (d.row, d.col);
    let stride = d.cur.stride[0];
    let mut lv = [[0i32; 16]; 4];
    let mut eobs = [0usize; 4];
    let mut sad = 0u64;
    let (mut top, mut left) = ([0u16; 65], [0u16; 32]);
    for n in 0..4 {
        let (x, y) = (n & 1, n >> 1);
        let (px, py) = (col * 8 + x * 4, row * 8 + y * 4);
        let pm = d.intra_edges(0, mode, px, py, 0, x, y, 2, &mut top, &mut left);
        intrapred::predict(pm, 0, &mut d.cur.planes[0][py * stride + px..], stride, &top, &left, 8);
        let (l, eob) = residual4(d, inp, 0, px, py, txtp);
        lv[n] = l;
        eobs[n] = eob;
        if eob > 0 {
            add4(d, inp, 0, px, py, &l, eob, txtp);
        }
        for j in 0..4 {
            for i in 0..4 {
                sad += (inp.planes[0][(py + j) * inp.stride[0] + px + i] - d.cur.planes[0][(py + j) * stride + px + i] as i32).unsigned_abs() as u64;
            }
        }
    }
    (lv, eobs, sad)
}

/// Predicts (DC) and reconstructs the 4x4 block of chroma plane `pl` of the block at `d`'s position.
fn code_chroma(d: &mut Dec, inp: &Input, pl: usize) -> ([i32; 16], usize) {
    let (px, py) = (d.col * 4, d.row * 4);
    let stride = d.cur.stride[1 + pl];
    let (mut top, mut left) = ([0u16; 65], [0u16; 32]);
    let pm = d.intra_edges(1 + pl, DC_PRED, px, py, 0, 0, 0, 1, &mut top, &mut left);
    intrapred::predict(pm, 0, &mut d.cur.planes[1 + pl][py * stride + px..], stride, &top, &left, 8);
    let (l, eob) = residual4(d, inp, 1 + pl, px, py, 0);
    if eob > 0 {
        add4(d, inp, 1 + pl, px, py, &l, eob, 0);
    }
    (l, eob)
}

/// The quantised residual of the 4x4 block of plane `pl` at (`px`, `py`) against the picture's prediction:
/// its levels and end of block.
fn residual4(d: &Dec, inp: &Input, pl: usize, px: usize, py: usize, txtp: usize) -> ([i32; 16], usize) {
    let stride = d.cur.stride[pl];
    let mut r = [0f64; 16];
    for j in 0..4 {
        for i in 0..4 {
            let s = inp.planes[pl][(py + j) * inp.stride[pl] + px + i];
            r[j * 4 + i] = (s - d.cur.planes[pl][(py + j) * stride + px + i] as i32) as f64;
        }
    }
    quantize(&fdct4(&r, txtp), inp.dq, SCANS[0][txtp])
}

/// Adds the inverse transform of levels `lv` (of transform type `txtp`) to the 4x4 block at (`px`, `py`).
#[allow(clippy::too_many_arguments)]
fn add4(d: &mut Dec, inp: &Input, pl: usize, px: usize, py: usize, lv: &[i32; 16], eob: usize, txtp: usize) {
    let stride = d.cur.stride[pl];
    let mut coef = dequant(lv, inp.dq);
    itx::inverse_add(0, txtp, &mut coef, &mut d.cur.planes[pl][py * stride + px..], stride, eob, 8);
}

/// The orthonormal 4-point bases [frequency][sample] of the DCT, or of VP9's ADST (a DST-VII), scaled
/// as the decoder's inverse transforms are, up to the common factor in `fdct4`.
fn basis(adst: bool) -> [[f64; 4]; 4] {
    let mut b = [[0.0; 4]; 4];
    for (k, row) in b.iter_mut().enumerate() {
        for (y, v) in row.iter_mut().enumerate() {
            *v = if adst {
                (2.0 / 3.0) * (PI * ((2 * k + 1) * (y + 1)) as f64 / 9.0).sin()
            } else {
                let s = if k == 0 { 0.5 } else { 0.5f64.sqrt() };
                s * (PI * ((2 * y + 1) * k) as f64 / 8.0).cos()
            };
        }
    }
    b
}

/// The forward 4x4 transform of the residual `r` (raster order) for transform type `txtp`, in the
/// decoder's convention: its first pass (ADST for types 1 and 3) runs over each row of the samples,
/// giving the first index of the coefficients, and its second (ADST for types 2 and 3) over the columns.
/// The scale is 8 times the orthonormal transform, which is what the decoder's inverse undoes.
fn fdct4(r: &[f64; 16], txtp: usize) -> [f64; 16] {
    let first = basis(txtp == 1 || txtp == 3);
    let second = basis(txtp == 2 || txtp == 3);
    let mut t = [0.0; 16];
    for kr in 0..4 {
        for y in 0..4 {
            t[kr * 4 + y] = (0..4).map(|x| first[kr][x] * r[y * 4 + x]).sum();
        }
    }
    let mut c = [0.0; 16];
    for kr in 0..4 {
        for kc in 0..4 {
            c[kr * 4 + kc] = 8.0 * (0..4).map(|y| second[kc][y] * t[kr * 4 + y]).sum::<f64>();
        }
    }
    c
}

/// Quantises `c` (raster order) with the dequantisers `dq`. Returns the levels and the end of block:
/// one past the last non-zero level in `scan` order, 0 when all are zero.
fn quantize(c: &[f64; 16], dq: [i32; 2], scan: &[i16]) -> ([i32; 16], usize) {
    let mut lv = [0i32; 16];
    for (rc, l) in lv.iter_mut().enumerate() {
        *l = ((c[rc] / dq[(rc != 0) as usize] as f64).round() as i32).clamp(-MAX_LEVEL, MAX_LEVEL);
    }
    let eob = scan.iter().rposition(|&rc| lv[rc as usize] != 0).map_or(0, |i| i + 1);
    (lv, eob)
}

/// The dequantised coefficients of `lv`, as the decoder's token reader makes them.
fn dequant(lv: &[i32; 16], dq: [i32; 2]) -> [i32; 16] {
    let mut c = [0i32; 16];
    for (rc, v) in c.iter_mut().enumerate() {
        *v = lv[rc] * dq[(rc != 0) as usize];
    }
    c
}

/// Writes the tokens of one 4x4 block with levels `lv` and end of block `eob`: the inverse of the
/// decoder's `coeffs_b`, with the same scan, neighbour contexts, bands and token tree.
fn write_tokens(e: &mut BoolEncoder, lv: &[i32; 16], eob: usize, mut nnz: usize, scan: &[i16], nb: &[[i16; 2]], p: &[[[u8; 11]; 6]; 6]) {
    if eob == 0 {
        e.put(p[0][nnz][0], false);
        return;
    }
    let bc = &BAND_COUNTS[0];
    let ctx = |cache: &[u8; 16], i: usize| (1 + cache[nb[i][0] as usize] as usize + cache[nb[i][1] as usize] as usize) >> 1;
    let mut cache = [0u8; 16];
    let (mut band, mut band_left) = (0usize, bc[0] as i32);
    let mut i = 0usize;
    loop {
        let mut tp = p[band][nnz];
        e.put(tp[0], true);
        while lv[scan[i] as usize] == 0 {
            e.put(tp[1], false);
            band_left -= 1;
            if band_left == 0 {
                band += 1;
                band_left = bc[band] as i32;
            }
            cache[scan[i] as usize] = 0;
            nnz = ctx(&cache, i);
            i += 1;
            tp = p[band][nnz];
        }
        e.put(tp[1], true);
        let rc = scan[i] as usize;
        cache[rc] = write_magnitude(e, &tp, lv[rc].unsigned_abs());
        e.put(128, lv[rc] < 0);
        band_left -= 1;
        if band_left == 0 {
            band += 1;
            band_left = bc[band] as i32;
        }
        nnz = ctx(&cache, i);
        i += 1;
        if i == 16 {
            break;
        }
        if i == eob {
            e.put(p[band][nnz][0], false);
            break;
        }
    }
}

/// Writes the magnitude `v` (at least 1) below a non-zero token, with the decoder's token tree and
/// probabilities `tp`. Returns the token class the decoder records as a neighbour context.
fn write_magnitude(e: &mut BoolEncoder, tp: &[u8; 11], v: u32) -> u8 {
    if v == 1 {
        e.put(tp[2], false);
        return 1;
    }
    e.put(tp[2], true);
    if v <= 4 {
        e.put(tp[3], false);
        if v == 2 {
            e.put(tp[4], false);
            return 2;
        }
        e.put(tp[4], true);
        e.put(tp[5], v == 4);
        return 3;
    }
    e.put(tp[3], true);
    if v <= 10 {
        e.put(tp[6], false);
        if v <= 6 {
            e.put(tp[7], false);
            e.put(159, v == 6);
        } else {
            e.put(tp[7], true);
            let w = v - 7;
            e.put(165, w & 2 != 0);
            e.put(145, w & 1 != 0);
        }
        return 4;
    }
    e.put(tp[6], true);
    if v <= 18 {
        e.put(tp[8], false);
        e.put(tp[9], false);
        let w = v - 11;
        e.put(173, (w >> 2) & 1 == 1);
        e.put(148, (w >> 1) & 1 == 1);
        e.put(140, w & 1 == 1);
    } else if v <= 34 {
        e.put(tp[8], false);
        e.put(tp[9], true);
        let w = v - 19;
        e.put(176, (w >> 3) & 1 == 1);
        e.put(155, (w >> 2) & 1 == 1);
        e.put(140, (w >> 1) & 1 == 1);
        e.put(135, w & 1 == 1);
    } else if v <= 66 {
        e.put(tp[8], true);
        e.put(tp[10], false);
        let w = v - 35;
        for (bit, prob) in [(4, 180), (3, 157), (2, 141), (1, 134), (0, 130)] {
            e.put(prob, (w >> bit) & 1 == 1);
        }
    } else {
        e.put(tp[8], true);
        e.put(tp[10], true);
        let w = v - 67;
        for (i, &prob) in CAT6_PROBS.iter().enumerate() {
            e.put(prob, (w >> (13 - i)) & 1 == 1);
        }
    }
    5
}

// Inter frames

/// The open-loop search's inputs: the current and previous source, and the frame size.
struct Open<'a> {
    cur: &'a Input,
    prev: &'a Input,
    w: usize,
    h: usize,
    cols: usize,
    rows: usize,
}

/// The block size and motion decided open-loop: `lvl` is the level of the block covering each 8x8 cell, `mv`
/// its whole-pixel motion against the previous source.
struct Plan {
    cols: usize,
    lvl: Vec<u8>,
    mv: Vec<(i32, i32)>,
}

impl Plan {
    fn new(cols: usize, rows: usize) -> Self {
        Plan { cols, lvl: vec![0; cols * rows], mv: vec![(0, 0); cols * rows] }
    }

    /// Makes the `n` x `n` cells at (`row`, `col`) (clipped to the frame) one block of level `bl`.
    fn mark(&mut self, row: usize, col: usize, n: usize, bl: usize, mv: (i32, i32), rows: usize) {
        for r in row..(row + n).min(rows) {
            for c in col..(col + n).min(self.cols) {
                self.lvl[r * self.cols + c] = bl as u8;
                self.mv[r * self.cols + c] = mv;
            }
        }
    }
}

/// The open-loop cost of the `size` x `size` block at (`x`, `y`) of the source against the previous source: the
/// better of zero and a whole-pixel search from `start`, with its motion.
fn open_loop(o: &Open, x: usize, y: usize, size: usize, start: (i32, i32)) -> (u64, (i32, i32)) {
    let sad = |dx: i32, dy: i32| -> u64 {
        let mut s = 0u64;
        for j in 0..size {
            let ry = (y as i32 + j as i32 + dy).clamp(0, o.h as i32 - 1) as usize;
            for i in 0..size {
                let rx = (x as i32 + i as i32 + dx).clamp(0, o.w as i32 - 1) as usize;
                let c = o.cur.planes[0][(y + j) * o.cur.stride[0] + x + i];
                let p = o.prev.planes[0][ry * o.prev.stride[0] + rx];
                s += (c - p).unsigned_abs() as u64;
            }
        }
        s
    };
    let mut best = (0, 0);
    let mut bc = sad(0, 0);
    if start != (0, 0) {
        let c = sad(start.0, start.1);
        if c < bc {
            best = start;
            bc = c;
        }
    }
    for step in [4, 2, 1] {
        for _ in 0..8 {
            let mut moved = false;
            for (dx, dy) in NEIGHBOURS {
                let m = (best.0 + dx * step, best.1 + dy * step);
                let c = sad(m.0, m.1);
                if c < bc {
                    bc = c;
                    best = m;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
    (bc + BLOCK_COST, best)
}

/// Decides the block sizes of the `1 << (3 - bl)`-cell node at (`row`, `col`): either one block, or the best
/// of its four children. Returns the cost of the decision.
fn plan_node(o: &Open, row: usize, col: usize, bl: usize, start: (i32, i32), p: &mut Plan) -> u64 {
    if row >= o.rows || col >= o.cols {
        return 0;
    }
    let n = 8 >> bl;
    let hbs = 4 >> bl;
    let (cost, mv) = open_loop(o, col * 8, row * 8, n * 8, start);
    if bl == 3 {
        p.mark(row, col, n, bl, mv, o.rows);
        return cost;
    }
    let mut split = 0;
    for (r, c) in [(row, col), (row, col + hbs), (row + hbs, col), (row + hbs, col + hbs)] {
        split += plan_node(o, r, c, bl + 1, mv, p);
    }
    if col + hbs < o.cols && row + hbs < o.rows && cost <= split {
        p.mark(row, col, n, bl, mv, o.rows);
        cost
    } else {
        split
    }
}

/// The block at (`row`, `col`) of size `bs` as the decoder's `decode_block` sets it up before reading it.
fn set_block(d: &mut Dec, row: usize, col: usize, bs: usize) {
    let (w4, h4) = (BWH_TAB[1][bs][0] as i32, BWH_TAB[1][bs][1] as i32);
    d.row = row;
    d.col = col;
    d.b = Blk { bs, bl: bs / 3, bp: bs % 3, filter: 1, ..Default::default() };
    d.min_mv = Mv32 { x: -(128 + col as i32 * 64), y: -(128 + row as i32 * 64) };
    d.max_mv = Mv32 { x: 128 + (d.cols as i32 - col as i32 - w4) * 64, y: 128 + (d.rows as i32 - row as i32 - h4) * 64 };
}

/// The pixels of the block `bs` at the current position: (x, y, width, height) of luma.
fn region(d: &Dec, bs: usize) -> (usize, usize, usize, usize) {
    (d.col * 8, d.row * 8, BWH_TAB[0][bs][0] as usize * 4, BWH_TAB[0][bs][1] as usize * 4)
}

/// The luma and chroma (rows of the picture) of the block, saved and restored around candidate modes.
fn save(d: &Dec, bs: usize) -> [Vec<u16>; 3] {
    let (x, y, w, h) = region(d, bs);
    let mut out: [Vec<u16>; 3] = Default::default();
    for pl in 0..3 {
        let (x0, y0, w0, h0) = if pl == 0 { (x, y, w, h) } else { (x / 2, y / 2, w / 2, h / 2) };
        let s = d.cur.stride[pl];
        for j in 0..h0 {
            out[pl].extend_from_slice(&d.cur.planes[pl][(y0 + j) * s + x0..][..w0]);
        }
    }
    out
}

fn restore(d: &mut Dec, bs: usize, rec: &[Vec<u16>; 3]) {
    let (x, y, w, h) = region(d, bs);
    for pl in 0..3 {
        let (x0, y0, w0, h0) = if pl == 0 { (x, y, w, h) } else { (x / 2, y / 2, w / 2, h / 2) };
        let s = d.cur.stride[pl];
        for j in 0..h0 {
            let o = (y0 + j) * s + x0;
            d.cur.planes[pl][o..o + w0].copy_from_slice(&rec[pl][j * w0..(j + 1) * w0]);
        }
    }
}

/// The 4x4 residual counts of the block in frame: (luma across, luma down); chroma is half of each.
fn ends(d: &Dec, bs: usize) -> (usize, usize) {
    let w4 = 2 * BWH_TAB[1][bs][0] as usize;
    let h4 = 2 * BWH_TAB[1][bs][1] as usize;
    ((2 * (d.cols - d.col)).min(w4), (2 * (d.rows - d.row)).min(h4))
}

/// A coded candidate for a block: its modes, motion, residual, cost and reconstruction.
struct Cand {
    cost: f64,
    side: f64,
    intra: bool,
    mode: u8,
    uvmode: u8,
    mv: Mv,
    ly: Vec<[i32; 16]>,
    ey: Vec<usize>,
    lc: [Vec<[i32; 16]>; 2],
    ec: [Vec<usize>; 2],
    rec: [Vec<u16>; 3],
}

/// The bits a block's tokens take, roughly: a count per coded 4x4 block and per coefficient.
fn est_bits(lv: &[[i32; 16]], eob: &[usize]) -> f64 {
    lv.iter()
        .zip(eob)
        .map(|(l, &e)| if e == 0 { 0.5 } else { 4.0 + l.iter().map(|&v| if v == 0 { 0.6 } else { 3.0 + 2.0 * (v.unsigned_abs() as f64).log2() }).sum::<f64>() })
        .sum()
}

/// The rate-distortion cost of candidate `c` for the block at `d`'s position: squared error over the frame
/// area it covers, plus lambda times its bits.
fn rd_cost(d: &Dec, inp: &Input, c: &Cand) -> f64 {
    let (x, y, w, h) = region(d, d.b.bs);
    let mut dist = 0u64;
    for pl in 0..3 {
        let (x0, y0, w0, h0, lw, lh) = if pl == 0 { (x, y, w, h, d.cols * 8, d.rows * 8) } else { (x / 2, y / 2, w / 2, h / 2, d.cols * 4, d.rows * 4) };
        let s = d.cur.stride[pl];
        for j in y0..(y0 + h0).min(lh) {
            for i in x0..(x0 + w0).min(lw) {
                let e = inp.planes[pl][j * inp.stride[pl] + i] - d.cur.planes[pl][j * s + i] as i32;
                dist += (e * e) as u64;
            }
        }
    }
    let bits = c.side + est_bits(&c.ly, &c.ey) + est_bits(&c.lc[0], &c.ec[0]) + est_bits(&c.lc[1], &c.ec[1]);
    let lambda = (inp.dq[1] * inp.dq[1]) as f64 / 640.0;
    dist as f64 + lambda * bits
}

/// The residuals of the luma 4x4 blocks inside the frame of the current inter block, whose prediction is in
/// place: their levels and ends, in the decoder's order, after adding them to the picture.
fn inter_luma(d: &mut Dec, inp: &Input) -> (Vec<[i32; 16]>, Vec<usize>) {
    let (ex, ey) = ends(d, d.b.bs);
    let (row, col) = (d.row, d.col);
    let mut lv = Vec::with_capacity(ex * ey);
    let mut eobs = Vec::with_capacity(ex * ey);
    for y in 0..ey {
        for x in 0..ex {
            let (px, py) = (col * 8 + x * 4, row * 8 + y * 4);
            let (l, eob) = residual4(d, inp, 0, px, py, 0);
            if eob > 0 {
                add4(d, inp, 0, px, py, &l, eob, 0);
            }
            lv.push(l);
            eobs.push(eob);
        }
    }
    (lv, eobs)
}

/// The chroma residuals of the current inter block, as `inter_luma` makes them for both planes.
fn inter_chroma(d: &mut Dec, inp: &Input) -> ([Vec<[i32; 16]>; 2], [Vec<usize>; 2]) {
    let (ex, ey) = ends(d, d.b.bs);
    let (ex, ey) = (ex / 2, ey / 2);
    let (row, col) = (d.row, d.col);
    let mut lc: [Vec<[i32; 16]>; 2] = Default::default();
    let mut ec: [Vec<usize>; 2] = Default::default();
    for pl in 0..2 {
        for y in 0..ey {
            for x in 0..ex {
                let (px, py) = (col * 4 + x * 4, row * 4 + y * 4);
                let (l, eob) = residual4(d, inp, 1 + pl, px, py, 0);
                if eob > 0 {
                    add4(d, inp, 1 + pl, px, py, &l, eob, 0);
                }
                lc[pl].push(l);
                ec[pl].push(eob);
            }
        }
    }
    (lc, ec)
}

/// Predicts the current block from LAST with `mode` and `mv`, codes its residual, and returns the candidate.
/// `near` is the nearest candidate, which NEWMV codes its difference from.
fn eval_inter(d: &mut Dec, inp: &Input, mode: u8, mv: Mv, near: Mv) -> Cand {
    d.b.intra = false;
    d.b.mode = [mode; 4];
    d.b.mv = [[mv, Mv::default()]; 4];
    d.inter_pred();
    let (ly, ey) = inter_luma(d, inp);
    let (lc, ec) = inter_chroma(d, inp);
    let side = match mode {
        ZEROMV => 1.0,
        NEARESTMV => 2.0,
        NEARMV => 3.0,
        _ => 5.0 + mv_bits(mv, near),
    };
    let mut c = Cand { cost: 0.0, side, intra: false, mode, uvmode: DC_PRED, mv, ly, ey, lc, ec, rec: save(d, d.b.bs) };
    c.cost = rd_cost(d, inp, &c);
    c
}

/// Predicts the current block (8x8) with intra `mode` and DC chroma, codes its residual, and returns the candidate.
fn eval_intra(d: &mut Dec, inp: &Input, mode: u8) -> Cand {
    d.b.intra = true;
    d.b.mode = [mode; 4];
    d.b.uvmode = DC_PRED;
    d.b.mv = [[Mv::default(); 2]; 4];
    let (lv, eobs, _) = code_luma(d, inp, mode);
    let mut lc: [Vec<[i32; 16]>; 2] = Default::default();
    let mut ec: [Vec<usize>; 2] = Default::default();
    for pl in 0..2 {
        let (l, e) = code_chroma(d, inp, pl);
        lc[pl].push(l);
        ec[pl].push(e);
    }
    let mut c = Cand { cost: 0.0, side: 6.0, intra: true, mode, uvmode: DC_PRED, mv: Mv::default(), ly: lv.to_vec(), ey: eobs.to_vec(), lc, ec, rec: save(d, d.b.bs) };
    c.cost = rd_cost(d, inp, &c);
    c
}

/// Bits for a motion vector difference, roughly: two logarithms and a constant.
fn mv_bits(mv: Mv, base: Mv) -> f64 {
    let (dx, dy) = ((mv.x - base.x).unsigned_abs() as f64, (mv.y - base.y).unsigned_abs() as f64);
    2.0 * ((dx + 1.0).log2() + (dy + 1.0).log2()) + 2.0
}

/// The SAD of the luma prediction of LAST at `mv` against the source, over the block `reg`. The prediction is
/// left in the picture.
fn mv_sad(d: &mut Dec, inp: &Input, r: &Pic, reg: (usize, usize, usize, usize), mv: Mv) -> u64 {
    let (x, y, w, h) = reg;
    d.mc_l(r, false, x, y, mv, w, h);
    let s = d.cur.stride[0];
    let mut sad = 0u64;
    for j in 0..h {
        for i in 0..w {
            sad += (inp.planes[0][(y + j) * inp.stride[0] + x + i] - d.cur.planes[0][(y + j) * s + x + i] as i32).unsigned_abs() as u64;
        }
    }
    sad
}

/// A motion vector for the current block: the best of the starts and zero, refined by whole, half and
/// quarter pixel steps. Vectors stay even, as high precision motion is not coded.
fn search_mv(d: &mut Dec, inp: &Input, starts: [Mv; 2]) -> Mv {
    let r = d.refs[0].clone().expect("the LAST reference");
    let reg = region(d, d.b.bs);
    let mut best = Mv::default();
    let mut bc = u64::MAX;
    for s in starts.into_iter().chain([Mv::default()]) {
        let c = mv_sad(d, inp, &r, reg, s);
        if c < bc {
            bc = c;
            best = s;
        }
    }
    for step in [8i32, 4, 2] {
        for _ in 0..8 {
            let mut moved = false;
            for (dx, dy) in NEIGHBOURS {
                let m = Mv { x: (best.x as i32 + dx * step) as i16, y: (best.y as i32 + dy * step) as i16 };
                let c = mv_sad(d, inp, &r, reg, m);
                if c < bc {
                    bc = c;
                    best = m;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
    best
}

/// The context of the bit that says the block uses LAST (`single_ref[c][0]`), from its neighbours.
fn last_ref_ctx(d: &Dec, row: usize, col: usize) -> usize {
    let (have_a, have_l) = (row > 0, col > 0);
    let row7 = row & 7;
    let (ai, ac, ar) = (d.above_intra[col] != 0, d.above_comp[col] != 0, d.above_ref[col] as usize);
    let (li, lc, lr) = (d.left_intra[row7] != 0, d.left_comp[row7] != 0, d.left_ref[row7] as usize);
    let fc0 = d.h.fixcompref == 0;
    if have_a && !ai {
        if have_l && !li {
            if lc {
                if ac { 1 + (fc0 || lr == 0 || ar == 0) as usize } else { 3 * (ar == 0) as usize + (fc0 || lr == 0) as usize }
            } else if ac {
                3 * (lr == 0) as usize + (fc0 || ar == 0) as usize
            } else {
                2 * (lr == 0) as usize + 2 * (ar == 0) as usize
            }
        } else if ai {
            2
        } else if ac {
            1 + (fc0 || ar == 0) as usize
        } else {
            4 * (ar == 0) as usize
        }
    } else if have_l && !li {
        if lc { 1 + (fc0 || lr == 0) as usize } else { 4 * (lr == 0) as usize }
    } else {
        2
    }
}

/// A motion vector difference, as the decoder reads it: the joint, then each non-zero component.
fn write_mv(e: &mut BoolEncoder, p: &Probs, diff: Mv) {
    let j = (diff.y != 0) as u8 * 2 + (diff.x != 0) as u8;
    e.tree(&MV_JOINT_TREE, &p.mv_joint, j);
    if diff.y != 0 {
        write_comp(e, &p.mv_comp[0], diff.y as i32);
    }
    if diff.x != 0 {
        write_comp(e, &p.mv_comp[1], diff.x as i32);
    }
}

/// One non-zero motion vector component `v` (even, so its high precision bit is implicitly 1).
fn write_comp(e: &mut BoolEncoder, p: &MvCompProbs, v: i32) {
    e.put(p.sign, v < 0);
    let z = v.abs() - 1;
    let c = if z < 16 { 0 } else { (31 - ((z >> 3) as u32).leading_zeros()) as usize };
    e.tree(&MV_CLASS_TREE, &p.classes, c as u8);
    if c == 0 {
        e.put(p.class0, z >> 3 == 1);
        e.tree(&MV_FP_TREE, &p.class0_fp[(z >> 3) as usize], ((z >> 1) & 3) as u8);
    } else {
        let o = z - (8 << c);
        for m in 0..c {
            e.put(p.bits[m], (o >> (3 + m)) & 1 == 1);
        }
        e.tree(&MV_FP_TREE, &p.fp, ((o >> 1) & 3) as u8);
    }
}

/// Writes the mode information, the tokens and the contexts of the candidate `c` for the block at `d`'s
/// position, and records it for the later blocks and the loop filter.
fn write_block(d: &mut Dec, e: &mut BoolEncoder, c: &Cand, bs: usize) {
    let (row, col) = (d.row, d.col);
    let row7 = row & 7;
    let (nw, nh) = (BWH_TAB[1][bs][0] as usize, BWH_TAB[1][bs][1] as usize);
    let skip = c.ey.iter().all(|&n| n == 0) && c.ec.iter().all(|v| v.iter().all(|&n| n == 0));
    d.b.skip = skip;
    d.b.intra = c.intra;
    d.b.mode = [c.mode; 4];
    d.b.uvmode = c.uvmode;
    d.b.mv = [[c.mv, Mv::default()]; 4];

    e.put(d.p.skip[(d.left_skip[row7] + d.above_skip[col]) as usize], skip);
    let (have_a, have_l) = (row > 0, col > 0);
    let (ai, li) = (d.above_intra[col] as usize, d.left_intra[row7] as usize);
    let ic = if have_a && have_l {
        let s = ai + li;
        s + (s == 2) as usize
    } else if have_a {
        2 * ai
    } else if have_l {
        2 * li
    } else {
        0
    };
    e.put(d.p.intra[ic], !c.intra);
    if c.intra {
        e.tree(&INTRAMODE_TREE, &d.p.y_mode[SIZE_GROUP[bs] as usize], c.mode);
        e.tree(&INTRAMODE_TREE, &d.p.uv_mode[c.mode as usize], c.uvmode);
    } else {
        e.put(d.p.single_ref[last_ref_ctx(d, row, col)][0], false);
        let off = ABOVE_MODE_CTX_OFFSET[bs] as usize;
        let mc = INTER_MODE_CTX_LUT[d.above_mode[col + off] as usize][d.left_mode[row7 + off] as usize] as usize;
        e.tree(&INTER_MODE_TREE, &d.p.mv_mode[mc], c.mode);
        if c.mode == NEWMV {
            let base = d.base_mv(NEWMV, 0, -1).0;
            write_mv(e, &d.p, Mv { x: c.mv.x - base.x, y: c.mv.y - base.y });
        }
    }

    splat(&mut d.above_skip, col, nw, skip as u8);
    splat(&mut d.left_skip, row7, nh, skip as u8);
    splat(&mut d.above_partition, col, nw, ABOVE_CTX[bs]);
    splat(&mut d.left_partition, row7, nh, LEFT_CTX[bs]);
    splat(&mut d.above_intra, col, nw, c.intra as u8);
    splat(&mut d.left_intra, row7, nh, c.intra as u8);
    splat(&mut d.above_comp, col, nw, 0);
    splat(&mut d.left_comp, row7, nh, 0);
    splat(&mut d.above_mode, col, nw, c.mode);
    splat(&mut d.left_mode, row7, nh, c.mode);
    if !c.intra {
        splat(&mut d.above_ref, col, nw, 0);
        splat(&mut d.left_ref, row7, nh, 0);
    }
    let stride = d.sb_cols * 8;
    for y in 0..nh.min(d.rows - row) {
        for x in 0..nw.min(d.cols - col) {
            let m = &mut d.cur.mv[(row + y) * stride + col + x];
            if c.intra {
                m.r = [-1, -1];
            } else {
                m.r = [0, -1];
                m.mv[0] = c.mv;
            }
        }
    }

    if skip {
        splat(&mut d.above_y_nnz, col * 2, nw * 2, 0);
        splat(&mut d.left_y_nnz, row7 * 2, nh * 2, 0);
        for pl in 0..2 {
            splat(&mut d.above_uv_nnz[pl], col, nw, 0);
            splat(&mut d.left_uv_nnz[pl], row7, nh, 0);
        }
    } else {
        let refi = (!c.intra) as usize;
        let txtp = if c.intra { INTRA_TXFM_TYPE[c.mode as usize] as usize } else { 0 };
        let (ex, ey) = ends(d, bs);
        let ly = row7 * 2;
        let mut a = [0u8; 20];
        let mut l = [0u8; 20];
        a[..16].copy_from_slice(&d.above_y_nnz[col * 2..col * 2 + 16]);
        l[..16 - ly].copy_from_slice(&d.left_y_nnz[ly..16]);
        let mut n = 0;
        for y in 0..ey {
            for x in 0..ex {
                let nnz = (a[x] + l[y]) as usize;
                write_tokens(e, &c.ly[n], c.ey[n], nnz, SCANS[0][txtp], SCANS_NB[0][txtp], &d.coef[0][0][refi]);
                a[x] = (c.ey[n] > 0) as u8;
                l[y] = a[x];
                n += 1;
            }
        }
        d.above_y_nnz[col * 2..col * 2 + 16].copy_from_slice(&a[..16]);
        d.left_y_nnz[ly..16].copy_from_slice(&l[..16 - ly]);

        for pl in 0..2 {
            let mut a = [0u8; 20];
            let mut l = [0u8; 20];
            a[..16].copy_from_slice(&d.above_uv_nnz[pl][col..col + 16]);
            l[..16 - row7].copy_from_slice(&d.left_uv_nnz[pl][row7..16]);
            let mut n = 0;
            for y in 0..ey / 2 {
                for x in 0..ex / 2 {
                    let nnz = (a[x] + l[y]) as usize;
                    write_tokens(e, &c.lc[pl][n], c.ec[pl][n], nnz, SCANS[0][0], SCANS_NB[0][0], &d.coef[0][1][refi]);
                    a[x] = (c.ec[pl][n] > 0) as u8;
                    l[y] = a[x];
                    n += 1;
                }
            }
            d.above_uv_nnz[pl][col..col + 16].copy_from_slice(&a[..16]);
            d.left_uv_nnz[pl][row7..16].copy_from_slice(&l[..16 - row7]);
        }
    }
    d.lf_masks();
}

/// Codes the inter-frame block at (`row`, `col`) of level `bl` (8x8 or larger, whole): the candidates are
/// tried in turn and the cheapest is written. `pm` is the open-loop motion, a starting point for the search.
fn encode_block_inter(d: &mut Dec, e: &mut BoolEncoder, inp: &Input, row: usize, col: usize, bl: usize, pm: Mv) {
    let bs = bl * 3;
    set_block(d, row, col, bs);
    let near = [d.base_mv(NEARESTMV, 0, -1).0, d.base_mv(NEARMV, 0, -1).0];
    let nm = search_mv(d, inp, [near[0], pm]);
    // a vector already tried under a cheaper mode is not tried again
    let mut cands = vec![eval_inter(d, inp, ZEROMV, Mv::default(), near[0])];
    for (mode, mv) in [(NEARESTMV, near[0]), (NEARMV, near[1]), (NEWMV, nm)] {
        if cands.iter().all(|c| c.mv != mv) {
            cands.push(eval_inter(d, inp, mode, mv, near[0]));
        }
    }
    if bl == 3 {
        for mode in [intrapred::VERT_PRED, intrapred::HOR_PRED, DC_PRED, intrapred::TM_VP8_PRED] {
            cands.push(eval_intra(d, inp, mode));
        }
    }
    let best = cands.into_iter().min_by(|a, b| a.cost.total_cmp(&b.cost)).expect("candidates were tried");
    restore(d, bs, &best.rec);
    write_block(d, e, &best, bs);
}

/// Codes the superblock or node at (`row`, `col`) of level `bl` for an inter frame, as the decoder's
/// `decode_sb`, with the block sizes of `plan`. Nodes that reach past the frame's edge are split, as the
/// decoder's partition reading requires.
fn encode_sb_inter(d: &mut Dec, e: &mut BoolEncoder, inp: &Input, plan: &Plan, row: usize, col: usize, bl: usize) {
    let c = (((d.above_partition[col] >> (3 - bl)) & 1) as usize) | ((((d.left_partition[row & 7] >> (3 - bl)) & 1) as usize) << 1);
    let p = d.p.partition[bl][c];
    let hbs = 4usize >> bl;
    let idx = row * plan.cols + col;
    if col + hbs < d.cols && row + hbs < d.rows {
        if plan.lvl[idx] as usize == bl {
            e.tree(&PARTITION_TREE, &p, 0);
            let (x, y) = plan.mv[idx];
            encode_block_inter(d, e, inp, row, col, bl, Mv { x: (x * 8) as i16, y: (y * 8) as i16 });
        } else {
            e.tree(&PARTITION_TREE, &p, 3);
            for (r, cc) in [(row, col), (row, col + hbs), (row + hbs, col), (row + hbs, col + hbs)] {
                encode_sb_inter(d, e, inp, plan, r, cc, bl + 1);
            }
        }
    } else if col + hbs < d.cols {
        e.put(p[1], true);
        encode_sb_inter(d, e, inp, plan, row, col, bl + 1);
        encode_sb_inter(d, e, inp, plan, row, col + hbs, bl + 1);
    } else if row + hbs < d.rows {
        e.put(p[2], true);
        encode_sb_inter(d, e, inp, plan, row, col, bl + 1);
        encode_sb_inter(d, e, inp, plan, row + hbs, col, bl + 1);
    } else {
        encode_sb_inter(d, e, inp, plan, row, col, bl + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::Vp9Decoder;
    use ironav_core::{Decoder, Frame};

    /// A moving pattern: a gradient, a bar sliding right and noise, with chroma ramps.
    fn pattern(w: usize, h: usize, t: usize) -> VideoFrame {
        let mut seed = 99u32 + t as u32;
        let mut planes = Vec::new();
        let mut y = Vec::new();
        for r in 0..h {
            for c in 0..w {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let noise = ((seed >> 16) % 9) as i32 - 4;
                let bar = (c + 2 * t) % w < w / 4;
                let v = if bar { 200 } else { (c * 120 / w + r * 80 / h) as i32 + 40 };
                y.push((v + noise).clamp(0, 255) as u8);
            }
        }
        planes.push(y);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        planes.push((0..cw * ch).map(|i| (100 + (i % cw) * 60 / cw + t) as u8).collect());
        planes.push((0..cw * ch).map(|i| (150 - (i / cw) * 50 / ch) as u8).collect());
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes }
    }

    fn params(w: usize, h: usize) -> StreamParams {
        StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: ironav_core::Rational::new(30, 1) }
    }

    /// The planes of a picture, cropped to its size, as bytes.
    fn crop(p: &Pic) -> Vec<u8> {
        let mut out = Vec::new();
        for pl in 0..3 {
            let (w, h) = if pl == 0 { (p.w, p.h) } else { (p.w.div_ceil(2), p.h.div_ceil(2)) };
            for r in 0..h {
                out.extend(p.planes[pl][r * p.stride[pl]..][..w].iter().map(|&s| s as u8));
            }
        }
        out
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a.iter().zip(b).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    }

    /// Encodes `n` frames at `qp` (key frames every `keyint`), decodes them, and checks that every decoded
    /// frame is the encoder's reconstruction. Returns the decoded frames as bytes of the picture planes, and
    /// the size of each packet.
    fn round_trip_keyint(w: usize, h: usize, n: usize, qp: u8, keyint: u64) -> (Vec<Vec<u8>>, Vec<usize>) {
        let mut enc = Vp9Encoder::new(&params(w, h), Some(qp)).unwrap().with_keyint(keyint);
        let mut dec = Vp9Decoder::new();
        let mut out = Vec::new();
        let mut sizes = Vec::new();
        for t in 0..n {
            let pk = enc.encode(&Frame::Video(pattern(w, h, t))).unwrap().remove(0);
            sizes.push(pk.data.len());
            let frames = dec.decode(&pk).unwrap();
            let Frame::Video(got) = &frames[0] else { panic!("video expected") };
            let bytes: Vec<u8> = got.planes.concat();
            let recon = enc.last.as_deref().unwrap();
            assert_eq!(bytes, crop(recon), "frame {t} of {w}x{h}: decoder differs from the encoder's reconstruction");
            out.push(bytes);
        }
        (out, sizes)
    }

    fn round_trip(w: usize, h: usize, n: usize, qp: u8) -> Vec<Vec<u8>> {
        round_trip_keyint(w, h, n, qp, DEFAULT_KEYINT).0
    }

    #[test]
    fn reconstruction_matches_the_decoder() {
        round_trip(64, 64, 4, 23);
        round_trip(177, 99, 4, 23);
    }

    #[test]
    fn inter_reconstruction_matches_the_decoder() {
        round_trip(64, 64, 12, 23);
        round_trip(177, 99, 12, 23);
        round_trip(177, 99, 12, 51);
    }

    #[test]
    fn quality_is_moderate_at_moderate_q() {
        for (w, h) in [(64, 64), (177, 99)] {
            let frames = round_trip(w, h, 3, 28);
            for (t, got) in frames.iter().enumerate() {
                let src = pattern(w, h, t);
                let want: Vec<u8> = src.planes.concat();
                let p = psnr(&got[..w * h], &want[..w * h]);
                assert!(p > 30.0, "{w}x{h} frame {t}: luma PSNR {p:.1} dB");
            }
        }
    }

    #[test]
    fn inter_frames_are_smaller_than_key_frames() {
        let (_, inter) = round_trip_keyint(177, 99, 10, 28, DEFAULT_KEYINT);
        let (_, key) = round_trip_keyint(177, 99, 10, 28, 1);
        let (i, k): (usize, usize) = (inter.iter().sum(), key.iter().sum());
        assert!(i < k * 3 / 4, "inter frames took {i} bytes, key frames only {k}");
    }

    #[test]
    fn flat_frames_code_as_small_frames() {
        let (w, h) = (64, 64);
        let flat = VideoFrame { width: 64, height: 64, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![128; 4096], vec![128; 1024], vec![128; 1024]] };
        let enc = Vp9Encoder::new(&params(w, h), Some(23)).unwrap();
        let inp = enc.input(&flat).unwrap();
        let (data, _) = enc.code_key(&inp).unwrap();
        assert!(data.len() < 100, "a flat key frame took {} bytes", data.len());
    }
}
