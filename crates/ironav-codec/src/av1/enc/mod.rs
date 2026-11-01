//! A software AV1 encoder: key frames every `keyint` frames (250 by default, as the VP9 and HEVC
//! encoders), and inter frames between them. Profile 0, 8-bit 4:2:0, one tile.
//!
//! Each frame is a temporal unit: a temporal delimiter, the sequence header, and a frame OBU with
//! the frame's header and its tile. An inter frame predicts from one of the three frames before it, or
//! from the average of LAST_FRAME and LAST2_FRAME or LAST3_FRAME (compound, `with_compound`)
//! (LAST_FRAME, LAST2_FRAME and LAST3_FRAME, held in reference slots that each frame refreshes in
//! turn; `with_refs(1)` limits it to LAST_FRAME), with the regular 8-tap filter, quarter-sample motion
//! vectors (searched to half samples) and the reference MV stack's candidates. LAST_FRAME is searched
//! fully and the others from its vector.
//!
//! With `with_altref(lag)` the encoder looks `lag` frames ahead: the last of each group of frames is
//! coded first as a hidden ALTREF_FRAME (`show_frame` 0, in a reference slot of its own), and the frames
//! before it predict from it as well, singly or bidirectionally with LAST_FRAME (compound, one forward
//! and one backward reference). The ALTREF is then shown by a show_existing_frame header, in the
//! temporal unit of its display time. The hidden frame shares the temporal unit of the first frame
//! shown after it, so every packet has the display time of its shown frame and the packets are in
//! display order. Every frame carries its display index as an order hint (7 bits). Superblocks
//! are 64x64, split by an open-loop search on the source down to 8x8 (never larger than 32x32, so
//! every block is coded with one transform of its size). Each block is coded as intra or inter by
//! rate-distortion cost, and intra blocks take the best of the 13 intra modes with angle deltas.
//! Coefficients are quantised at the base quantiser index that `qp` maps to (as for the other
//! encoders, 0 to 51), or with a rate control that moves the quantiser so the stream's average bit
//! rate follows the one asked for. The deblocking filter and CDEF run on each reconstructed frame, with levels
//! and strengths that follow the quantiser, and the reference is the filtered frame.
//!
//! The encoder keeps the decoder's own per-frame state (`av1::block::FrameDec`) and reconstructs
//! through it, so the pictures it keeps as references are the decoder's pictures.
//!
//! The range coder (`rc.rs`) is adapted from rav1e's `src/ec.rs`, which is BSD-2-Clause licensed
//! (the rav1e contributors); rav1e is not a dependency.

mod fwd;
mod headers;
mod motion;
mod rc;
mod tile;

pub use headers::config_record;

use std::collections::VecDeque;
use std::sync::Arc;

use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};

use crate::av1::block::{FrameDec, Picture};
use crate::av1::ipred::Plane;
use crate::av1::obu::{parse_frame_header, parse_sequence_header, RefInfo, RefState, SequenceHeader};
use crate::av1::postfilter;
use crate::av1::superres::downscaled_width;
use headers::FrameParams;

/// The quantiser when none is set (the VP9 encoder's default).
const DEFAULT_QP: u8 = 23;

/// The key frame interval when none is set (the VP9 and HEVC encoders' default).
const DEFAULT_KEYINT: u64 = 250;

/// The reference slot of ALTREF_FRAME: the hidden frames are stored there, and no shown frame
/// refreshes it. The slots 0 to 2 hold LAST_FRAME, LAST2_FRAME and LAST3_FRAME in turn.
const ALTREF_SLOT: usize = 3;

/// The largest lag `with_altref` takes: the frames an ALTREF group may span.
const MAX_LAG: usize = 16;

/// How much finer (in the 0 to 51 quantiser) a hidden ALTREF is coded than the frames before it.
const ALT_QP_OFFSET: f64 = 4.0;

/// Bits per pixel at quantiser 26, roughly, for the rate control's first guess (as the H.264 encoder's).
const BPP: f64 = 0.1;

/// The largest picture one tile holds: 4096 wide, and no more than 2304 superblocks, so that the
/// minimum tile counts stay zero.
const MAX_WIDTH: u32 = 4096;
const MAX_SUPERBLOCKS: usize = 2304;

/// The quantiser offset of key frames under rate control, in the 0 to 51 quantiser: key frames are
/// finer, since the inter frames after them inherit their quality, and inter frames take the rate
/// control's quantiser as it stands (as the H.264 encoder's IDR offset).
const KEY_QP_OFFSET: f64 = -3.0;

