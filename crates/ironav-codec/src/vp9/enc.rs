//! A software VP9 encoder for profile 0 (8-bit 4:2:0), key frames only.
//!
//! Every frame is a shown key frame that refreshes all references, with one tile, no loop filter, no
//! segmentation and 4x4 transforms. Every 64x64 superblock is split down to 8x8 blocks; each block takes
//! the best of DC, vertical, horizontal and TrueMotion prediction for luma (DC for chroma), and its 4x4
//! residuals are quantised and coded with the decoder's contexts. The encoder reconstructs with the
//! decoder's own edge, prediction and inverse transform code, so its pictures are the decoder's.

use super::dec::*;
use super::header::*;
use super::intrapred;
use super::itx;
use super::tables::*;
use super::bad;
use crate::vp8::enc::boolenc::BoolEncoder;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use std::f64::consts::PI;

/// The quantiser when none is set (the H.264 encoder's default).
const DEFAULT_QP: u8 = 23;

/// The coefficient magnitude limit: the largest value the token tree codes.
const MAX_LEVEL: i32 = 67 + (1 << 14) - 1;

/// The probabilities of the 14 extra bits of the largest token category, most significant first.
const CAT6_PROBS: [u8; 14] = [254, 254, 254, 252, 249, 243, 230, 196, 177, 153, 140, 133, 130, 129];

/// The base quantiser index for a quantiser of 0 to 51 (as for H.264). Index 0 would be lossless.
fn base_q(qp: u8) -> i32 {
    (qp.min(51) as i32 * 255 / 51).max(1)
}

/// The source picture, extended to whole 8x8 blocks, and the dequantisers [dc, ac] of luma and chroma.
struct Input {
    planes: [Vec<i32>; 3],
    stride: [usize; 3],
    dq: [i32; 2],
}

pub struct Vp9Encoder {
    width: usize,
    height: usize,
    q: i32,
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
        Ok(Self { width: width as usize, height: height as usize, q })
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
        let (bw, bh) = (w.div_ceil(8) * 8, h.div_ceil(8) * 8);
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

    /// Codes one key frame: the headers, then every block decided, coded and reconstructed. Returns the
    /// frame and the reconstruction.
    fn code_frame(&self, inp: &Input) -> Result<(Vec<u8>, Pic)> {
        let (w, h) = (self.width, self.height);
        let mut hdr = FrameHeader { keyframe: true, width: w, height: h, ss_h: true, ss_v: true, bpp: 8, ..Default::default() };
        // no probability updates: the compressed header is the transform mode and the update flags
        let mut ce = BoolEncoder::default();
        ce.put(128, false);
        ce.literal(0, 2); // tx_mode ONLY_4X4
        ce.put(128, false); // no coefficient updates for 4x4
        for _ in 0..3 {
            ce.put(252, false); // no skip probability updates
        }
        let comp = ce.finish();
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

        let mut data = uncompressed(w, h, self.q, comp.len());
        data.extend_from_slice(&comp);
        data.extend(e.finish());
        // a last byte of the form 110xxxxx would read as a superframe index, so a zero byte follows it
        if data.last().is_some_and(|&b| b & 0xe0 == 0xc0) {
            data.push(0);
        }
        Ok((data, d.cur))
    }
}

impl Encoder for Vp9Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the VP9 encoder received an audio frame"));
        };
        let inp = self.input(v)?;
        let (data, _) = self.code_frame(&inp)?;
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
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

