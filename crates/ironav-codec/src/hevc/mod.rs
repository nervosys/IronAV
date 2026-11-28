//! A software HEVC (H.265) decoder: Main, Main 10 and the range extension profiles' chroma formats
//! (monochrome, 4:2:0, 4:2:2 and 4:4:4, 8 to 12 bits), with tiles, wavefronts, multiple slices, sample
//! adaptive offset, the deblocking filter and inter prediction (merge and AMVP, temporal candidates,
//! weighted prediction). The range extension coding tools (RDPCM, cross-component prediction and the
//! rest) are not implemented; a stream that turns one on is refused, naming it.
//!
//! Several pictures are decoded at once (frame threading). The caller's thread does the bitstream work:
//! NAL units, parameter sets, slice headers, picture order counts, reference picture sets and lists,
//! the decoded picture buffer and the output order. Each picture's slice data then goes to a thread of
//! its own, which decodes it, runs the loop filters a CTB row behind and publishes the rows as they become
//! final (see `shared`); a picture that refers to it waits only for the rows its motion vectors reach.

#![allow(dead_code)]

mod cabac;
mod filter;
mod frame;
mod inter;
mod intra;
mod enc;
pub use enc::HevcEncoder;
pub(crate) mod ps;
mod shared;
mod slice;
mod tables;
mod transform;

use crate::h264::bits::{split_annexb, unescape};
use filter::RowFilter;
use frame::{CurPic, Geometry, SliceInfo};
use ironav_core::{ColorInfo, Decoder, Error, ErrorCode, Frame as OutFrame, HdrMetadata, Packet, PixelFormat, Result, VideoFrame};
use ps::{parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps};
use shared::{BufPool, Publisher, SharedFrame};
use slice::{active_scaling_list, Carry, Dec, SliceCtx};
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;
use transform::ScalingFactors;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

/// Pictures decoded at once by default, at most.
const MAX_THREADS: usize = 16;

/// A reference picture of a slice's list.
#[derive(Clone)]
pub struct RefPic {
    pub frame: Arc<SharedFrame>,
    pub poc: i32,
    pub long_term: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marking {
    Unused,
    Short,
    Long,
}

struct DpbEntry {
    frame: Arc<SharedFrame>,
    marking: Marking,
    needed_for_output: bool,
}

/// Where a picture's finished rows go.
enum Out {
    /// The loop filters follow the decoding row by row.
    Follow(RowFilter),
    /// The picture is filtered whole after it is decoded (tiles in several columns; samples exempt from
    /// the loop filters).
    Whole(Publisher),
}

/// A slice segment queued for the thread that decodes its picture: header, RBSP and reference lists.
type SliceJob = (SliceHeader, Vec<u8>, [Vec<RefPic>; 2]);

/// The pixel work of one picture: slice data decoding and the loop filters. It runs on whichever thread
/// the picture is given to.
struct Work {
    pic: CurPic,
    sps: Arc<Sps>,
    pps: Arc<Pps>,
    geo: Arc<Geometry>,
    scaling: Option<Arc<ScalingFactors>>,
    poc: i32,
    carry: Carry,
    slice_idx: u16,
    slice_addr_rs: i32,
    out: Out,
    pool: Arc<BufPool>,
}

impl Work {
    fn slice(&mut self, hdr: &SliceHeader, rbsp: &[u8], refs: &[Vec<RefPic>; 2]) -> Result<()> {
        if !hdr.dependent {
            self.slice_idx = self.pic.slices.len() as u16;
            self.slice_addr_rs = hdr.segment_address as i32;
            self.pic.slices.push(SliceInfo {
                deblocking_disabled: hdr.deblocking_disabled,
                beta_offset_div2: hdr.beta_offset_div2,
                tc_offset_div2: hdr.tc_offset_div2,
                loop_filter_across_slices: hdr.loop_filter_across_slices,
                cb_qp_offset: hdr.cb_qp_offset,
                cr_qp_offset: hdr.cr_qp_offset,
            });
        }
        let Work { pic, sps, pps, geo, scaling, poc, carry, slice_idx, slice_addr_rs, out, .. } = self;
        let (sps, pps, geo) = (&**sps, &**pps, &**geo);
        let ctx = SliceCtx { sps, pps, hdr, geo, scaling: scaling.as_deref(), slice_idx: *slice_idx, slice_addr_rs: *slice_addr_rs, refs, poc: *poc };
        let mut hook = |p: &CurPic, rows: usize| {
            if let Out::Follow(rf) = out {
                rf.rows_decoded(p, sps, pps, geo, rows);
            }
        };
        let mut dec = Dec::new(&ctx, pic, rbsp, carry)?.with_row_hook(&mut hook);
        dec.run(carry)
    }

