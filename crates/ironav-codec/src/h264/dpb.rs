//! Picture order count, reference picture marking and reference list construction
//! (ITU-T H.264 8.2.1, 8.2.4 and 8.2.5), for frame pictures.

use super::mb::Spare;
use super::ps::{Marking, SliceHeader, SliceType, Sps};
use super::shared::Published;
use ironav_core::{Error, Result, VideoFrame};
use std::sync::{Arc, Mutex};

/// A decoded, loop-filtered picture, kept for reference and for output. While it is decoded on
/// another thread, its rows become readable as they are finished.
pub struct DecodedPic {
    pub data: Published,
    /// The output frame, when the thread that decoded the picture made it.
    pub frame: Mutex<Option<VideoFrame>>,
    pub mb_w: usize,
    pub mb_h: usize,
    /// chroma_format_idc: 0 monochrome, 1 4:2:0, 2 4:2:2.
    pub cf: u8,
    /// Width of a macroblock's chroma samples.
    pub cw: usize,
    pub poc: i32,
    /// The order counts of the top and bottom fields; `poc` is the lesser.
    pub field_poc: [i32; 2],
    /// Unique across the stream; deblocking and direct prediction compare references by it.
    pub id: i32,
    /// Cropping (left, right, top, bottom) in luma samples, applied when the picture is output.
    pub crop: [usize; 4],
    pub bit_depth: u32,
}

/// One entry of a reference picture list.
#[derive(Clone)]
pub struct RefEntry {
    pub pic: Arc<DecodedPic>,
    pub long: bool,
}

pub type RefList = Vec<Option<RefEntry>>;

struct Stored {
    pic: Arc<DecodedPic>,
    frame_num: u32,
    /// Some(index) for a long-term reference, None for a short-term one.
    long_idx: Option<u32>,
}

/// An output-pending picture with the presentation time of the packet it came from.
pub struct Pending {
    pub pic: Arc<DecodedPic>,
    pub pts: i64,
}

#[derive(Default)]
pub struct Dpb {
    refs: Vec<Stored>,
    pub pending: Vec<Pending>,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num_offset: i64,
    prev_frame_num: u32,
    max_long_idx: Option<u32>,
    next_id: i32,
    /// Live use: when the SPS does not say how deep reordering goes, start at none and learn it.
    pub low_delay: bool,
    /// The depth learned so far in low-delay mode.
    learned: usize,
    /// The order count of the last picture output since the last IDR (or memory reset).
    last_out: Option<i32>,
    /// Pictures dropped from the output in low-delay mode because they came after a later one.
    pub late: u64,
    /// Buffers of finished pictures, for the next pictures to reuse.
    pub spare: Vec<Spare>,
    /// How many buffers to keep beyond 4.
    pub spare_extra: usize,
}

/// MaxDpbMbs for a level_idc (Table A-1).
fn max_dpb_mbs(level_idc: u8, constraint_set3: bool) -> usize {
    match level_idc {
        9 => 396,
        10 => 396,
        11 if constraint_set3 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    }
}

impl Dpb {
    pub fn new_id(&mut self) -> i32 {
        self.next_id += 1;
        self.next_id
    }

    /// How many pictures may wait for output before the earliest must go out.
    pub fn reorder_depth(sps: &Sps) -> usize {
        if sps.poc_type == 2 {
            return 0;
        }
        if let Some(n) = sps.num_reorder_frames {
            return n as usize;
        }
        let frames = max_dpb_mbs(sps.level_idc, false) / (sps.mb_width * sps.mb_height).max(1);
        frames.clamp(1, 16)
    }

