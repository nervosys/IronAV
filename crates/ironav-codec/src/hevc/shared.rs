//! Pictures that other threads read while they are still being decoded: one thread publishes the rows
//! of a picture in order as they become final, and readers (motion compensation and temporal motion
//! vector prediction in the pictures that refer to it) wait for the rows they need.

use super::frame::{Frame, MvField};
use super::ps::Sps;
use ironav_core::VideoFrame;
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// A buffer whose front part is read by several threads while one thread writes past it.
struct Cells<T>(Vec<UnsafeCell<T>>);

// SAFETY: elements are written only by a picture's one `Publisher`, and only past the published rows;
// readers see only the published rows (see `SharedFrame`), so no write overlaps a reference.
unsafe impl<T: Sync> Sync for Cells<T> {}

impl<T: Copy> Cells<T> {
    fn new(v: Vec<T>) -> Self {
        let mut v = ManuallyDrop::new(v);
        // SAFETY: UnsafeCell<T> has the same in-memory representation as T
        Cells(unsafe { Vec::from_raw_parts(v.as_mut_ptr().cast(), v.len(), v.capacity()) })
    }

    fn into_vec(self) -> Vec<T> {
        let mut v = ManuallyDrop::new(self.0);
        // SAFETY: as in `new`
        unsafe { Vec::from_raw_parts(v.as_mut_ptr().cast(), v.len(), v.capacity()) }
    }

    /// SAFETY: nothing may write the first `n` elements while the slice lives.
    unsafe fn prefix(&self, n: usize) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.0[..n].as_ptr().cast(), n) }
    }

    /// SAFETY: nothing may read or write the elements from `from` on while the slice lives.
    #[allow(clippy::mut_from_ref)]
    unsafe fn tail(&self, from: usize) -> &mut [T] {
        let t = &self.0[from..];
        unsafe { std::slice::from_raw_parts_mut(t.as_ptr().cast_mut().cast(), t.len()) }
    }

    /// SAFETY: no reference to the elements written may exist, and no other thread may write them.
    unsafe fn write(&self, at: usize, src: &[T]) {
        let dst = &self.0[at..at + src.len()];
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), UnsafeCell::raw_get(dst.as_ptr()), src.len()) }
    }
}

/// The buffers of a published picture, for a new one to reuse.
struct Spare {
    planes: [Vec<u16>; 3],
    mv: Vec<MvField>,
}

/// Buffers of finished pictures, for the next ones: a buffer that was in use before is faster than a
/// fresh allocation, which costs page faults.
#[derive(Default)]
pub(super) struct BufPool {
    shared: Mutex<Vec<Spare>>,
    work: Mutex<Vec<Frame>>,
}

const MAX_SPARE: usize = 40;

impl BufPool {
    /// A buffer of a picture being decoded, if one of the same size and format is kept.
    pub(super) fn take_work(&self, fits: impl Fn(&Frame) -> bool) -> Option<Frame> {
        let mut w = self.work.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(f) = w.pop() {
            if fits(&f) {
                return Some(f);
            }
        }
        None
    }

    pub(super) fn give_work(&self, f: Frame) {
        let mut w = self.work.lock().unwrap_or_else(PoisonError::into_inner);
        if w.len() < MAX_SPARE {
            w.push(f);
        }
    }

    fn take_shared(&self, lens: [usize; 3], mv: usize) -> Option<Spare> {
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(b) = s.pop() {
            if b.mv.len() == mv && b.planes.iter().zip(lens).all(|(p, n)| p.len() == n) {
                return Some(b);
            }
        }
        None
    }
}