    /// Filters what is left, publishes the whole picture and builds its output frame.
    fn finish(self) {
        let Work { mut pic, sps, pps, geo, out, pool, .. } = self;
        match out {
            Out::Follow(mut rf) => {
                rf.finish(&pic, &sps, &pps, &geo);
                // the output frame too, here rather than on the caller's thread
                rf.out().pic().set_output(output(rf.out().pic()));
                pic.frame.snap = rf.into_snap();
            }
            Out::Whole(mut publ) => {
                filter::run(&mut pic, &sps, &pps, &geo);
                for (t, p) in publ.tails().into_iter().zip(&pic.frame.planes) {
                    t.copy_from_slice(p);
                }
                publ.advance(sps.height, &pic.frame.mv);
                publ.pic().set_output(output(publ.pic()));
            }
        }
        pool.give_work(pic.frame);
    }
}

/// The work of a picture decoded on a thread of its own: its slices, then the end of the picture. The
/// first error of a slice is returned. If this panics, the picture is published as it is (its publisher
/// is dropped), so no reader waits for it forever.
fn run_picture(mut work: Work, slices: Vec<SliceJob>) -> Option<Error> {
    let mut err = None;
    for (hdr, rbsp, refs) in &slices {
        if let Err(e) = work.slice(hdr, rbsp, refs) {
            err.get_or_insert(e);
        }
    }
    work.finish();
    err
}

/// The output frame of a finished picture: the conformance window of its samples as bytes (little-endian
/// 16-bit words above 8 bits).
fn output(f: &SharedFrame) -> VideoFrame {
    let [cl, cr, ct, cb] = f.crop;
    let (w, h) = (f.width - cl - cr, f.height - ct - cb);
    let wide = f.bit_depth > 8;
    let put = |v: &mut Vec<u8>, src: &[u16]| {
        let start = v.len();
        if wide {
            v.resize(start + src.len() * 2, 0);
            for (d, &s) in v[start..].as_chunks_mut::<2>().0.iter_mut().zip(src) {
                *d = s.to_le_bytes();
            }
        } else {
            v.resize(start + src.len(), 0);
            for (d, &s) in v[start..].iter_mut().zip(src) {
                *d = s as u8;
            }
        }
    };
    let (sx, sy) = match f.chroma_format {
        1 => (1, 1),
        2 => (1, 0),
        _ => (0, 0),
    };
    let gray = f.chroma_format == 0 && !wide;
    let mut planes = Vec::with_capacity(3);
    let mut y = Vec::with_capacity(w * h);
    let luma = f.full_plane(0);
    for r in 0..h {
        put(&mut y, &luma[(ct + r) * f.width + cl..(ct + r) * f.width + cl + w]);
    }
    planes.push(y);
    if f.chroma_format == 0 {
        if !gray {
            // monochrome deeper than 8 bits goes out as 4:2:0 with flat chroma
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let flat = 1u16 << (f.bit_depth - 1);
            for _ in 0..2 {
                let mut v = Vec::new();
                put(&mut v, &vec![flat; cw * ch]);
                planes.push(v);
            }
        }
    } else {
        let (cw, ch) = (w >> sx, h >> sy);
        for pl in 1..3 {
            let src = f.full_plane(pl);
            let mut v = Vec::with_capacity(cw * ch);
            for r in 0..ch {
                let o = ((ct >> sy) + r) * f.cwidth + (cl >> sx);
                put(&mut v, &src[o..o + cw]);
            }
            planes.push(v);
        }
    }
    let layout = match f.chroma_format {
        0 | 1 => PixelFormat::Yuv420p,
        2 => PixelFormat::Yuv422p,
        _ => PixelFormat::Yuv444p,
    };
    let pix_fmt = if gray { PixelFormat::Gray8 } else { layout.with_depth(f.bit_depth as u8).expect("8, 10 or 12 bits") };
    VideoFrame { width: w as u32, height: h as u32, pix_fmt, pts: f.pts, planes }
}

struct Cur {
    /// The picture's decoding state: used at once with one thread, else handed to a thread of its own
    /// when the picture ends.
    work: Work,
    /// The slices to decode on that thread.
    slices: Vec<SliceJob>,
    frame: Arc<SharedFrame>,
    sps: Arc<Sps>,
    poc: i32,
    output: bool,
    /// The reference lists of the latest slice (a dependent slice segment goes on with them).
    refs: [Vec<RefPic>; 2],
    st_before: Vec<i32>,
    st_after: Vec<i32>,
    lt_curr: Vec<i32>,
}

pub struct HevcDecoder {
    /// Pictures decoded at once, each on a thread of its own; 1 decodes on the calling thread.
    threads: usize,
    /// Pictures being decoded on other threads, in decoding order.
    running: VecDeque<JoinHandle<Option<Error>>>,
    /// Pictures in output order, waiting for their decoding to finish.
    ready: VecDeque<Arc<SharedFrame>>,
    /// An error from a picture decoded on another thread, for the next call to return.
    failed: Option<Error>,
    /// Buffers of pictures no longer needed, for the next ones.
    bufs: Arc<BufPool>,
    /// Whether the loop filters follow the decoding where they can (they do, but for tests).
    follow: bool,
    sps: Vec<Option<Arc<Sps>>>,
    pps: Vec<Option<Arc<Pps>>>,
    cur: Option<Cur>,
    dpb: Vec<DpbEntry>,
    out: Vec<OutFrame>,
    prev_tid0_poc: i32,
    first_picture: bool,
    after_eos: bool,
    /// NoRaslOutputFlag of the last IRAP picture.
    no_rasl_output: bool,
    skipping_picture: bool,
    last_hdr: Option<SliceHeader>,
    geo_cache: Option<(usize, usize, Arc<Geometry>)>,
    /// The colour description of the sequence parameter set the latest picture used.
    color: Option<ColorInfo>,
    /// HDR metadata from the SEI messages seen so far.
    hdr: HdrMetadata,
}

impl Default for HevcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcDecoder {
    /// Decodes up to as many pictures at once as there are cores, at most 16. Pictures then come out a few
    /// calls later than with one thread.
    pub fn new() -> Self {
        Self::with_threads(std::thread::available_parallelism().map_or(1, |n| n.get()).min(MAX_THREADS))
    }