    /// Order counts of the top and bottom fields of the frame the header starts (8.2.1); the
    /// frame's own is the lesser.
    pub fn compute_poc(&mut self, sps: &Sps, h: &SliceHeader) -> [i32; 2] {
        let max_frame_num = 1i64 << sps.log2_max_frame_num;
        let frame_num_offset = if h.idr {
            0
        } else if self.prev_frame_num > h.frame_num {
            self.prev_frame_num_offset + max_frame_num
        } else {
            self.prev_frame_num_offset
        };
        let poc = match sps.poc_type {
            0 => {
                let (prev_msb, prev_lsb) = if h.idr { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let lsb = h.poc_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                let bottom = top + h.delta_poc_bottom;
                if h.nal_ref_idc != 0 {
                    self.prev_poc_msb = msb;
                    self.prev_poc_lsb = lsb;
                }
                [top, bottom]
            }
            1 => {
                let n = sps.offset_for_ref_frame.len() as i64;
                let mut abs_frame_num = if n != 0 { frame_num_offset + h.frame_num as i64 } else { 0 };
                if h.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0i64;
                if abs_frame_num > 0 {
                    let cycle = (abs_frame_num - 1) / n;
                    let in_cycle = ((abs_frame_num - 1) % n) as usize;
                    let per_cycle: i64 = sps.offset_for_ref_frame.iter().map(|&v| v as i64).sum();
                    expected = cycle * per_cycle + sps.offset_for_ref_frame[..=in_cycle].iter().map(|&v| v as i64).sum::<i64>();
                }
                if h.nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic as i64;
                }
                let top = expected + h.delta_poc[0] as i64;
                let bottom = top + sps.offset_for_top_to_bottom_field as i64 + h.delta_poc[1] as i64;
                [top as i32, bottom as i32]
            }
            _ => {
                let p = if h.idr {
                    0
                } else if h.nal_ref_idc == 0 {
                    (2 * (frame_num_offset + h.frame_num as i64) - 1) as i32
                } else {
                    (2 * (frame_num_offset + h.frame_num as i64)) as i32
                };
                [p, p]
            }
        };
        self.prev_frame_num_offset = frame_num_offset;
        self.prev_frame_num = h.frame_num;
        poc
    }

    /// The counts after a picture with memory management operation 5 (8.2.1): the picture's own top
    /// count is now `top` (its lesser count rebased to 0), and the picture counts after it restart from
    /// there with frame_num 0.
    pub fn restart_after_mmco5(&mut self, top: i32) {
        self.prev_frame_num = 0;
        self.prev_frame_num_offset = 0;
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = top;
    }

    fn frame_num_wrap(&self, frame_num: u32, cur: u32, max: u32) -> i64 {
        if frame_num > cur {
            frame_num as i64 - max as i64
        } else {
            frame_num as i64
        }
    }

