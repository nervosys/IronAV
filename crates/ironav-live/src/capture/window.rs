//! Per-window capture, the parts that need no operating system: the types a caller sees, how a
//! frame's size is planned against the frame pool's, where a window's client area lies in its picture,
//! the packing of mapped rows, and the signal that carries "a frame arrived" between threads.
//!
//! The Windows backend (`wgc.rs`, Windows.Graphics.Capture) uses these; they are plain code so that
//! the logic is tested on every platform.

use super::screen::{copy_rows, ScreenFrame};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// What a [`WindowCapture`](super::WindowCapture) shows of a window.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowCaptureOptions {
    /// Whether the mouse cursor is drawn into the pictures (when it is over the window). Turning it
    /// off needs Windows 10 version 2004 or later; on an older build opening fails rather than
    /// showing a cursor that was asked to be left out. Default true.
    pub cursor: bool,
    /// Whether the yellow border Windows draws around a captured window is kept. When false (the
    /// default) the capture asks the system to leave it out; Windows 11 allows that, and on a build
    /// or in a setting that does not the request is ignored and the border stays (see
    /// [`WindowCapture::border_hidden`](super::WindowCapture::border_hidden)).
    pub border: bool,
    /// Whether only the window's client area is captured (without the title bar and the frame)
    /// instead of the whole window. Default false.
    ///
    /// The area is cut out of the window's picture from where Windows says the window's visible
    /// frame and its client area lie. That needs a process that is DPI aware (otherwise those are in
    /// scaled coordinates that do not match the picture); when the two cannot be matched up, the whole
    /// window is given instead, and [`WindowFrame::client_only`] says which it was.
    pub client_area: bool,
}

impl Default for WindowCaptureOptions {
    fn default() -> WindowCaptureOptions {
        WindowCaptureOptions { cursor: true, border: false, client_area: false }
    }
}

/// What a captured window is doing, as [`WindowCapture::state`](super::WindowCapture::state) says.
///
/// Windows sends no pictures of a minimized or hidden window, so `next_frame` returns `Ok(None)`
/// for as long as it is; this is how to tell that apart from a window that is only still.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    /// The window is shown (it may be covered by others, or partly off the display).
    Running,
    /// The window is minimized to the taskbar.
    Minimized,
    /// The window exists but is not shown.
    Hidden,
    /// The window is gone. Reading from the capture fails.
    Closed,
}

/// One picture of a window, as [`WindowCapture::next_frame`](super::WindowCapture::next_frame)
/// gives it. It borrows the capture, so it is valid until the next call. It is always a picture
/// that was not given before.
///
/// The pixels are 32-bit BGRA, tightly packed (`stride == width * 4`). The alpha byte is whatever
/// the window composes (255 for most windows) and should not be relied on. This type has no `Debug`
/// output, so that pixels are not printed by accident.
#[non_exhaustive]
pub struct WindowFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bgra: &'a [u8],
    /// Counts the frames given by this capture, from 1.
    pub sequence: u64,
    /// When the picture was copied out.
    pub at: Instant,
    /// When the system composed the picture, on the system's performance counter (a clock that
    /// starts at boot); for the spacing of frames, not for the time of day.
    pub composed: Duration,
    /// The size of the whole window's picture as Windows reports it for this frame, whatever was cut out of it.
    /// It is larger than `(width, height)` when the picture is [`truncated`](Self::truncated).
    pub content: (u32, u32),
    /// The size differs from the previous frame's (always true for the first frame).
    pub resized: bool,
    /// The window grew, and this picture was made before the capture could follow it: it holds only the
    /// part of the window that fitted the old size. A whole picture follows once the window draws again.
    pub truncated: bool,
    /// Pictures Windows made since the previous frame that were skipped because a newer one was ready.
    pub dropped: u32,
    /// Only the client area was cut out (see [`WindowCaptureOptions::client_area`]).
    pub client_only: bool,
    /// How long the GPU copy, the read-back and the packing of this picture took.
    pub copy: Duration,
}

impl WindowFrame<'_> {
    /// The picture as a [`ScreenFrame`], whole, for the code that takes those (an encoder, say).
    pub fn to_screen_frame(&self) -> ScreenFrame<'_> {
        ScreenFrame::new(self.width, self.height, self.stride, self.bgra, self.at)
    }
}