    /// Decodes up to `threads` pictures at once, each on a thread of its own; 1 decodes on the calling thread.
    pub fn with_threads(threads: usize) -> Self {
        HevcDecoder {
            threads: threads.max(1),
            running: VecDeque::new(),
            ready: VecDeque::new(),
            failed: None,
            bufs: Arc::new(BufPool::default()),
            follow: true,
            sps: vec![None; 16],
            pps: vec![None; 64],
            cur: None,
            dpb: Vec::new(),
            out: Vec::new(),
            prev_tid0_poc: 0,
            first_picture: true,
            after_eos: false,
            no_rasl_output: true,
            skipping_picture: false,
            last_hdr: None,
            geo_cache: None,
            color: None,
            hdr: HdrMetadata::default(),
        }
    }

    /// For live and low-latency sources: decodes on the calling thread, so that no picture waits for
    /// others and each comes out on the call that completes it, as far as the stream's declared reordering
    /// allows. The loop filters still follow the decoding row by row. Same as `with_threads(1)`.
    pub fn low_delay() -> Self {
        Self::with_threads(1)
    }

    /// Makes the loop filters wait for the whole picture instead of following the decoding row by row, as
    /// they must for some streams. The pictures are the same; this is for tests that check it.
    #[doc(hidden)]
    pub fn with_whole_picture_filters(mut self) -> Self {
        self.follow = false;
        self
    }