    /// Builds the initial reference lists for a slice and applies the header's modifications.
    pub fn ref_lists(&self, sps: &Sps, h: &SliceHeader, cur_poc: i32) -> Result<[RefList; 2]> {
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        let mut shorts: Vec<&Stored> = self.refs.iter().filter(|s| s.long_idx.is_none()).collect();
        let mut longs: Vec<&Stored> = self.refs.iter().filter(|s| s.long_idx.is_some()).collect();
        longs.sort_by_key(|s| s.long_idx);
        let entry = |s: &Stored| RefEntry { pic: s.pic.clone(), long: s.long_idx.is_some() };
        let mut lists: [Vec<RefEntry>; 2] = [Vec::new(), Vec::new()];
        match h.slice_type {
            SliceType::I => {}
            SliceType::P => {
                shorts.sort_by_key(|s| std::cmp::Reverse(self.frame_num_wrap(s.frame_num, h.frame_num, max_frame_num)));
                lists[0] = shorts.iter().map(|s| entry(s)).chain(longs.iter().map(|s| entry(s))).collect();
            }
            SliceType::B => {
                let mut before: Vec<&Stored> = shorts.iter().copied().filter(|s| s.pic.poc < cur_poc).collect();
                let mut after: Vec<&Stored> = shorts.iter().copied().filter(|s| s.pic.poc >= cur_poc).collect();
                before.sort_by_key(|s| std::cmp::Reverse(s.pic.poc));
                after.sort_by_key(|s| s.pic.poc);
                lists[0] = before.iter().chain(after.iter()).chain(longs.iter()).map(|s| entry(s)).collect();
                lists[1] = after.iter().chain(before.iter()).chain(longs.iter()).map(|s| entry(s)).collect();
                if lists[1].len() > 1 && lists[0].len() == lists[1].len() && lists[0].iter().zip(&lists[1]).all(|(a, b)| a.pic.id == b.pic.id) {
                    lists[1].swap(0, 1);
                }
            }
        }
        let mut out: [RefList; 2] = [Vec::new(), Vec::new()];
        for l in 0..2 {
            let n = h.num_ref_idx_active[l] as usize;
            if n == 0 {
                continue;
            }
            let mut list: RefList = lists[l].iter().map(|e| Some(e.clone())).collect();
            list.truncate(n);
            list.resize(n, None);
            // modification
            let mut pred = h.frame_num as i64;
            let max_pic_num = max_frame_num as i64;
            let mut idx = 0usize;
            // idx is also read after the insert and bumped mid-body, so it stays a separate counter.
            #[allow(clippy::explicit_counter_loop)]
            for &(idc, v) in &h.ref_mod.ops[l] {
                if idx >= n {
                    break;
                }
                let pic: Option<RefEntry> = if idc < 2 {
                    let d = v as i64 + 1;
                    let no_wrap = if idc == 0 {
                        if pred - d < 0 {
                            pred - d + max_pic_num
                        } else {
                            pred - d
                        }
                    } else if pred + d >= max_pic_num {
                        pred + d - max_pic_num
                    } else {
                        pred + d
                    };
                    pred = no_wrap;
                    let pic_num = if no_wrap > h.frame_num as i64 { no_wrap - max_pic_num } else { no_wrap };
                    self.refs
                        .iter()
                        .find(|s| s.long_idx.is_none() && self.frame_num_wrap(s.frame_num, h.frame_num, max_frame_num) == pic_num)
                        .map(|s| RefEntry { pic: s.pic.clone(), long: false })
                } else {
                    self.refs.iter().find(|s| s.long_idx == Some(v)).map(|s| RefEntry { pic: s.pic.clone(), long: true })
                };
                let Some(pic) = pic else {
                    return Err(Error::invalid_input("H.264 reference list modification names a picture that is not in the buffer"));
                };
                list.insert(idx, Some(pic.clone()));
                idx += 1;
                let mut k = idx;
                let mut w = idx;
                while k < list.len() {
                    let keep = match &list[k] {
                        Some(e) => !(e.pic.id == pic.pic.id && e.long == pic.long),
                        None => true,
                    };
                    if keep {
                        list.swap(w, k);
                        w += 1;
                    }
                    k += 1;
                }
                list.truncate(n.max(w).min(n + 1));
                list.truncate(n);
            }
            out[l] = list;
        }
        Ok(out)
    }

    /// Reference picture marking after a picture has been decoded (8.2.5), then storage.
    pub fn finish(&mut self, sps: &Sps, h: &SliceHeader, pic: Arc<DecodedPic>, pts: i64) -> Vec<Pending> {
        let mut out = Vec::new();
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        let mut has_mmco5 = false;
        if h.idr {
            // everything before an IDR picture goes out first
            out.extend(self.drain_all());
            self.last_out = None;
            self.clear_refs();
            self.max_long_idx = None;
        }
        if h.nal_ref_idc != 0 {
            let mut long_idx = None;
            if h.idr {
                if h.marking.long_term_reference {
                    long_idx = Some(0);
                    self.max_long_idx = Some(0);
                }
            } else if h.marking.adaptive {
                let (long, m5) = self.apply_mmco(&h.marking, h.frame_num, max_frame_num);
                long_idx = long;
                has_mmco5 = m5;
            } else {
                self.sliding_window(sps, h.frame_num, max_frame_num);
            }
            if has_mmco5 {
                out.extend(self.drain_all());
                self.last_out = None;
            }
            self.refs.push(Stored { pic: pic.clone(), frame_num: if has_mmco5 { 0 } else { h.frame_num }, long_idx });
        }
        if has_mmco5 {
            self.prev_frame_num = 0;
            self.prev_frame_num_offset = 0;
            self.prev_poc_msb = 0;
            // the picture's top field order count once 8.2.1 has rebased it: 0 for a frame
            self.prev_poc_lsb = 0;
        }
        let learning = self.low_delay && sps.num_reorder_frames.is_none() && sps.poc_type != 2;
        if learning && self.last_out.is_some_and(|l| pic.poc < l) {
            // a picture to be shown before one already shown: the stream reorders deeper than
            // assumed. It stays a reference but is not output, so the order shown stays right.
            self.learned = (self.learned + 1).min(16);
            self.late += 1;
            return out;
        }
        self.pending.push(Pending { pic, pts });
        let depth = if learning { self.learned } else { Self::reorder_depth(sps) };
        while self.pending.len() > depth {
            out.push(self.take_min());
        }
        out
    }