/// A rectangle of a picture, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Crop {
    pub fn whole((w, h): (u32, u32)) -> Crop {
        Crop { x: 0, y: 0, w, h }
    }
}

/// What to do with a frame the pool handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FramePlan {
    /// The part of the frame's texture to read; `None` when nothing of the window shows in it.
    pub region: Option<Crop>,
    /// The window's picture does not fit the texture: `region` is only what fits.
    pub truncated: bool,
    /// The pool must be recreated at this size (after this frame): the window's size is not the pool's.
    pub recreate: Option<(u32, u32)>,
}

/// Plans a frame whose content is `content` big, in a texture of `texture`, from a pool that was last
/// set to `pool`. `wanted` is the part of the content to give (the client area), or all of it.
///
/// The frame pool holds textures of the size it was made with, and the window's picture lies in their
/// top-left corner. When the window is smaller than the texture the picture is cut out of it; when it
/// is larger, only what fits can be read, and the pool is to be recreated for the frames after.
pub(crate) fn plan_frame(content: (u32, u32), texture: (u32, u32), pool: (u32, u32), wanted: Option<Crop>) -> FramePlan {
    let recreate = (content != pool && content.0 > 0 && content.1 > 0).then_some(content);
    if content.0 == 0 || content.1 == 0 {
        return FramePlan { region: None, truncated: false, recreate };
    }
    let want = wanted.unwrap_or(Crop::whole(content));
    let x1 = want.x.saturating_add(want.w).min(texture.0);
    let y1 = want.y.saturating_add(want.h).min(texture.1);
    if want.x >= x1 || want.y >= y1 {
        return FramePlan { region: None, truncated: want.w > 0 && want.h > 0, recreate };
    }
    let got = Crop { x: want.x, y: want.y, w: x1 - want.x, h: y1 - want.y };
    FramePlan { region: Some(got), truncated: got != want, recreate }
}

/// Where the client area lies in the picture of a window, from the numbers Windows gives: `frame` is
/// the window's visible frame on the screen (its origin and size), `client` the client area's origin
/// on the screen and its size. `None` when they cannot be matched to a `content`-sized picture: the
/// frame's size is not the picture's, or the client area is empty or lies outside it.
pub(crate) fn client_crop(content: (u32, u32), frame: ((i32, i32), (i32, i32)), client: ((i32, i32), (i32, i32))) -> Option<Crop> {
    let ((fx, fy), (fw, fh)) = frame;
    let ((cx, cy), (cw, ch)) = client;
    if fw != content.0 as i32 || fh != content.1 as i32 || cw <= 0 || ch <= 0 {
        return None;
    }
    let (x, y) = (i64::from(cx) - i64::from(fx), i64::from(cy) - i64::from(fy));
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + i64::from(cw)).min(i64::from(content.0));
    let y1 = (y + i64::from(ch)).min(i64::from(content.1));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Crop { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
}

/// How many bytes of a mapped texture hold `rows` rows of `row` bytes, `pitch` apart (the last row
/// has no padding after it); `None` when the pitch is less than a row, or the size overflows.
pub(crate) fn mapped_len(pitch: usize, row: usize, rows: usize) -> Option<usize> {
    if rows == 0 || row == 0 {
        return Some(0);
    }
    if rows > 1 && pitch < row {
        return None;
    }
    pitch.checked_mul(rows - 1)?.checked_add(row)
}

/// Copies the picture of `width` by `rows` 32-bit pixels out of mapped memory `src` whose rows are
/// `pitch` bytes apart into `dst`, tightly packed. `dst` keeps its allocation between calls. False,
/// with `dst` empty, when `src` is too short or the pitch is less than a row.
pub(crate) fn pack_rows(src: &[u8], pitch: usize, width: usize, rows: usize, dst: &mut Vec<u8>) -> bool {
    let Some(row) = width.checked_mul(4) else {
        dst.clear();
        return false;
    };
    copy_rows(src, pitch, row, rows, dst)
}

/// What the frame pool's callbacks (which run on Windows' threads) tell the reader.
#[derive(Default)]
struct Wake {
    arrived: bool,
    closed: bool,
}