/// The base quantiser index of a quantiser of 0 to 51 (as the VP9 encoder maps it, 5 steps to each
/// quantiser step). Index 0 would be lossless, which this encoder does not code.
fn base_q(qp: f64) -> u32 {
    (qp.clamp(0.0, 51.0) * 255.0 / 51.0).round().max(1.0) as u32
}

/// The deblocking level of a base quantiser index: a level that rises with the quantiser, from 0 at
/// the finest to 31 at the coarsest.
fn filter_level(q: u32) -> u8 {
    (q / 8).min(31) as u8
}

/// The CDEF strengths (primary and secondary, luma then chroma) of a base quantiser index.
fn cdef_strengths(q: u32) -> [u8; 4] {
    let pri = (q / 32).min(15) as u8;
    [pri, 1, pri / 2, 0]
}

pub struct Av1Encoder {
    width: usize,
    height: usize,
    /// A fixed quantiser (0 to 51), or a bit rate in bits per second to average.
    qp: Option<u8>,
    bitrate: Option<u32>,
    /// The frame rate, and the rate control state: the quantiser (0 to 51) for the next inter frame,
    /// and the bits spent against the bits allowed.
    fps: f64,
    rc_qp: f64,
    spent: f64,
    allowed: f64,
    keyint: u64,
    /// The display index of the last key frame, and of the next frame to code (the first pending one).
    last_key: u64,
    next_idx: u64,
    /// The frames waiting to be coded, in display order (only with `with_altref`: an ALTREF group
    /// is coded once its last frame has arrived), and the group's lag: 1 codes every frame as it comes.
    pending: VecDeque<VideoFrame>,
    lag: usize,
    seq: SequenceHeader,
    /// The last three shown frames as references, in reference slots 0 to 2, and the hidden ALTREF
    /// in slot `ALTREF_SLOT`, each with what the frame headers read of it. The newest is in `head`;
    /// each shown inter frame refreshes the slot after it, which holds the oldest, so the slots are
    /// LAST (`head`), LAST2 (the one before) and LAST3.
    dpb: [Option<(Arc<Picture>, RefState)>; 4],
    head: usize,
    /// How many references a block may use: 1 (LAST_FRAME alone) or 3 (see `with_refs`).
    nrefs: usize,
    /// Whether blocks may code compound predictions of two references (see `with_compound`).
    compound: bool,
    /// The source luma of the last frame, which the next frame's motion field reads.
    prev_src: Option<Plane>,
    /// The forward transform bases, built on first use and kept across frames.
    bases: fwd::Bases,
    /// The superres denominator, when frames are coded at a downscaled width (see `with_superres`).
    superres: Option<u32>,
    /// Whether frames use loop restoration (see `with_restoration`).
    restoration: bool,
}