    // ------------------------------------------------------------ output

    /// Outputs the pictures due whose decoding has finished, or all of them, waiting for them.
    fn emit_ready(&mut self, wait: bool) {
        while let Some(f) = self.ready.front() {
            if !wait && !f.is_complete() {
                break;
            }
            let f = self.ready.pop_front().unwrap();
            let v = f.take_output().unwrap_or_else(|| output(&f));
            self.out.push(OutFrame::Video(v));
        }
    }

    /// Outputs the picture with the smallest POC among those waiting.
    fn bump(&mut self) -> bool {
        let Some(i) = self.dpb.iter().enumerate().filter(|(_, e)| e.needed_for_output).min_by_key(|(_, e)| e.frame.poc).map(|(i, _)| i) else {
            return false;
        };
        self.dpb[i].needed_for_output = false;
        self.ready.push_back(self.dpb[i].frame.clone());
        true
    }

    /// Drops the pictures that are neither referenced nor waiting for output; their buffers go back to the pool
    /// once nothing else (a picture being decoded, the output queue) holds them.
    fn prune_dpb(&mut self) {
        self.dpb.retain(|e| e.marking != Marking::Unused || e.needed_for_output);
    }

    fn waiting(&self) -> usize {
        self.dpb.iter().filter(|e| e.needed_for_output).count()
    }

    /// Waits for the oldest picture decoded on another thread and keeps its error.
    fn join_oldest(&mut self) {
        let Some(h) = self.running.pop_front() else { return };
        match h.join() {
            Ok(err) => {
                if let Some(e) = err {
                    self.failed.get_or_insert(e);
                }
            }
            Err(_) => {
                self.failed.get_or_insert(Error::new(ErrorCode::Internal, "an HEVC decoding thread panicked"));
            }
        }
    }

    // ------------------------------------------------------------ picture start and end

    fn finish_picture(&mut self) {
        let Some(c) = self.cur.take() else { return };
        if self.threads <= 1 {
            c.work.finish();
        } else {
            while self.running.len() >= self.threads {
                self.join_oldest();
            }
            let (work, slices) = (c.work, c.slices);
            match std::thread::Builder::new().name("hevc".into()).stack_size(8 << 20).spawn(move || run_picture(work, slices)) {
                Ok(h) => self.running.push_back(h),
                Err(e) => {
                    self.failed.get_or_insert(Error::new(ErrorCode::Internal, format!("could not start an HEVC decoding thread: {e}")));
                }
            }
        }
        self.dpb.push(DpbEntry { frame: c.frame, marking: Marking::Short, needed_for_output: c.output });
        let max_reorder = c.sps.max_num_reorder as usize;
        while self.waiting() > max_reorder {
            self.bump();
        }
        self.emit_ready(false);
    }

    fn missing_frame(&self, sps: &Sps, poc: i32) -> SharedFrame {
        SharedFrame::new(&self.bufs, sps, poc, 0, true)
    }