/// A signal from the threads Windows calls us on to the one that reads: a frame arrived, or the
/// window closed. It holds nothing else, so a callback that outlives the capture touches only this.
#[derive(Default)]
pub(crate) struct Signal {
    state: Mutex<Wake>,
    cv: Condvar,
}

impl Signal {
    fn lock(&self) -> MutexGuard<'_, Wake> {
        // the flags stay meaningful if a thread panicked while holding them
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn frame_arrived(&self) {
        self.lock().arrived = true;
        self.cv.notify_all();
    }

    pub fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Forgets earlier arrivals. The reader calls this before it looks for a frame, so that a frame
    /// that arrives after the look is not lost: it sets the flag again, and `wait` returns at once.
    pub fn clear_arrived(&self) {
        self.lock().arrived = false;
    }

    /// Waits up to `dur` for an arrival or the close; true when one has happened.
    pub fn wait(&self, dur: Duration) -> bool {
        let guard = self.lock();
        let (w, _) = self.cv.wait_timeout_while(guard, dur, |w| !w.arrived && !w.closed).unwrap_or_else(PoisonError::into_inner);
        w.arrived || w.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mapped_len_counts_the_last_row_without_padding() {
        assert_eq!(mapped_len(64, 40, 3), Some(64 * 2 + 40));
        assert_eq!(mapped_len(40, 40, 1), Some(40));
        assert_eq!(mapped_len(8, 40, 1), Some(40));
        assert_eq!(mapped_len(8, 40, 2), None);
        assert_eq!(mapped_len(0, 0, 5), Some(0));
        assert_eq!(mapped_len(usize::MAX, 4, 3), None);
    }

    #[test]
    fn pack_rows_drops_the_row_padding() {
        // 3 pixels wide, 2 rows, pitch 16 (4 bytes of padding per row)
        let mut src = vec![0u8; 16 * 2];
        for (i, b) in src.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut dst = Vec::new();
        assert!(pack_rows(&src, 16, 3, 2, &mut dst));
        assert_eq!(dst.len(), 24);
        assert_eq!(&dst[..12], &src[..12]);
        assert_eq!(&dst[12..], &src[16..28]);
    }

    #[test]
    fn pack_rows_needs_no_padding_after_the_last_row() {
        let src: Vec<u8> = (0..28).collect(); // 16 + 12
        let mut dst = Vec::new();
        assert!(pack_rows(&src, 16, 3, 2, &mut dst));
        assert_eq!(dst.len(), 24);
        assert!(!pack_rows(&src[..27], 16, 3, 2, &mut dst));
        assert!(dst.is_empty());
    }

    #[test]
    fn pack_rows_refuses_a_pitch_shorter_than_a_row_and_reuses_the_buffer() {
        let src = vec![1u8; 64];
        let mut dst = Vec::new();
        assert!(!pack_rows(&src, 8, 3, 2, &mut dst));
        assert!(pack_rows(&src, 16, 4, 4, &mut dst));
        let (cap, ptr) = (dst.capacity(), dst.as_ptr());
        assert!(pack_rows(&src, 16, 4, 4, &mut dst));
        assert_eq!((dst.capacity(), dst.as_ptr()), (cap, ptr));
        // a smaller picture fits in the same allocation
        assert!(pack_rows(&src, 16, 2, 2, &mut dst));
        assert_eq!(dst.as_ptr(), ptr);
    }

    #[test]
    fn a_frame_of_the_pool_size_is_kept_whole() {
        let p = plan_frame((800, 600), (800, 600), (800, 600), None);
        assert_eq!(p, FramePlan { region: Some(Crop::whole((800, 600))), truncated: false, recreate: None });
    }

    #[test]
    fn a_smaller_window_is_cut_out_of_the_texture_and_the_pool_follows() {
        let p = plan_frame((640, 480), (800, 600), (800, 600), None);
        assert_eq!(p.region, Some(Crop::whole((640, 480))));
        assert!(!p.truncated);
        assert_eq!(p.recreate, Some((640, 480)));
        // once recreated, frames of the new pool size need nothing more
        let q = plan_frame((640, 480), (640, 480), (640, 480), None);
        assert_eq!(q.recreate, None);
    }

    #[test]
    fn a_larger_window_gives_what_fits_and_says_so() {
        let p = plan_frame((1000, 700), (800, 600), (800, 600), None);
        assert_eq!(p.region, Some(Crop::whole((800, 600))));
        assert!(p.truncated);
        assert_eq!(p.recreate, Some((1000, 700)));
        // frames still queued from before the recreate are truncated the same way, without asking again
        let q = plan_frame((1000, 700), (800, 600), (1000, 700), None);
        assert!(q.truncated);
        assert_eq!(q.recreate, None);
    }

    #[test]
    fn growth_in_one_direction_only() {
        let p = plan_frame((800, 900), (800, 600), (800, 600), None);
        assert_eq!(p.region, Some(Crop { x: 0, y: 0, w: 800, h: 600 }));
        assert!(p.truncated);
    }

    #[test]
    fn an_empty_window_gives_nothing() {
        let p = plan_frame((0, 480), (800, 600), (800, 600), None);
        assert_eq!(p, FramePlan { region: None, truncated: false, recreate: None });
    }

    #[test]
    fn a_wanted_part_is_clamped_to_the_texture() {
        let want = Some(Crop { x: 10, y: 40, w: 700, h: 500 });
        let p = plan_frame((800, 600), (800, 600), (800, 600), want);
        assert_eq!(p.region, want);
        assert!(!p.truncated);
        // the window grew past the texture: part of the wanted area is not there yet
        let p = plan_frame((1000, 600), (800, 600), (800, 600), Some(Crop { x: 10, y: 40, w: 980, h: 500 }));
        assert_eq!(p.region, Some(Crop { x: 10, y: 40, w: 790, h: 500 }));
        assert!(p.truncated);
        // wholly outside the texture
        let p = plan_frame((1000, 600), (800, 600), (800, 600), Some(Crop { x: 900, y: 0, w: 100, h: 100 }));
        assert_eq!(p.region, None);
        assert!(p.truncated);
    }

    #[test]
    fn client_area_is_found_inside_the_frame() {
        // a 1000x700 frame at (100, 50) with a 1pt-wide border left and right, a 31pt title bar
        let c = client_crop((1000, 700), ((100, 50), (1000, 700)), ((108, 81), (984, 611)));
        assert_eq!(c, Some(Crop { x: 8, y: 31, w: 984, h: 611 }));
    }

    #[test]
    fn client_area_is_refused_when_the_frame_does_not_match_the_picture() {
        // the frame is reported at another size (a DPI-scaled process, say)
        assert_eq!(client_crop((1000, 700), ((0, 0), (800, 560)), ((8, 31), (784, 521))), None);
        assert_eq!(client_crop((1000, 700), ((0, 0), (1000, 700)), ((8, 31), (0, 521))), None);
    }

    #[test]
    fn client_area_is_clipped_to_the_picture() {
        let c = client_crop((100, 100), ((0, 0), (100, 100)), ((-5, 90), (200, 50)));
        assert_eq!(c, Some(Crop { x: 0, y: 90, w: 100, h: 10 }));
        assert_eq!(client_crop((100, 100), ((0, 0), (100, 100)), ((200, 0), (10, 10))), None);
    }

    #[test]
    fn an_arrival_before_the_wait_is_not_lost() {
        let s = Signal::default();
        s.clear_arrived();
        s.frame_arrived();
        assert!(s.wait(Duration::from_secs(5)));
    }

    #[test]
    fn an_arrival_before_the_clear_is_forgotten() {
        let s = Signal::default();
        s.frame_arrived();
        s.clear_arrived();
        let t = Instant::now();
        assert!(!s.wait(Duration::from_millis(30)));
        assert!(t.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn a_close_wakes_the_reader_and_stays() {
        let s = Arc::new(Signal::default());
        let s2 = s.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.close();
        });
        assert!(s.wait(Duration::from_secs(5)));
        h.join().unwrap();
        assert!(s.is_closed());
        // a closed signal returns at once however often it is waited on
        assert!(s.wait(Duration::from_secs(5)));
    }

    #[test]
    fn a_frame_from_another_thread_wakes_the_reader() {
        let s = Arc::new(Signal::default());
        s.clear_arrived();
        let s2 = s.clone();
        let h = std::thread::spawn(move || s2.frame_arrived());
        assert!(s.wait(Duration::from_secs(5)));
        h.join().unwrap();
    }
}