impl Av1Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, 0 to 51) or an average
    /// bit rate in bits per second. With a bit rate the quantiser follows it, starting from a guess
    /// from the bits per pixel the rate allows; `qp` is then ignored.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the AV1 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software AV1 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        let superblocks = width.div_ceil(64) as usize * height.div_ceil(64) as usize;
        if width == 0 || height == 0 || width > MAX_WIDTH || superblocks > MAX_SUPERBLOCKS {
            return Err(Error::invalid_arg(format!("{width}x{height} is too large for the software AV1 encoder (one tile, up to 4096 wide)")));
        }
        let seq = parse_sequence_header(&headers::sequence_header(width, height))?;
        let fps = if frame_rate.num > 0 && frame_rate.den > 0 { frame_rate.num as f64 / frame_rate.den as f64 } else { 25.0 };
        // a first guess from the bits per pixel the rate allows, about 26 at a tenth of a bit per pixel
        let rc_qp = bitrate.map_or(DEFAULT_QP as f64, |r| {
            (26.0 - 7.0 * (r as f64 / (fps * width as f64 * height as f64) / BPP).log2()).clamp(10.0, 45.0)
        });
        Ok(Self {
            width: width as usize,
            height: height as usize,
            qp: if bitrate.is_none() { Some(qp.unwrap_or(DEFAULT_QP).min(51)) } else { None },
            bitrate,
            fps,
            rc_qp,
            spent: 0.0,
            allowed: 0.0,
            keyint: DEFAULT_KEYINT,
            last_key: 0,
            next_idx: 0,
            pending: VecDeque::new(),
            lag: 1,
            seq,
            dpb: [None, None, None, None],
            head: 0,
            nrefs: 3,
            compound: true,
            prev_src: None,
            bases: fwd::Bases::default(),
            superres: None,
            restoration: false,
        })
    }

    /// Sets the key frame interval: a key frame every `n` frames (at least 1).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    /// Sets how many references a block chooses from: 1 for LAST_FRAME alone (the frame before),
    /// or 3 for LAST_FRAME, LAST2_FRAME and LAST3_FRAME (the three frames before), each block by
    /// its rate-distortion cost.
    pub fn with_refs(mut self, n: usize) -> Self {
        self.nrefs = n.clamp(1, 3);
        self
    }

    /// Lets blocks code compound predictions: the average of two references (LAST_FRAME with
    /// LAST2_FRAME or LAST3_FRAME, unidirectional), chosen by rate-distortion cost with the other
    /// modes. On by default; it needs `with_refs(3)`, since with one reference no compound block is
    /// possible. `with_compound(false)` codes every block with one reference.
    pub fn with_compound(mut self, on: bool) -> Self {
        self.compound = on;
        self
    }

    /// Codes hidden ALTREF frames: every `lag` frames (2 to 16), the last of each group is coded
    /// first, from the source `lag - 1` frames ahead, and is shown by a show_existing_frame header at
    /// its display time. The frames between predict from it as well as from LAST_FRAME (single, and
    /// bidirectional with LAST_FRAME). The encoder then holds `lag` frames before it can code a group,
    /// so its packets come `lag - 1` frames late, and `flush` codes the rest. A lag of 1 (the default)
    /// codes every frame as it comes. Key frames still start every key frame interval, and a group
    /// never spans one.
    pub fn with_altref(mut self, lag: usize) -> Self {
        self.lag = lag.clamp(1, MAX_LAG);
        self
    }

    #[cfg(test)]
    /// The newest reference frame, LAST_FRAME.
    fn last(&self) -> &(Arc<Picture>, RefState) {
        self.dpb[self.head].as_ref().expect("an inter frame has a reference")
    }

    /// Codes every frame with superres at `denom` (9 to 16, the denominator of 8ths of the width):
    /// the frame is coded at the downscaled width (the source is resampled horizontally), and its
    /// reconstruction is upscaled before it becomes a reference, as the decoder does. Used by the
    /// tests, which compare the decoder's output with libaom's.
    pub fn with_superres(mut self, denom: u32) -> Self {
        assert!((9..=16).contains(&denom), "superres denominator {denom} is not 9 to 16");
        self.superres = Some(denom);
        self.set_sequence_header();
        self
    }

    /// Codes every frame with loop restoration: the Wiener filter on each plane, in units of 64
    /// luma samples (32 chroma) of which every other one is filtered and the rest are left alone.
    /// The filter's coefficients are the midpoints, so the filter is the same in every unit that
    /// uses it. Used by the tests, with superres.
    pub fn with_restoration(mut self) -> Self {
        self.restoration = true;
        self.set_sequence_header();
        self
    }

    /// Parses the sequence header of the options set so far, for the encoder's own frame headers.
    fn set_sequence_header(&mut self) {
        let hdr = headers::sequence_header_with(self.width as u32, self.height as u32, self.superres.is_some(), self.restoration);
        self.seq = parse_sequence_header(&hdr).expect("the sequence header of the options parses");
    }

    /// The quantiser (0 to 51) of the next frame: the one set, or the rate control's, with the key
    /// frame offset.
    fn next_qp(&self, key: bool) -> f64 {
        match self.qp {
            Some(q) => q as f64,
            None => (self.rc_qp + if key { KEY_QP_OFFSET } else { 0.0 }).clamp(1.0, 51.0),
        }
    }

    /// Steers the quantiser so the bits spent follow the bits the rate allows, as the H.264 encoder
    /// does: a frame's size roughly halves for every 6 added to the quantiser, so the size of the last
    /// frame says what quantiser it would have needed, and the surplus or deficit against a second's
    /// worth of bits pulls the next quantiser further. A key frame is expected to cost four frames'
    /// worth. The quantiser moves at most 4 steps a frame.
    fn rate_update(&mut self, bits: usize, qp: f64, key: bool) {
        let Some(rate) = self.bitrate else {
            return;
        };
        let per_frame = rate as f64 / self.fps;
        self.spent += bits as f64;
        self.allowed += per_frame;
        let window = rate as f64;
        let budget = 6.0 * ((self.spent + window) / (self.allowed + window).max(1.0)).log2();
        let (offset, expected) = if key { (KEY_QP_OFFSET, per_frame * 4.0) } else { (0.0, per_frame) };
        let wanted = (qp - offset) + 6.0 * (bits as f64 / expected).max(1e-3).log2();
        let next = 0.5 * self.rc_qp + 0.5 * wanted + budget;
        self.rc_qp = next.clamp(self.rc_qp - 4.0, self.rc_qp + 4.0).clamp(1.0, 51.0);
    }

    /// The temporal unit of one shown frame, with no lookahead (every frame is coded as it comes, as
    /// with the default lag of 1): what `Encoder::encode` returns for it.
    pub fn encode_frame(&mut self, v: &VideoFrame) -> Result<Vec<u8>> {
        self.pending.push_back(v.clone());
        let packets = self.drain(false)?;
        Ok(packets.into_iter().flat_map(|p| p.data).collect())
    }

    /// The frame OBU's payload (the frame header and the tile) of one frame at display index `order_hint`,
    /// and its quantiser. A key frame refreshes every reference; a shown inter frame refreshes the
    /// oldest LAST slot; a hidden frame is an ALTREF, stored in its own slot. `dists` are the display
    /// distances of LAST_FRAME and of ALTREF_FRAME (`None` when the frame may not use it).
    fn code_frame(&mut self, v: &VideoFrame, key: bool, shown: bool, order_hint: u32, dists: (i32, Option<i32>)) -> Result<(Vec<u8>, f64)> {
        let (w, h) = (self.width, self.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the AV1 encoder was set up for {w}x{h} yuv420p frames")));
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if v.planes[0].len() < w * h || v.planes[1].len() < cw * ch || v.planes[2].len() < cw * ch {
            return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
        }
        // a hidden ALTREF is coded finer than the frames that predict from it
        let qp = (self.next_qp(key) - if key || shown { 0.0 } else { ALT_QP_OFFSET }).clamp(0.0, 51.0);
        let q = base_q(qp);
        // LAST_FRAME is the newest slot, LAST2_FRAME the one before it and LAST3_FRAME the oldest; a shown
        // frame is then stored in the oldest slot, which becomes the newest. The other references name
        // LAST_FRAME (GOLDEN) or ALTREF_SLOT (BWDREF, ALTREF2 and ALTREF).
        let ref_slots = [self.head, (self.head + 2) % 3, (self.head + 1) % 3];
        let new_slot = (self.head + 1) % 3;
        let ref_idx = [ref_slots[0], ref_slots[1], ref_slots[2], ref_slots[0], ALTREF_SLOT, ALTREF_SLOT, ALTREF_SLOT].map(|s| s as u8);
        let ref_hints: [u32; 7] = std::array::from_fn(|i| self.dpb[ref_idx[i] as usize].as_ref().map_or(0, |(_, s)| s.order_hint));
        let refresh = if key {
            0xff
        } else if shown {
            1 << new_slot
        } else {
            1 << ALTREF_SLOT
        };
        let params = FrameParams {
            key,
            width: w as u32,
            height: h as u32,
            base_q_idx: q,
            lf_level: filter_level(q),
            cdef: cdef_strengths(q),
            superres: self.superres,
            restoration: self.restoration,
            order_hint,
            shown,
            ref_idx,
            ref_hints,
            refresh,
            reference_select: self.compound,
        };
        let hdr = headers::frame_header(&params);

        // the frame's header as the decoder reads it, with the reference slots holding the frames' states
        let refs = RefInfo { slots: std::array::from_fn(|i| self.dpb.get(i).and_then(|s| s.as_ref().map(|(_, s)| s.clone()))) };
        let fh = parse_frame_header(&hdr, &self.seq, &refs, 0, 0)?;
        // with superres the frame is coded at its downscaled width, from the resampled source
        let coded_w = self.superres.map_or(w, |d| downscaled_width(w as u32, d) as usize);
        let resampled = self.superres.map(|_| resample_planes(v, w, h, coded_w));
        let planes = match &resampled {
            Some(p) => [&p[0][..], &p[1][..], &p[2][..]],
            None => [&v.planes[0][..], &v.planes[1][..], &v.planes[2][..]],
        };
        let mut f = FrameDec::new(self.seq.clone(), fh);
        if !key {
            // each reference frame names its slot as the header does
            for rf in 1..8 {
                f.refs[rf] = self.dpb[ref_idx[rf - 1] as usize].as_ref().map(|(pic, _)| pic.clone());
            }
        }
        let enc = tile::FrameEnc::new(f, planes[0], planes[1], planes[2], (coded_w, h), self.prev_src.clone(), std::mem::take(&mut self.bases), self.nrefs, self.compound, dists);
        let (tile_bytes, mut f, src_luma, bases) = enc.encode();
        self.bases = bases;

        // the post filters, as the decoder runs them on the frame
        postfilter(&mut f, 1);
        let state = RefState {
            frame_type: f.h.frame_type,
            order_hint: f.h.order_hint,
            upscaled_width: f.h.upscaled_width,
            height: f.h.height,
            render_width: f.h.render_width,
            render_height: f.h.render_height,
            lf_ref_deltas: f.h.lf_ref_deltas,
            lf_mode_deltas: f.h.lf_mode_deltas,
            feature_enabled: f.h.feature_enabled,
            feature_data: f.h.feature_data,
            gm_params: f.h.gm_params,
            grain: None,
        };
        let pic = Picture {
            width: w,
            height: h,
            bd: 8,
            ss: [1, 1],
            mono: false,
            rgb: false,
            frame_type: f.h.frame_type,
            order_hint: f.h.order_hint,
            saved_hints: f.h.order_hints,
            mi_rows: f.h.mi_rows,
            mi_cols: f.h.mi_cols,
            mvs: std::mem::take(&mut f.saved_mvs),
            seg_ids: Vec::new(),
            planes: std::mem::replace(&mut f.planes, [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)]),
        };
        let entry = (Arc::new(pic), state);
        if key {
            self.dpb = [Some(entry.clone()), Some(entry.clone()), Some(entry.clone()), Some(entry)];
            self.head = 0;
        } else if shown {
            self.dpb[new_slot] = Some(entry);
            self.head = new_slot;
        } else {
            self.dpb[ALTREF_SLOT] = Some(entry);
        }
        if shown {
            self.prev_src = Some(src_luma);
        }

        let mut frame = hdr;
        frame.extend_from_slice(&tile_bytes);
        Ok((frame, qp))
    }

    /// The start of a temporal unit: the temporal delimiter and the sequence header.
    fn temporal_unit_start(&self) -> Vec<u8> {
        let mut out = headers::obu(headers::OBU_TEMPORAL_DELIMITER, &[]);
        out.extend(headers::obu(headers::OBU_SEQUENCE_HEADER, &headers::sequence_header_with(self.width as u32, self.height as u32, self.superres.is_some(), self.restoration)));
        out
    }

    /// Codes the frames that are ready, in display order, and returns their packets. A key frame is
    /// coded as soon as it is at the front. An inter group of `lag` frames (cut short at the next key
    /// frame) is coded when all its frames are here, or at `flush` with what there is: its last frame
    /// is coded first as a hidden ALTREF, in the temporal unit of the group's first frame, then the
    /// frames before it, then the ALTREF's show_existing_frame in its own temporal unit.
    fn drain(&mut self, flush: bool) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        while !self.pending.is_empty() {
            let idx = self.next_idx;
            if self.dpb[self.head].is_none() || idx - self.last_key >= self.keyint {
                let v = self.pending.pop_front().expect("a pending frame");
                let (frame, qp) = self.code_frame(&v, true, true, idx as u32, (0, None))?;
                let mut tu = self.temporal_unit_start();
                tu.extend(headers::obu(headers::OBU_FRAME, &frame));
                self.rate_update(tu.len() * 8, qp, true);
                self.last_key = idx;
                self.next_idx += 1;
                out.push(Packet { stream: 0, pts: v.pts, duration: 1, data: tu });
                continue;
            }
            // the frames before the next key frame
            let until_key = (self.keyint - (idx - self.last_key)) as usize;
            let len = self.lag.min(until_key);
            if self.pending.len() < len && !flush {
                break;
            }
            let len = len.min(self.pending.len());
            if len <= 1 {
                let v = self.pending.pop_front().expect("a pending frame");
                let (frame, qp) = self.code_frame(&v, false, true, idx as u32, (1, None))?;
                let mut tu = self.temporal_unit_start();
                tu.extend(headers::obu(headers::OBU_FRAME, &frame));
                self.rate_update(tu.len() * 8, qp, false);
                self.next_idx += 1;
                out.push(Packet { stream: 0, pts: v.pts, duration: 1, data: tu });
                continue;
            }
            // the group's ALTREF: the last of its frames, `len - 1` frames ahead of LAST_FRAME's frame
            let alt_idx = idx + len as u64 - 1;
            let alt = self.pending[len - 1].clone();
            let (hidden, _) = self.code_frame(&alt, false, false, alt_idx as u32, (len as i32, None))?;
            for k in 0..len - 1 {
                let v = self.pending.pop_front().expect("a pending frame");
                let dist = (len - 1 - k) as i32;
                let (frame, qp) = self.code_frame(&v, false, true, (idx + k as u64) as u32, (1, Some(dist)))?;
                let mut tu = self.temporal_unit_start();
                if k == 0 {
                    tu.extend(headers::obu(headers::OBU_FRAME, &hidden));
                }
                tu.extend(headers::obu(headers::OBU_FRAME, &frame));
                self.rate_update(tu.len() * 8, qp, false);
                self.next_idx += 1;
                out.push(Packet { stream: 0, pts: v.pts, duration: 1, data: tu });
            }
            let shown = self.pending.pop_front().expect("the ALTREF's frame");
            let mut tu = self.temporal_unit_start();
            tu.extend(headers::obu(headers::OBU_FRAME_HEADER, &headers::show_existing_header(ALTREF_SLOT as u8)));
            self.rate_pass();
            self.next_idx += 1;
            out.push(Packet { stream: 0, pts: shown.pts, duration: 1, data: tu });
        }
        Ok(out)
    }

    /// Passes a frame that codes no bits (a show_existing_frame) in the rate control's budget.
    fn rate_pass(&mut self) {
        if let Some(rate) = self.bitrate {
            self.allowed += rate as f64 / self.fps;
        }
    }
}