    /// Reference picture set derivation and marking (8.3.2). Returns the POCs of the pictures the
    /// current one may refer to: before, after and long-term.
    fn apply_rps(&mut self, sps: &Sps, hdr: &SliceHeader, poc: i32) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let (mut before, mut after, mut lt_curr) = (Vec::new(), Vec::new(), Vec::new());
        let mut keep_short = Vec::new();
        let mut keep_long: Vec<(i32, bool)> = Vec::new();
        if !hdr.idr() {
            for (i, &d) in hdr.st_rps.delta_poc_s0.iter().enumerate() {
                keep_short.push(poc + d);
                if hdr.st_rps.used_s0[i] {
                    before.push(poc + d);
                }
            }
            for (i, &d) in hdr.st_rps.delta_poc_s1.iter().enumerate() {
                keep_short.push(poc + d);
                if hdr.st_rps.used_s1[i] {
                    after.push(poc + d);
                }
            }
            for &(v, msb, used) in &hdr.lt {
                let full = if msb { v + (poc & !(max_lsb - 1)) } else { v };
                keep_long.push((full, msb));
                if used {
                    lt_curr.push((full, msb));
                }
            }
        }
        let mut long_pocs = Vec::new();
        for &(v, msb) in &keep_long {
            if let Some(e) = self.dpb.iter().find(|e| e.marking != Marking::Unused && if msb { e.frame.poc == v } else { (e.frame.poc & (max_lsb - 1)) == v }) {
                long_pocs.push(e.frame.poc);
            }
        }
        for e in self.dpb.iter_mut() {
            let p = e.frame.poc;
            if long_pocs.contains(&p) {
                e.marking = Marking::Long;
            } else if e.marking == Marking::Short && keep_short.contains(&p) {
            } else {
                e.marking = Marking::Unused;
            }
        }
        let lt_full: Vec<i32> = lt_curr
            .iter()
            .map(|&(v, msb)| self.dpb.iter().find(|e| e.marking == Marking::Long && if msb { e.frame.poc == v } else { (e.frame.poc & (max_lsb - 1)) == v }).map_or(v, |e| e.frame.poc))
            .collect();
        (before, after, lt_full)
    }

    fn find_ref(&mut self, sps: &Sps, poc: i32, long: bool) -> RefPic {
        if let Some(e) = self.dpb.iter().find(|e| e.frame.poc == poc && e.marking != Marking::Unused) {
            return RefPic { frame: e.frame.clone(), poc, long_term: long };
        }
        // a reference the stream does not have (after random access, or damage): substitute grey
        let frame = Arc::new(self.missing_frame(sps, poc));
        self.dpb.push(DpbEntry { frame: frame.clone(), marking: if long { Marking::Long } else { Marking::Short }, needed_for_output: false });
        RefPic { frame, poc, long_term: long }
    }

    fn build_ref_lists(&mut self, sps: &Sps, hdr: &SliceHeader) -> [Vec<RefPic>; 2] {
        let c = self.cur.as_ref().unwrap();
        let (before, after, lt) = (c.st_before.clone(), c.st_after.clone(), c.lt_curr.clone());
        let mut lists: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if hdr.slice_type == SliceType::I {
            return lists;
        }
        for l in 0..if hdr.slice_type == SliceType::B { 2 } else { 1 } {
            let (first, second) = if l == 0 { (&before, &after) } else { (&after, &before) };
            let total = first.len() + second.len() + lt.len();
            let n = hdr.num_ref_idx[l].max(total);
            let mut temp: Vec<(i32, bool)> = Vec::new();
            if total > 0 {
                while temp.len() < n {
                    for &p in first.iter() {
                        if temp.len() < n {
                            temp.push((p, false));
                        }
                    }
                    for &p in second.iter() {
                        if temp.len() < n {
                            temp.push((p, false));
                        }
                    }
                    for &p in lt.iter() {
                        if temp.len() < n {
                            temp.push((p, true));
                        }
                    }
                }
            }
            for i in 0..hdr.num_ref_idx[l] {
                let idx = match &hdr.list_mod[l] {
                    Some(m) => m[i] as usize,
                    None => i,
                };
                if let Some(&(poc, long)) = temp.get(idx) {
                    let r = self.find_ref(sps, poc, long);
                    lists[l].push(r);
                }
            }
        }
        lists
    }