    fn take_min(&mut self) -> Pending {
        let k = self.pending.iter().enumerate().min_by_key(|(_, p)| p.pic.poc).map(|(i, _)| i).unwrap_or(0);
        let p = self.pending.remove(k);
        self.last_out = Some(p.pic.poc);
        p
    }

    pub fn drain_all(&mut self) -> Vec<Pending> {
        let mut v = Vec::new();
        while !self.pending.is_empty() {
            v.push(self.take_min());
        }
        v
    }

    fn sliding_window(&mut self, sps: &Sps, cur_frame_num: u32, max_frame_num: u32) {
        let max = sps.max_num_ref_frames.max(1) as usize;
        while self.refs.len() >= max {
            let victim = self
                .refs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.long_idx.is_none())
                .min_by_key(|(_, s)| self.frame_num_wrap(s.frame_num, cur_frame_num, max_frame_num))
                .map(|(i, _)| i);
            self.unref(victim.unwrap_or(0));
        }
    }

    fn unref(&mut self, i: usize) {
        let s = self.refs.remove(i);
        self.release(s.pic);
    }

    fn clear_refs(&mut self) {
        for s in std::mem::take(&mut self.refs) {
            self.release(s.pic);
        }
    }

    /// Keeps the buffers of a picture that nothing refers to any more. Only a complete picture is
    /// kept: an incomplete one may hold samples and macroblocks that a new picture must not inherit.
    pub fn release(&mut self, pic: Arc<DecodedPic>) {
        if let Ok(p) = Arc::try_unwrap(pic) {
            self.keep(p.data.into_spare());
        }
    }

    /// Keeps the buffers of a picture if all its macroblocks were decoded.
    pub fn keep(&mut self, (planes, mbs): Spare) {
        if self.spare.len() < 4 + self.spare_extra && mbs.iter().all(|m| m.slice != 0) {
            self.spare.push((planes, mbs));
        }
    }

    /// Memory management control operations; returns the long-term index given to the current
    /// picture, if any, and whether operation 5 was seen.
    fn apply_mmco(&mut self, m: &Marking, cur_frame_num: u32, max_frame_num: u32) -> (Option<u32>, bool) {
        let mut cur_long = None;
        let mut m5 = false;
        for &(op, a, b) in &m.ops {
            match op {
                1 => {
                    let pic_num = cur_frame_num as i64 - (a as i64 + 1);
                    if let Some(i) = self.refs.iter().position(|s| s.long_idx.is_none() && self.frame_num_wrap(s.frame_num, cur_frame_num, max_frame_num) == pic_num) {
                        self.unref(i);
                    }
                }
                2 => {
                    if let Some(i) = self.refs.iter().position(|s| s.long_idx == Some(a)) {
                        self.unref(i);
                    }
                }
                3 => {
                    let pic_num = cur_frame_num as i64 - (a as i64 + 1);
                    if let Some(i) = self.refs.iter().position(|s| s.long_idx == Some(b)) {
                        self.unref(i);
                    }
                    if let Some(s) = self.refs.iter_mut().find(|s| s.long_idx.is_none() && (if s.frame_num > cur_frame_num { s.frame_num as i64 - max_frame_num as i64 } else { s.frame_num as i64 }) == pic_num) {
                        s.long_idx = Some(b);
                    }
                }
                4 => {
                    self.max_long_idx = if a == 0 { None } else { Some(a - 1) };
                    let lim = a;
                    while let Some(i) = self.refs.iter().position(|s| s.long_idx.is_some_and(|i| i >= lim)) {
                        self.unref(i);
                    }
                }
                5 => {
                    self.clear_refs();
                    self.max_long_idx = None;
                    m5 = true;
                }
                6 => {
                    if let Some(i) = self.refs.iter().position(|s| s.long_idx == Some(b)) {
                        self.unref(i);
                    }
                    cur_long = Some(b);
                }
                _ => {}
            }
        }
        (cur_long, m5)
    }
}