/// The uncompressed header of a profile 0 key frame of `w` x `h` at base quantiser `q`.
fn uncompressed(w: usize, h: usize, q: i32, comp_len: usize) -> Vec<u8> {
    let mut b = BitWriter::default();
    b.put(2, 2); // frame marker
    b.put(0, 2); // profile 0
    b.put(0, 1); // not show_existing_frame
    b.put(0, 1); // key frame
    b.put(1, 1); // shown
    b.put(0, 1); // not error resilient
    b.put(0x49_83_42, 24); // sync code
    b.put(0, 3); // colour space: unknown
    b.put(0, 1); // studio range
    b.put(w as u32 - 1, 16);
    b.put(h as u32 - 1, 16);
    b.put(0, 1); // render size is the frame size
    b.put(0, 1); // no refresh of the frame context
    b.put(1, 1); // parallel decoding: no adaptation
    b.put(0, 2); // frame context 0
    b.put(0, 6); // loop filter level 0
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

/// Codes the superblock or block at (`row`, `col`) of size 2^(3 - `bl`) 8x8 blocks, as the decoder's
/// `decode_sb`: always split, so every block is 8x8.
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

/// Codes one 8x8 block: picks its modes, writes the skip flag, the modes and the tokens with the
/// decoder's contexts, and leaves the reconstruction in the picture.
fn encode_block(d: &mut Dec, e: &mut BoolEncoder, inp: &Input, row: usize, col: usize) {
    d.row = row;
    d.col = col;
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
        let mut r = [0f64; 16];
        for j in 0..4 {
            for i in 0..4 {
                let s = inp.planes[0][(py + j) * inp.stride[0] + px + i];
                r[j * 4 + i] = (s - d.cur.planes[0][(py + j) * stride + px + i] as i32) as f64;
            }
        }
        let (l, eob) = quantize(&fdct4(&r, txtp), inp.dq, SCANS[0][txtp]);
        lv[n] = l;
        eobs[n] = eob;
        if eob > 0 {
            let mut coef = dequant(&l, inp.dq);
            itx::inverse_add(0, txtp, &mut coef, &mut d.cur.planes[0][py * stride + px..], stride, eob, 8);
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
    let mut r = [0f64; 16];
    for j in 0..4 {
        for i in 0..4 {
            let s = inp.planes[1 + pl][(py + j) * inp.stride[1 + pl] + px + i];
            r[j * 4 + i] = (s - d.cur.planes[1 + pl][(py + j) * stride + px + i] as i32) as f64;
        }
    }
    let dq = inp.dq;
    let (l, eob) = quantize(&fdct4(&r, 0), dq, SCANS[0][0]);
    if eob > 0 {
        let mut coef = dequant(&l, dq);
        itx::inverse_add(0, 0, &mut coef, &mut d.cur.planes[1 + pl][py * stride + px..], stride, eob, 8);
    }
    (l, eob)
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

    /// Encodes `n` frames at `qp`, decodes them, and checks that every decoded frame is the encoder's
    /// reconstruction. Returns the decoded frames as bytes of the picture planes.
    fn round_trip(w: usize, h: usize, n: usize, qp: u8) -> Vec<Vec<u8>> {
        let enc = Vp9Encoder::new(&params(w, h), Some(qp)).unwrap();
        let mut dec = Vp9Decoder::new();
        let mut out = Vec::new();
        for t in 0..n {
            let v = pattern(w, h, t);
            let inp = enc.input(&v).unwrap();
            let (data, recon) = enc.code_frame(&inp).unwrap();
            let pk = Packet { stream: 0, pts: t as i64, duration: 1, data };
            let frames = dec.decode(&pk).unwrap();
            let Frame::Video(got) = &frames[0] else { panic!("video expected") };
            let bytes: Vec<u8> = got.planes.concat();
            assert_eq!(bytes, crop(&recon), "frame {t} of {w}x{h}: decoder differs from the encoder's reconstruction");
            out.push(bytes);
        }
        out
    }

    #[test]
    fn reconstruction_matches_the_decoder() {
        round_trip(64, 64, 4, 23);
        round_trip(177, 99, 4, 23);
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
    fn flat_frames_code_as_small_frames() {
        let (w, h) = (64, 64);
        let flat = VideoFrame { width: 64, height: 64, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![128; 4096], vec![128; 1024], vec![128; 1024]] };
        let enc = Vp9Encoder::new(&params(w, h), Some(23)).unwrap();
        let inp = enc.input(&flat).unwrap();
        let (data, _) = enc.code_frame(&inp).unwrap();
        assert!(data.len() < 100, "a flat key frame took {} bytes", data.len());
    }
}