    fn start_picture(&mut self, nal_type: u8, tid: u8, hdr: &SliceHeader, pts: i64) -> Result<()> {
        let pps = self.pps[hdr.pps_id].clone().ok_or_else(|| unsupported("missing PPS"))?;
        let sps = self.sps[pps.sps_id].clone().ok_or_else(|| unsupported("missing SPS"))?;
        if sps.bit_depth_luma != sps.bit_depth_chroma || !(8..=12).contains(&sps.bit_depth_luma) {
            return Err(unsupported("this HEVC decoder handles equal luma and chroma bit depths of 8 to 12 only"));
        }
        if let Some(tool) = sps.range_tool.or(pps.range_tool) {
            return Err(unsupported(&format!("this HEVC decoder does not implement the range extension tool \"{tool}\"")));
        }
        let irap = hdr.irap();
        if irap {
            self.no_rasl_output = matches!(nal_type, 16..=20) || self.first_picture || self.after_eos;
        }
        // RASL pictures of a random access point that starts the stream cannot be decoded
        if matches!(nal_type, 8 | 9) && self.no_rasl_output {
            self.skipping_picture = true;
            return Ok(());
        }
        self.skipping_picture = false;
        // picture order count (8.3.1)
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = hdr.poc_lsb as i32;
        let msb = if irap && self.no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;
        if tid == 0 && !matches!(nal_type, 6..=9) && !(nal_type <= 14 && nal_type.is_multiple_of(2)) {
            self.prev_tid0_poc = poc;
        }
        if irap && self.no_rasl_output && !self.first_picture {
            // a new coded video sequence (C.5.2.2): what is still waiting goes out first, unless
            // the picture says to drop it (no_output_of_prior_pics_flag, taken as set for a CRA
            // after an end of sequence, as the HM and FFmpeg take it)
            let drop = hdr.no_output_of_prior_pics || (nal_type == 21 && self.after_eos);
            if !drop {
                while self.bump() {}
            }
            self.dpb.clear();
        }
        self.first_picture = false;
        self.after_eos = false;
        let (before, after, lt) = self.apply_rps(&sps, hdr, poc);
        // free what is neither referenced nor waiting for output, then make room by outputting
        self.prune_dpb();
        while self.waiting() > sps.max_num_reorder as usize || (self.dpb.len() >= sps.max_dec_pic_buffering as usize && self.waiting() > 0) {
            if !self.bump() {
                break;
            }
            self.prune_dpb();
        }
        let geo = match &self.geo_cache {
            Some((s, p, g)) if *s == sps.id && *p == pps.id => g.clone(),
            _ => {
                let g = Arc::new(Geometry::new(&sps, &pps));
                self.geo_cache = Some((sps.id, pps.id, g.clone()));
                g
            }
        };
        let scaling = active_scaling_list(&sps, &pps).map(|l| Arc::new(ScalingFactors::new(&l)));
        let output = hdr.pic_output;
        self.color = sps.color;
        let mut pic = CurPic::reusing(&sps, pts, poc, self.bufs.take_work(|_| true));
        let (frame, publisher) = Publisher::new(SharedFrame::new(&self.bufs, &sps, poc, pts, false));
        let out = if self.follow && filter::can_follow(&sps, &pps) { Out::Follow(RowFilter::new(publisher, std::mem::take(&mut pic.frame.snap))) } else { Out::Whole(publisher) };
        let work = Work { pic, sps: sps.clone(), pps, geo, scaling, poc, carry: Carry::default(), slice_idx: 0, slice_addr_rs: 0, out, pool: self.bufs.clone() };
        self.cur = Some(Cur { work, slices: Vec::new(), frame, sps, poc, output, refs: [Vec::new(), Vec::new()], st_before: before, st_after: after, lt_curr: lt });
        Ok(())
    }