/// The planes of a yuv420p picture of `w` x `h` resampled horizontally to `dw` luma columns (and
/// (dw + 1) / 2 chroma columns), tightly packed. Each output sample is the average of the source
/// samples its span covers, weighted by the overlap.
fn resample_planes(v: &VideoFrame, w: usize, h: usize, dw: usize) -> [Vec<u8>; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let resample = |src: &[u8], sw: usize, rows: usize, ow: usize| -> Vec<u8> {
        let mut out = Vec::with_capacity(ow * rows);
        let scale = sw as f64 / ow as f64;
        for y in 0..rows {
            let s = &src[y * sw..(y + 1) * sw];
            for x in 0..ow {
                let (a, b) = (x as f64 * scale, (x + 1) as f64 * scale);
                let (mut acc, mut total) = (0.0, 0.0);
                for (i, &p) in s.iter().enumerate().take((b.ceil() as usize).min(sw)).skip(a.floor() as usize) {
                    let overlap = (i as f64 + 1.0).min(b) - (i as f64).max(a);
                    if overlap > 0.0 {
                        acc += overlap * p as f64;
                        total += overlap;
                    }
                }
                out.push((acc / total).round() as u8);
            }
        }
        out
    };
    let dcw = dw.div_ceil(2);
    [resample(&v.planes[0], w, h, dw), resample(&v.planes[1][..cw * ch], cw, ch, dcw), resample(&v.planes[2][..cw * ch], cw, ch, dcw)]
}

