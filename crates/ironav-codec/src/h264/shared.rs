//! Pictures that other threads read while they are still being decoded: one thread publishes their
//! macroblock rows in order as the rows become final, and readers wait for the rows they need.

use super::dpb::DecodedPic;
use super::mb::{MbInfo, Picture, Spare};
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// A buffer whose front part is read by several threads while one thread writes past it.
struct Cells<T>(Vec<UnsafeCell<T>>);

// SAFETY: elements are written only by a picture's one `Publisher`, and only past the published
// rows; readers see only the published rows (see `Published`), so no write overlaps a reference.
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

    /// SAFETY: no reference to the elements written may exist, and no other thread may write them.
    unsafe fn write(&self, at: usize, src: &[T]) {
        let dst = &self.0[at..at + src.len()];
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), UnsafeCell::raw_get(dst.as_ptr()), src.len()) }
    }
}

/// The samples and macroblock info of a picture, and how many of its macroblock rows are final.
pub struct Published {
    planes: [Cells<u16>; 3],
    mbs: Cells<MbInfo>,
    mb_w: usize,
    mb_h: usize,
    /// Final macroblock rows. It only grows, and rows below it are never written again.
    done: AtomicUsize,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Published {
    /// A whole picture.
    pub fn new((planes, mbs): Spare, mb_w: usize, mb_h: usize) -> Self {
        let [a, b, c] = planes;
        let mb_h = mb_h.max(1);
        Published { planes: [Cells::new(a), Cells::new(b), Cells::new(c)], mbs: Cells::new(mbs), mb_w: mb_w.max(1), mb_h, done: AtomicUsize::new(mb_h), lock: Mutex::new(()), cv: Condvar::new() }
    }

    pub fn is_complete(&self) -> bool {
        self.done.load(Ordering::Acquire) == self.mb_h
    }

    /// Waits until the first `rows` macroblock rows (all of them, if there are fewer) are final;
    /// returns how many rows are.
    fn wait(&self, rows: usize) -> usize {
        let rows = rows.min(self.mb_h);
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

    /// The final rows of a plane, at least its first `rows` macroblock rows: waits for them.
    pub fn plane(&self, pl: usize, rows: usize) -> &[u16] {
        let d = self.wait(rows);
        let c = &self.planes[pl];
        // SAFETY: the publisher wrote these rows before its release store of `done`, which `wait`
        // loaded with acquire, and from then on it writes only rows past them
        unsafe { c.prefix(c.0.len() / self.mb_h * d) }
    }

    /// The info of macroblock `i`, once its row is final.
    pub fn mb(&self, i: usize) -> Option<&MbInfo> {
        if i >= self.mbs.0.len() {
            return None;
        }
        let d = self.wait(i / self.mb_w + 1);
        // SAFETY: as in `plane`
        unsafe { self.mbs.prefix(self.mb_w * d) }.get(i)
    }

    /// The buffers, for a new picture to reuse.
    pub fn into_spare(self) -> Spare {
        let [a, b, c] = self.planes;
        ([a.into_vec(), b.into_vec(), c.into_vec()], self.mbs.into_vec())
    }
}

/// The one writer of a picture that other threads read while it is decoded. Dropping it marks the
/// whole picture final, so that if decoding stops early (a panic) no reader waits forever.
pub struct Publisher {
    pic: Arc<DecodedPic>,
    n: usize,
}

impl Publisher {
    /// Shares a picture none of whose rows are final yet.
    pub fn new(mut pic: DecodedPic) -> (Arc<DecodedPic>, Publisher) {
        *pic.data.done.get_mut() = 0;
        let pic = Arc::new(pic);
        (pic.clone(), Publisher { pic, n: 0 })
    }

    pub fn pic(&self) -> &DecodedPic {
        &self.pic
    }

    /// Publishes the first `rows` macroblock rows of `src`, which must be final.
    pub fn publish(&mut self, src: &Picture, rows: usize) {
        let p = &self.pic.data;
        let rows = rows.min(p.mb_h);
        if rows <= self.n {
            return;
        }
        for (c, s) in p.planes.iter().zip(&src.planes) {
            assert_eq!(c.0.len(), s.len(), "a picture is published into buffers of its size");
            let r = s.len() / p.mb_h;
            // SAFETY: only this publisher writes, and only rows at or past `done`, to which no
            // reader has a reference
            unsafe { c.write(self.n * r, &s[self.n * r..rows * r]) }
        }
        assert_eq!(p.mbs.0.len(), src.mbs.len(), "a picture is published into buffers of its size");
        // SAFETY: as above
        unsafe { p.mbs.write(self.n * p.mb_w, &src.mbs[self.n * p.mb_w..rows * p.mb_w]) }
        self.n = rows;
        self.mark(rows);
    }

    fn mark(&self, rows: usize) {
        let p = &self.pic.data;
        p.done.store(rows, Ordering::Release);
        // taking the lock orders this with a reader between its check and its wait
        drop(p.lock.lock().unwrap_or_else(PoisonError::into_inner));
        p.cv.notify_all();
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if self.n < self.pic.data.mb_h {
            // nothing writes the picture any more, so all of it can be read as it is
            self.mark(self.pic.data.mb_h);
        }
    }
}