    fn slice(&mut self, nal_type: u8, tid: u8, rbsp: &[u8], pts: i64) -> Result<()> {
        let hdr = parse_slice_header(rbsp, nal_type, tid, &self.sps, &self.pps, self.last_hdr.as_ref())?;
        if hdr.first_slice_segment {
            self.finish_picture();
            self.start_picture(nal_type, tid, &hdr, pts)?;
        }
        if self.skipping_picture || self.cur.is_none() {
            return Ok(());
        }
        self.last_hdr = Some(hdr.clone());
        if !hdr.dependent {
            let sps = self.cur.as_ref().unwrap().sps.clone();
            let refs = self.build_ref_lists(&sps, &hdr);
            self.cur.as_mut().unwrap().refs = refs;
        }
        let c = self.cur.as_mut().unwrap();
        if self.threads <= 1 {
            c.work.slice(&hdr, rbsp, &c.refs)
        } else {
            c.slices.push((hdr, rbsp.to_vec(), c.refs.clone()));
            Ok(())
        }
    }
}

impl Decoder for HevcDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<OutFrame>> {
        let mut err = None;
        for nal in split_annexb(&packet.data) {
            // an end of sequence or of bitstream is the 2-byte header alone
            if nal.len() < 2 {
                continue;
            }
            let ty = (nal[0] >> 1) & 63;
            let layer = ((nal[0] & 1) << 5) | (nal[1] >> 3);
            let tid = (nal[1] & 7).saturating_sub(1);
            if layer != 0 {
                continue;
            }
            let rbsp = unescape(&nal[2..]);
            let r = match ty {
                33 => parse_sps(&rbsp).map(|s| {
                    let id = s.id;
                    self.sps[id] = Some(Arc::new(s));
                    self.geo_cache = None;
                }),
                34 => parse_pps(&rbsp).map(|p| {
                    let id = p.id;
                    self.pps[id] = Some(Arc::new(p));
                    self.geo_cache = None;
                }),
                // prefix and suffix SEI: a malformed message is ignored, never an error
                39 | 40 => {
                    crate::hdr::parse_sei(&rbsp, &mut self.hdr);
                    Ok(())
                }
                36 | 37 => {
                    self.finish_picture();
                    self.after_eos = true;
                    Ok(())
                }
                0..=9 | 16..=21 => self.slice(ty, tid, &rbsp, packet.pts),
                _ => Ok(()),
            };
            if let Err(e) = r {
                err = Some(e);
                break;
            }
        }
        // a packet holds whole access units, so a complete picture is ready now (decoding on other
        // threads, the picture starts when the next one does)
        if self.threads <= 1 {
            if let Some(c) = &self.cur {
                if c.work.pic.ctbs_done == c.sps.ctb_w * c.sps.ctb_h {
                    self.finish_picture();
                }
            }
        }
        while self.running.front().is_some_and(|h| h.is_finished()) {
            self.join_oldest();
        }
        self.emit_ready(false);
        if let Some(e) = err.or_else(|| self.failed.take()) {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<OutFrame>> {
        self.finish_picture();
        while !self.running.is_empty() {
            self.join_oldest();
        }
        while self.bump() {}
        self.emit_ready(true);
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn reset(&mut self) -> bool {
        // the pictures on other threads end (they never wait for anything that is not decoding)
        self.cur = None;
        while !self.running.is_empty() {
            self.join_oldest();
        }
        self.failed = None;
        self.ready.clear();
        self.dpb.clear();
        self.out.clear();
        self.prev_tid0_poc = 0;
        self.first_picture = true;
        self.after_eos = false;
        self.no_rasl_output = true;
        self.skipping_picture = false;
        self.last_hdr = None;
        true
    }

    fn color_info(&self) -> Option<ColorInfo> {
        self.color
    }

    fn hdr_metadata(&self) -> Option<HdrMetadata> {
        Some(self.hdr).filter(|h| !h.is_empty())
    }
}