/// A picture as other pictures and the output see it: its samples and motion field, and how many of its
/// luma rows are final.
pub struct SharedFrame {
    planes: [Cells<u16>; 3],
    mv: Cells<MvField>,
    pub width: usize,
    pub height: usize,
    pub cwidth: usize,
    pub cheight: usize,
    pub w4: usize,
    h4: usize,
    /// Vertical chroma subsampling shift.
    sy: usize,
    pub poc: i32,
    pub pts: i64,
    /// Conformance window: left, right, top, bottom (luma samples).
    pub crop: [usize; 4],
    pub bit_depth: u32,
    pub chroma_format: u32,
    /// Final luma rows. It only grows, and rows below it are never written again. Chroma rows are final
    /// in proportion; a value short of the height is a multiple of 4 and of the chroma row height.
    done: AtomicUsize,
    lock: Mutex<()>,
    cv: Condvar,
    /// The output frame, built by the thread that decoded the picture.
    output: Mutex<Option<VideoFrame>>,
    pool: Arc<BufPool>,
}

impl SharedFrame {
    /// A picture of the size and format of `sps` none of whose rows are final yet (`grey` false), or a
    /// finished grey one.
    pub(super) fn new(pool: &Arc<BufPool>, sps: &Sps, poc: i32, pts: i64, grey: bool) -> SharedFrame {
        let (w, h) = (sps.width, sps.height);
        let (cw, ch) = match sps.chroma_format_idc {
            0 => (0, 0),
            1 => (w / 2, h / 2),
            2 => (w / 2, h),
            _ => (w, h),
        };
        let (w4, h4) = (w.div_ceil(4), h.div_ceil(4));
        let mid = |bd: u32| 1u16 << (bd - 1);
        let lens = [w * h, cw * ch, cw * ch];
        let Spare { planes, mv } = match pool.take_shared(lens, w4 * h4) {
            Some(mut s) => {
                if grey {
                    s.planes[0].fill(mid(sps.bit_depth_luma));
                    s.planes[1].fill(mid(sps.bit_depth_chroma));
                    s.planes[2].fill(mid(sps.bit_depth_chroma));
                    s.mv.fill(MvField::default());
                }
                s
            }
            None => Spare { planes: [vec![mid(sps.bit_depth_luma); lens[0]], vec![mid(sps.bit_depth_chroma); lens[1]], vec![mid(sps.bit_depth_chroma); lens[2]]], mv: vec![MvField::default(); w4 * h4] },
        };
        let [a, b, c] = planes;
        SharedFrame {
            planes: [Cells::new(a), Cells::new(b), Cells::new(c)],
            mv: Cells::new(mv),
            width: w,
            height: h,
            cwidth: cw,
            cheight: ch,
            w4,
            h4,
            sy: sps.chroma_shift().1,
            poc,
            pts,
            crop: sps.crop,
            bit_depth: sps.bit_depth_luma,
            chroma_format: sps.chroma_format_idc,
            done: AtomicUsize::new(if grey { h } else { 0 }),
            lock: Mutex::new(()),
            cv: Condvar::new(),
            output: Mutex::new(None),
            pool: pool.clone(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.done.load(Ordering::Acquire) >= self.height
    }

    /// Waits until the first `rows` luma rows (all of them, if there are fewer) are final; returns how
    /// many rows are.
    pub fn wait(&self, rows: usize) -> usize {
        let rows = rows.min(self.height);
        let d = self.done.load(Ordering::Acquire);
        if d >= rows {
            return d;
        }
        let mut g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let d = self.done.load(Ordering::Acquire);
            if d >= rows {
                return d;
            }
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Rows of component `c` that `d` final luma rows make final.
    fn plane_rows(&self, c: usize, d: usize) -> usize {
        match (c, d >= self.height) {
            (0, _) => d.min(self.height),
            (_, true) => self.cheight,
            _ => d >> self.sy,
        }
    }

    fn mv_rows(&self, d: usize) -> usize {
        if d >= self.height {
            self.h4
        } else {
            d / 4
        }
    }

    /// The final rows of a plane, at least those that go with the first `rows` luma rows: waits for them.
    pub fn plane(&self, c: usize, rows: usize) -> &[u16] {
        let d = self.wait(rows);
        let stride = if c == 0 { self.width } else { self.cwidth };
        // SAFETY: the publisher wrote these rows before its release store of `done`, which `wait`
        // loaded with acquire, and from then on it writes only rows past them
        unsafe { self.planes[c].prefix(self.plane_rows(c, d) * stride) }
    }

    /// The whole plane, once the picture is final.
    pub fn full_plane(&self, c: usize) -> &[u16] {
        self.plane(c, self.height)
    }

    /// The motion of the 4x4 block `i` of the 4x4 grid, once its rows are final.
    pub fn mv(&self, i: usize) -> Option<MvField> {
        if i >= self.mv.0.len() {
            return None;
        }
        let d = self.wait(((i / self.w4) + 1) * 4);
        // SAFETY: as in `plane`
        unsafe { self.mv.prefix(self.mv_rows(d) * self.w4) }.get(i).copied()
    }

    /// The output frame built when the picture was decoded, if it was.
    pub fn take_output(&self) -> Option<VideoFrame> {
        self.output.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    pub fn set_output(&self, v: VideoFrame) {
        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = Some(v);
    }
}

impl Drop for SharedFrame {
    fn drop(&mut self) {
        let take = |c: &mut Cells<u16>| std::mem::replace(c, Cells(Vec::new())).into_vec();
        let [a, b, c] = &mut self.planes;
        let spare = Spare { planes: [take(a), take(b), take(c)], mv: std::mem::replace(&mut self.mv, Cells(Vec::new())).into_vec() };
        let mut s = self.pool.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if s.len() < MAX_SPARE {
            s.push(spare);
        }
    }
}

/// The one writer of a picture that other threads read while it is decoded. Dropping it marks the whole
/// picture final, so that if decoding stops early (a panic) no reader waits forever.
pub struct Publisher {
    pic: Arc<SharedFrame>,
    /// Final luma rows; what `done` holds, as this is its only writer.
    n: usize,
}

impl Publisher {
    pub(super) fn new(pic: SharedFrame) -> (Arc<SharedFrame>, Publisher) {
        let pic = Arc::new(pic);
        (pic.clone(), Publisher { pic, n: 0 })
    }

    pub fn pic(&self) -> &SharedFrame {
        &self.pic
    }

    /// Luma rows published so far.
    pub fn rows(&self) -> usize {
        self.n
    }

    /// The three planes from the first row that is not final on; the loop filters work on them in place
    /// until their rows are published.
    pub fn tails(&mut self) -> [&mut [u16]; 3] {
        let p = &*self.pic;
        let stride = [p.width, p.cwidth, p.cwidth];
        // SAFETY: readers see only rows below `done`, which equals `n`, and this publisher, borrowed
        // here for as long as the slices live, is the only writer
        std::array::from_fn(|c| unsafe { p.planes[c].tail(p.plane_rows(c, self.n) * stride[c]) })
    }

    /// Makes the first `rows` luma rows final, with the motion of those rows from `mv`, the picture's
    /// whole motion field. The samples must have been written into `tails` already.
    pub fn advance(&mut self, rows: usize, mv: &[MvField]) {
        let p = &*self.pic;
        let rows = rows.min(p.height);
        if rows <= self.n {
            return;
        }
        assert_eq!(p.mv.0.len(), mv.len(), "a picture is published into buffers of its size");
        let (from, to) = (p.mv_rows(self.n) * p.w4, p.mv_rows(rows) * p.w4);
        // SAFETY: only this publisher writes, and only rows at or past `done`, to which no reader has a
        // reference
        unsafe { p.mv.write(from, &mv[from..to]) }
        self.n = rows;
        self.mark(rows);
    }

    /// Stores the output frame, for the thread that emits it to find.
    pub fn set_output(&self, v: VideoFrame) {
        self.pic.set_output(v);
    }

    fn mark(&self, rows: usize) {
        let p = &*self.pic;
        p.done.store(rows, Ordering::Release);
        // taking the lock orders this with a reader between its check and its wait
        drop(p.lock.lock().unwrap_or_else(PoisonError::into_inner));
        p.cv.notify_all();
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if self.n < self.pic.height {
            // nothing writes the picture any more, so all of it can be read as it is
            self.mark(self.pic.height);
        }
    }
}