impl Encoder for Av1Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the AV1 encoder received an audio frame"));
        };
        self.pending.push_back(v.clone());
        self.drain(false)
    }

    /// Codes the frames still waiting for their ALTREF group (see `with_altref`).
    fn flush(&mut self) -> Result<Vec<Packet>> {
        self.drain(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::Av1Decoder;
    use ironav_core::{Decoder, Frame as CoreFrame};

    /// A test picture: gradients, a diagonal edge and a little noise, so that every mode is used.
    /// `shift` moves the picture to the right and down by that many samples, for a moving clip.
    fn picture_shifted(w: usize, h: usize, shift: usize) -> VideoFrame {
        let mut seed = 1u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 16) % 9) as i32 - 4
        };
        let luma: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w + w - shift.min(w)) % w, (i / w + h - shift.min(h)) % h);
                let v = if x + y > (w + h) / 2 { 200 } else { (x * 2 + y) as i32 } + noise();
                v.clamp(0, 255) as u8
            })
            .collect();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let chroma = |k: usize| -> Vec<u8> { (0..cw * ch).map(|i| ((i % cw) * 3 + (i / cw) * k) as u8).collect() };
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![luma, chroma(1), chroma(2)] }
    }

    fn picture(w: usize, h: usize) -> VideoFrame {
        picture_shifted(w, h, 0)
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    fn params(w: usize, h: usize) -> StreamParams {
        StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: ironav_core::Rational { num: 25, den: 1 } }
    }

    /// The Y plane of a decoded frame.
    fn luma_of(f: CoreFrame) -> Vec<u8> {
        let CoreFrame::Video(out) = f else { panic!("no picture") };
        out.planes[0][..out.width as usize * out.height as usize].to_vec()
    }

    /// A picture through the encoder and IronAV's own decoder comes back close to the source, at
    /// sizes with and without a partial superblock.
    #[test]
    fn round_trips_through_the_decoder() {
        for &(w, h) in &[(64usize, 48usize), (177, 99)] {
            let src = picture(w, h);
            let mut enc = Av1Encoder::new(&params(w, h), Some(20), None).unwrap();
            let data = enc.encode_frame(&src).unwrap();
            let mut dec = Av1Decoder::new(&[]);
            let mut frames = dec.decode(&Packet { stream: 0, pts: 0, duration: 1, data }).unwrap();
            frames.extend(dec.flush().unwrap());
            let p = psnr(&src.planes[0], &luma_of(frames.remove(0)));
            assert!(p > 35.0, "{w}x{h}: luma PSNR {p:.1} dB");
        }
    }

    /// Key and inter frames: each decoded frame equals the encoder's own reconstruction, bit for bit,
    /// and the moving clip is coded with inter frames at a useful quality.
    #[test]
    fn inter_frames_decode_to_the_encoders_reconstruction() {
        for &(w, h, nrefs, compound) in &[(96usize, 64usize, 1usize, false), (177, 99, 1, false), (96, 64, 3, false), (177, 99, 3, false), (96, 64, 3, true), (177, 99, 3, true)] {
            let mut enc = Av1Encoder::new(&params(w, h), Some(24), None).unwrap().with_keyint(4).with_refs(nrefs).with_compound(compound);
            let mut dec = Av1Decoder::new(&[]);
            for t in 0..6 {
                let src = picture_shifted(w, h, 2 * t);
                let data = enc.encode_frame(&src).unwrap();
                let recon = enc.last().0.planes[0].clone();
                let mut frames = dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data }).unwrap();
                assert_eq!(frames.len(), 1, "frame {t} shows one picture");
                let got = luma_of(frames.remove(0));
                let want: Vec<u8> = (0..w * h).map(|i| recon.at(i % w, i / w) as u8).collect();
                assert!(got == want, "{w}x{h} frame {t}: decoder differs from the encoder's reconstruction");
                if t % 2 == 1 {
                    let p = psnr(&src.planes[0], &got);
                    assert!(p > 30.0, "{w}x{h} frame {t}: luma PSNR {p:.1} dB");
                }
            }
        }
    }

    /// Superres: frames coded at a downscaled width decode to the encoder's reconstruction, all
    /// three planes, for key and inter frames at each denominator and at an odd width.
    #[test]
    fn superres_frames_decode_to_the_encoders_reconstruction() {
        for &(denom, restoration) in &[(9u32, false), (12, false), (16, false), (9, true), (12, true), (16, true)] {
            for &(w, h) in &[(352usize, 288usize), (177, 99)] {
                let mut enc = Av1Encoder::new(&params(w, h), Some(24), None).unwrap().with_keyint(3).with_superres(denom);
                if restoration {
                    enc = enc.with_restoration();
                }
                let mut dec = Av1Decoder::new(&[]);
                for t in 0..4 {
                    let src = picture_shifted(w, h, 2 * t);
                    let data = enc.encode_frame(&src).unwrap();
                    let recon = enc.last().0.clone();
                    let mut frames = dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data }).unwrap();
                    assert_eq!(frames.len(), 1, "frame {t} shows one picture");
                    let CoreFrame::Video(out) = frames.remove(0) else { panic!("no picture") };
                    assert_eq!((out.width as usize, out.height as usize), (w, h), "the output is the upscaled size");
                    for pl in 0..3 {
                        let (pw, ph) = if pl == 0 { (w, h) } else { (w.div_ceil(2), h.div_ceil(2)) };
                        let want: Vec<u8> = (0..pw * ph).map(|i| recon.planes[pl].at(i % pw, i / pw) as u8).collect();
                        assert!(out.planes[pl] == want, "1/{denom} {w}x{h} frame {t} plane {pl}: decoder differs from the encoder's reconstruction");
                    }
                    let p = psnr(&src.planes[0], &out.planes[0]);
                    assert!(p > 20.0, "1/{denom} {w}x{h} frame {t}: luma PSNR {p:.1} dB");
                }
            }
        }
    }

    /// With `with_altref`, the packets come one per frame, in display order with each frame's time, and
    /// decode to the source's frames in order: the hidden ALTREFs are shown by show_existing_frame
    /// headers, and the frames between predict from them.
    #[test]
    fn altref_groups_decode_in_display_order() {
        let (w, h) = (96usize, 64usize);
        for &lag in &[2usize, 4, 6] {
            let mut enc = Av1Encoder::new(&params(w, h), Some(24), None).unwrap().with_keyint(9).with_altref(lag);
            let mut packets = Vec::new();
            for t in 0..11 {
                let mut src = picture_shifted(w, h, 2 * t);
                src.pts = t as i64;
                packets.extend(enc.encode(&CoreFrame::Video(src)).unwrap());
            }
            packets.extend(enc.flush().unwrap());
            assert_eq!(packets.len(), 11, "lag {lag}: one packet per frame");
            for (t, p) in packets.iter().enumerate() {
                assert_eq!(p.pts, t as i64, "lag {lag}: packet {t} has its frame's time");
            }
            let mut dec = Av1Decoder::new(&[]);
            let mut got = Vec::new();
            for p in &packets {
                got.extend(dec.decode(p).unwrap());
            }
            got.extend(dec.flush().unwrap());
            assert_eq!(got.len(), 11, "lag {lag}: one picture per frame");
            for (t, f) in got.into_iter().enumerate() {
                let p = psnr(&picture_shifted(w, h, 2 * t).planes[0], &luma_of(f));
                assert!(p > 30.0, "lag {lag} frame {t}: luma PSNR {p:.1} dB");
            }
        }
    }

    /// A measurement, not a test (run with `--ignored --release`): encodes the raw yuv420p file named by
    /// IRONAV_BENCH_YUV (IRONAV_BENCH_SIZE "WxH", IRONAV_BENCH_QP, IRONAV_BENCH_LAG: 1 for no ALTREF) and
    /// prints the bytes, the mean luma PSNR of the decoded frames against the source in display order,
    /// and the encode and decode times. IRONAV_BENCH_OUT, when set, is a path prefix: the stream is written
    /// there as `.ivf` and the decoded frames as `.yuv`, for FFmpeg's libaom to check against.
    #[test]
    #[ignore]
    fn bench_altref() {
        let Ok(path) = std::env::var("IRONAV_BENCH_YUV") else {
            return;
        };
        let size = std::env::var("IRONAV_BENCH_SIZE").unwrap_or("1280x720".into());
        let (w, h) = size.split_once('x').map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap())).unwrap();
        let qp: u8 = std::env::var("IRONAV_BENCH_QP").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let lag: usize = std::env::var("IRONAV_BENCH_LAG").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        let raw = std::fs::read(&path).unwrap();
        let fsize = w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
        let frames: Vec<VideoFrame> = raw
            .chunks_exact(fsize)
            .enumerate()
            .map(|(t, c)| {
                let (y, rest) = c.split_at(w * h);
                let (u, v) = rest.split_at(w.div_ceil(2) * h.div_ceil(2));
                VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y.to_vec(), u.to_vec(), v.to_vec()] }
            })
            .collect();
        let mut enc = Av1Encoder::new(&params(w, h), Some(qp), None).unwrap().with_altref(lag);
        let t0 = std::time::Instant::now();
        let mut packets = Vec::new();
        for f in &frames {
            packets.extend(enc.encode(&CoreFrame::Video(f.clone())).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let enc_s = t0.elapsed().as_secs_f64();
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        let t1 = std::time::Instant::now();
        let mut dec = Av1Decoder::new(&[]);
        let mut got = Vec::new();
        for p in &packets {
            got.extend(dec.decode(p).unwrap());
        }
        got.extend(dec.flush().unwrap());
        let dec_s = t1.elapsed().as_secs_f64();
        assert_eq!(got.len(), frames.len(), "one decoded picture per source frame");
        let mut sum = 0.0;
        let mut decoded = Vec::new();
        for (f, src) in got.into_iter().zip(&frames) {
            let CoreFrame::Video(out) = f else { panic!("no picture") };
            sum += psnr(&src.planes[0], &out.planes[0][..w * h]);
            for pl in &out.planes {
                decoded.extend_from_slice(pl);
            }
        }
        let n = frames.len() as f64;
        println!(
            "IRONAV_BENCH {size} qp={qp} lag={lag} frames={} bytes={bytes} luma_psnr={:.3} enc_fps={:.2} dec_fps={:.2}",
            frames.len(),
            sum / n,
            n / enc_s,
            n / dec_s
        );
        if let Ok(prefix) = std::env::var("IRONAV_BENCH_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"AV01");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&25u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for p in &packets {
                ivf.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(p.pts as u64).to_le_bytes());
                ivf.extend_from_slice(&p.data);
            }
            std::fs::write(format!("{prefix}.ivf"), ivf).unwrap();
            std::fs::write(format!("{prefix}.yuv"), decoded).unwrap();
        }
    }
}
