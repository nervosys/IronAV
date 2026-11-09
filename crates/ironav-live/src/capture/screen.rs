//! Screens as `device://screen[/N]` sources: the display's picture, converted to Y4M 4:2:0 like a
//! camera's. N is the display's index from [`super::devices`]; without it, the primary display.
//!
//! The platform backends (DXGI Desktop Duplication, X11, CoreGraphics) only fetch the latest
//! picture as 32-bit BGRA; this module scales it down to the size asked for, converts it, and
//! sends it at a constant rate: when the screen has not changed, the last picture is sent again,
//! so a still desktop does not look like a stalled stream.
//!
//! Options: `fps` (30 by default, 60 at most), `size=WxH` (scale down to fit that box, keeping the
//! aspect; the default is the display's own size, but no wider than 1920 so that software encoding
//! stays feasible) and `cursor=0` to leave the mouse cursor out where the API draws it.
//!
//! Nothing here keeps or logs a picture; this reads everything on the display, so it is only
//! opened when a source asks for it.

use super::send;
use crate::source::ThreadSource;
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Rational, Result};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

type Tx = SyncSender<std::io::Result<Vec<u8>>>;

/// The widest picture a screen is sent in when `size` does not say.
const DEFAULT_MAX_WIDTH: u32 = 1920;
const DEFAULT_FPS: f64 = 30.0;
const MAX_FPS: f64 = 60.0;

/// A display as listed: its name and native size in pixels.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Listed {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// What a source asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Wants {
    pub index: usize,
    pub fps: f64,
    pub size: Option<(u32, u32)>,
    pub cursor: bool,
}

/// One picture from a display: 32-bit pixels, blue first, rows `stride` bytes apart.
pub(crate) struct Shot<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    /// Where the picture's top-left pixel lies on the display (0, 0 for the whole display).
    pub origin: (u32, u32),
    /// The display's size.
    pub display: (u32, u32),
}

impl<'a> Shot<'a> {
    /// A picture of the whole display.
    pub fn new(data: &'a [u8], stride: usize, width: u32, height: u32) -> Shot<'a> {
        Shot { data, stride, width, height, origin: (0, 0), display: (width, height) }
    }

    /// The same picture, taken to be the part of a `display`-sized display whose top-left pixel is at `origin`.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn at(self, origin: (u32, u32), display: (u32, u32)) -> Shot<'a> {
        Shot { origin, display, ..self }
    }
}

/// What a backend gives: the display's latest picture.
pub(crate) trait Grabber {
    /// The display's size in pixels.
    fn size(&self) -> (u32, u32);
    /// The latest picture, or `None` when the display has not changed since the last call (or has
    /// shown nothing yet).
    fn grab(&mut self) -> Result<Option<Shot<'_>>>;
    /// Asks for only `region` (display coordinates, not clipped) from the next picture on, or the whole
    /// display for `None`; true when the backend does that itself (its pictures then carry their
    /// `origin` and `display`, and its changes are the region's). The default says no, and the caller
    /// crops the whole display's pictures.
    fn set_region(&mut self, region: Option<Rect>) -> bool {
        let _ = region;
        false
    }
    /// Waits up to `wait` for the display to change, then gives its latest picture, and writes what
    /// changed since the last call into `changes`. A backend that cannot tell what changed (the
    /// default) takes a picture after `wait` and reports the frame as whole.
    fn grab_within(&mut self, wait: Duration, changes: &mut Changes) -> Result<Option<Shot<'_>>> {
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        changes.whole = true;
        changes.dirty.clear();
        changes.moved.clear();
        self.grab()
    }
}

/// `screen` or `screen/N`: the display's index.
pub(crate) fn parse_target(target: &str) -> Result<usize> {
    match target.split_once('/') {
        None => Ok(0),
        Some((_, "")) => Ok(0),
        Some((_, n)) => n.parse::<usize>().map_err(|_| Error::invalid_arg(format!("a screen is device://screen[/N] with N the display's index from `ironav devices`, got device://{target}")).alternatives(["device://screen", "device://screen/1"])),
    }
}

/// The options of a screen source.
pub(crate) fn parse_wants(u: &SourceUrl, index: usize) -> Result<Wants> {
    let fps = match u.option("fps") {
        None => DEFAULT_FPS,
        Some(s) => {
            let f = super::parse_fps(s)?;
            if !(1.0..=MAX_FPS).contains(&f) {
                return Err(Error::invalid_arg(format!("a screen's fps must be from 1 to {MAX_FPS}, got '{s}'")));
            }
            f
        }
    };
    let cursor = match u.option("cursor") {
        None | Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        Some(other) => return Err(Error::invalid_arg(format!("cursor must be 0 or 1, got '{other}'"))),
    };
    Ok(Wants { index, fps, size: u.option("size").map(super::parse_size).transpose()?, cursor })
}

/// The size of the pictures sent, for a display of `native` pixels: scaled down (never up) to fit
/// `want`, or no wider than [`DEFAULT_MAX_WIDTH`] when none is given, with the aspect kept and both
/// sides even, as 4:2:0 needs.
pub(crate) fn fit_size(native: (u32, u32), want: Option<(u32, u32)>) -> (u32, u32) {
    let (nw, nh) = (native.0.max(2) as f64, native.1.max(2) as f64);
    let scale = match want {
        Some((w, h)) => (w as f64 / nw).min(h as f64 / nh),
        None => DEFAULT_MAX_WIDTH as f64 / nw,
    }
    .min(1.0);
    let side = |n: f64, limit: Option<u32>| {
        let s = (n * scale).round() as u32;
        (limit.map_or(s, |l| s.min(l)) & !1).max(2)
    };
    (side(nw, want.map(|w| w.0)), side(nh, want.map(|w| w.1)))
}

/// A frame rate as the fraction Y4M carries.
fn rational(fps: f64) -> Rational {
    let (mut n, mut d) = ((fps * 1000.0).round() as i64, 1000i64);
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    n /= a;
    d /= a;
    Rational::new(n, d)
}

/// The BT.709 limited-range luma and chroma of a pixel (the camera paths get the same from the
/// devices, which convert with BT.709 for HD pictures).
fn yuv(r: i32, g: i32, b: i32) -> (u8, u8, u8) {
    let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
    let u = ((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (y.clamp(16, 235) as u8, u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

/// One Y4M frame of `dw` by `dh` (both even) from a picture, scaled by taking the nearest pixels.
/// `None` when the picture is smaller than its size and stride say.
pub(crate) fn bgra_to_y4m(shot: &Shot, dw: usize, dh: usize) -> Option<Vec<u8>> {
    let (sw, sh) = (shot.width as usize, shot.height as usize);
    if dw < 2 || dh < 2 || !dw.is_multiple_of(2) || !dh.is_multiple_of(2) || sw == 0 || sh == 0 || shot.stride < sw * 4 || shot.data.len() < shot.stride * (sh - 1) + sw * 4 {
        return None;
    }
    // the source pixel at the middle of each output pixel
    let xs: Vec<usize> = (0..dw).map(|x| ((2 * x + 1) * sw / (2 * dw)).min(sw - 1) * 4).collect();
    let ys: Vec<usize> = (0..dh).map(|y| ((2 * y + 1) * sh / (2 * dh)).min(sh - 1) * shot.stride).collect();
    let px = |x: usize, y: usize| {
        let i = ys[y] + xs[x];
        (shot.data[i + 2] as i32, shot.data[i + 1] as i32, shot.data[i] as i32)
    };
    let mut out = Vec::with_capacity(6 + dw * dh * 3 / 2);
    out.extend_from_slice(b"FRAME\n");
    for y in 0..dh {
        for x in 0..dw {
            let (r, g, b) = px(x, y);
            out.push(yuv(r, g, b).0);
        }
    }
    let (mut us, mut vs) = (Vec::with_capacity(dw * dh / 4), Vec::with_capacity(dw * dh / 4));
    for cy in 0..dh / 2 {
        for cx in 0..dw / 2 {
            let (mut r, mut g, mut b) = (2, 2, 2);
            for (x, y) in [(2 * cx, 2 * cy), (2 * cx + 1, 2 * cy), (2 * cx, 2 * cy + 1), (2 * cx + 1, 2 * cy + 1)] {
                let p = px(x, y);
                (r, g, b) = (r + p.0, g + p.1, b + p.2);
            }
            let (_, u, v) = yuv(r / 4, g / 4, b / 4);
            us.push(u);
            vs.push(v);
        }
    }
    out.extend(us);
    out.extend(vs);
    Some(out)
}

/// A black frame, for a display that has shown nothing yet.
fn black(dw: usize, dh: usize) -> Vec<u8> {
    let mut f = b"FRAME\n".to_vec();
    f.resize(6 + dw * dh, 16);
    f.resize(6 + dw * dh * 3 / 2, 128);
    f
}

/// Sends a frame every `1 / fps` seconds until the source is closed: the display's latest picture,
/// or the last one again when it has not changed.
fn pump(g: &mut dyn Grabber, wants: &Wants, dw: usize, dh: usize, tx: &Tx) -> Result<()> {
    let period = Duration::from_secs_f64(1.0 / wants.fps);
    let mut next = Instant::now();
    let mut last: Option<Vec<u8>> = None;
    loop {
        if let Some(shot) = g.grab()? {
            let frame = bgra_to_y4m(&shot, dw, dh).ok_or_else(|| Error::new(ErrorCode::InvalidInput, format!("the screen gave a picture of {} bytes, too short for {}x{}", shot.data.len(), shot.width, shot.height)))?;
            last = Some(frame);
        }
        let frame = last.clone().unwrap_or_else(|| black(dw, dh));
        if !send(tx, frame) {
            return Ok(());
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > period {
            // a slow consumer or conversion: carry on from now rather than send a burst to catch up
            next = now;
        }
    }
}

#[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
mod imp {
    #[cfg(target_os = "macos")]
    pub(super) use super::super::cg::{list, open};
    #[cfg(windows)]
    pub(super) use super::super::dxgi::{list, open};
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    pub(super) use super::super::x11::{list, open};
}

/// The most a `wait` may be; a longer one is cut to this.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// A rectangle of pixels in a frame's own coordinates (the top-left pixel is at 0, 0).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }
}

/// A rectangle the display moved, as scrolling does: the pixels of `to` are those of the same size
/// whose top-left pixel was at `from` in the previous frame. `to` is clipped to the frame, and `from`
/// moves with its top-left pixel.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRect {
    pub from: (u32, u32),
    pub to: Rect,
}

impl MoveRect {
    pub fn new(from: (u32, u32), to: Rect) -> MoveRect {
        MoveRect { from, to }
    }
}

/// A display, as [`list_screens`] lists it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenInfo {
    /// The index that [`ScreenCapture::open`] takes.
    pub index: usize,
    pub name: String,
    /// The display's native size in pixels.
    pub width: u32,
    pub height: u32,
    /// Whether this is the primary display (index 0).
    pub primary: bool,
}

/// What a [`ScreenCapture`] shows of the display.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenOptions {
    /// Whether the mouse cursor is drawn into the pictures. Windows and X11 draw it here; macOS
    /// leaves it out whatever this says.
    pub cursor: bool,
}

impl Default for ScreenOptions {
    fn default() -> ScreenOptions {
        ScreenOptions { cursor: true }
    }
}

/// One picture of a display, as [`ScreenCapture::next_frame`] gives it. It borrows the capture, so
/// it is valid until the next call.
///
/// The pixels are 32-bit BGRA (blue first in each pixel, the alpha byte not meaningful), rows
/// `stride` bytes apart. `dirty` and `moved` list what changed since the previous frame; when `whole`
/// is true they are empty and the whole frame must be taken as changed. This type has no `Debug`
/// output, so that pixels are not printed by accident.
///
/// `width` and `height` are the picture's own size. For the whole display (the default) it is the display's
/// size and `origin` is (0, 0); after [`ScreenCapture::set_region`] it is the region's, `origin` is where
/// the picture's top-left pixel lies on the display, and `dirty`, `moved` and `stride` are the picture's
/// own (so a display point (x, y) in the region is the pixel (x - origin.0, y - origin.1)). `display` is
/// always the display's full size.
#[non_exhaustive]
pub struct ScreenFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bgra: &'a [u8],
    pub dirty: &'a [Rect],
    pub moved: &'a [MoveRect],
    pub whole: bool,
    /// When the picture was taken.
    pub at: Instant,
    /// Where the picture's top-left pixel lies on the display: (0, 0) unless a region was asked for.
    pub origin: (u32, u32),
    /// The display's full size in pixels.
    pub display: (u32, u32),
}

impl<'a> ScreenFrame<'a> {
    /// A frame made outside a capture, such as a test's or another source's. `whole` starts true and
    /// `dirty` and `moved` empty; set them with [`ScreenFrame::with_changes`].
    ///
    /// # Panics
    ///
    /// If `stride` is less than `width * 4`, or `bgra` holds fewer than `stride * height` bytes.
    pub fn new(width: u32, height: u32, stride: usize, bgra: &'a [u8], at: Instant) -> ScreenFrame<'a> {
        let row = width as usize * 4;
        assert!(stride >= row, "stride {stride} is less than {row} bytes for width {width}");
        let need = stride.checked_mul(height as usize).expect("frame size overflows");
        assert!(bgra.len() >= need, "{} bytes given, {need} needed", bgra.len());
        ScreenFrame { width, height, stride, bgra, dirty: &[], moved: &[], whole: true, at, origin: (0, 0), display: (width, height) }
    }

    /// The same frame as the part of a `display`-sized display whose top-left pixel is at `origin`.
    pub fn with_region(self, origin: (u32, u32), display: (u32, u32)) -> ScreenFrame<'a> {
        ScreenFrame { origin, display, ..self }
    }

    /// The same frame with what changed since the previous one. With `whole` true, `dirty` and
    /// `moved` must be empty, as a capture gives them.
    ///
    /// # Panics
    ///
    /// If `whole` is true and `dirty` or `moved` is not empty.
    pub fn with_changes(self, whole: bool, dirty: &'a [Rect], moved: &'a [MoveRect]) -> ScreenFrame<'a> {
        assert!(!whole || (dirty.is_empty() && moved.is_empty()), "a whole frame lists no changes");
        ScreenFrame { dirty, moved, whole, ..self }
    }
}

/// What changed since the previous frame, as a backend reports it.
#[derive(Default)]
pub(crate) struct Changes {
    pub whole: bool,
    pub dirty: Vec<Rect>,
    pub moved: Vec<MoveRect>,
}

/// An open display, read a picture at a time with what changed in each. Opening it reads everything
/// on the display, so open only a display the person has asked to have captured.
///
/// It is not `Send`: it is used on the thread that opened it.
#[non_exhaustive]
pub struct ScreenCapture {
    grabber: Box<dyn Grabber>,
    changes: Changes,
    /// The part of the display asked for.
    region: Option<Rect>,
    /// The backend crops to `region` itself.
    native: bool,
    /// What the software crop last gave, to tell when the region moved.
    last_view: Option<Rect>,
    crop: Vec<u8>,
    crop_changes: Changes,
}

/// The displays present, the primary one first. A display's index is what
/// [`ScreenCapture::open`] takes.
pub fn list_screens() -> Result<Vec<ScreenInfo>> {
    Ok(list_result()?.into_iter().enumerate().map(|(index, d)| ScreenInfo { index, name: d.name, width: d.width, height: d.height, primary: index == 0 }).collect())
}

/// The displays present, or an error when the platform cannot list them.
pub(crate) fn list_result() -> Result<Vec<Listed>> {
    #[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
    {
        imp::list()
    }
    #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
    {
        Err(Error::new(ErrorCode::HardwareUnavailable, "screen capture is not supported on this platform yet").hint("Windows, macOS and 64-bit Linux (X11) are supported"))
    }
}

/// The displays present; none when the platform cannot say.
pub(crate) fn list() -> Vec<Listed> {
    list_result().unwrap_or_default()
}

impl ScreenCapture {
    /// Opens the display at `index` (see [`list_screens`]).
    pub fn open(index: usize, options: ScreenOptions) -> Result<ScreenCapture> {
        #[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
        {
            let grabber = imp::open(index, options.cursor)?;
            Ok(ScreenCapture { grabber: Box::new(grabber), changes: Changes::default(), region: None, native: false, last_view: None, crop: Vec::new(), crop_changes: Changes::default() })
        }
        #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
        {
            let _ = (index, options);
            Err(Error::new(ErrorCode::HardwareUnavailable, "screen capture is not supported on this platform yet").hint("Windows, macOS and 64-bit Linux (X11) are supported"))
        }
    }

    /// The display's next picture. Waits up to `wait` (at most an hour) for the display to change, and
    /// returns `Ok(None)` when nothing changed within that time. The first frame that comes back is
    /// the display as it is at the call, and it is whole.
    ///
    /// On Windows the rectangles come from DXGI Desktop Duplication. The first frame, and the first
    /// after the duplication was lost (a mode change, the secure desktop), is `whole`. A mouse-only
    /// update has no picture change: it is a frame only when the cursor is drawn (`ScreenOptions::cursor`),
    /// and then its `dirty` lists the cursor's old and new places; otherwise it is not reported.
    ///
    /// On X11 and macOS no rectangles are available, so every frame is `whole`. The call waits `wait`
    /// and then takes a picture, so it never returns `None` there; pass a `wait` to pace the loop.
    ///
    /// With a region set ([`ScreenCapture::set_region`]) the frame is only that part of the display.
    pub fn next_frame(&mut self, wait: Duration) -> Result<Option<ScreenFrame<'_>>> {
        let wait = wait.min(MAX_WAIT);
        if let Some(r) = self.region {
            if clamp_region(r, self.grabber.size()).is_none() {
                // wholly off the display: keep the pace, and say so
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                self.last_view = None;
                return Ok(Some(empty_frame(self.grabber.size())));
            }
        }
        let Some(shot) = self.grabber.grab_within(wait, &mut self.changes)? else {
            return Ok(None);
        };
        let at = Instant::now();
        let Some(want) = self.region.filter(|_| !self.native) else {
            let c = &self.changes;
            return Ok(Some(ScreenFrame { width: shot.width, height: shot.height, stride: shot.stride, bgra: shot.data, dirty: &c.dirty, moved: &c.moved, whole: c.whole, at, origin: shot.origin, display: shot.display }));
        };
        // the backend gives the whole display: crop its picture here
        let Some(view) = clamp_region(want, shot.display) else {
            self.last_view = None;
            return Ok(Some(empty_frame(shot.display)));
        };
        if !crop_bgra(shot.data, shot.stride, view, &mut self.crop) {
            return Err(Error::new(ErrorCode::InvalidInput, format!("the screen gave a picture of {} bytes, too short for {}x{}", shot.data.len(), shot.width, shot.height)));
        }
        let whole = self.changes.whole || self.last_view != Some(view);
        self.last_view = Some(view);
        if whole {
            self.crop_changes.dirty.clear();
            self.crop_changes.moved.clear();
        } else {
            crop_changes(view, &self.changes.dirty, &self.changes.moved, &mut self.crop_changes);
        }
        let c = &self.crop_changes;
        Ok(Some(ScreenFrame { width: view.width, height: view.height, stride: view.width as usize * 4, bgra: &self.crop, dirty: &c.dirty, moved: &c.moved, whole, at, origin: (view.x, view.y), display: shot.display }))
    }

    /// Asks for only `region` of the display, from the next [`ScreenCapture::next_frame`] on; `None`
    /// goes back to the whole display. The region is in display pixels (the top-left pixel is 0, 0)
    /// and may be changed between any two calls, to follow a window, say.
    ///
    /// The region is clipped to the display, to any size and position (no rounding, so odd sizes and
    /// origins are exact). The frame is exactly the display's pixels in that rectangle, plus the cursor
    /// where it lies inside it, and carries its `origin`, its own size and the `display`'s size. Its
    /// `dirty` and `moved` are the region's own (intersected with it and moved to its origin; a move
    /// whose source is outside the region is given as dirty instead). The first frame after the
    /// region's origin or size changed is `whole`.
    ///
    /// A region wholly off the display gives, after waiting `wait` (to keep the pace), a frame of
    /// size 0 by 0 with no pixels, `whole` set and `origin` (0, 0).
    ///
    /// On Windows only the region is copied from the GPU, mapped and moved on the CPU, so the cost
    /// scales with its area; changes elsewhere on the display do not produce frames. X11 and macOS
    /// crop the whole display's picture in software: the same frames, at the whole display's cost.
    pub fn set_region(&mut self, region: Option<Rect>) {
        self.region = region;
        self.native = self.grabber.set_region(region);
    }

    /// The region last given to [`ScreenCapture::set_region`].
    pub fn region(&self) -> Option<Rect> {
        self.region
    }
}

/// What a call that fills a buffer reports: the entries written, or the number it needs.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Fill {
    Done(usize),
    Need(usize),
}

/// Fills `buf` by calling `call` with it, growing `buf` and calling again when the call says it needs
/// more entries (a few times at most). Returns the number of entries written, or `None` when the
/// buffer still does not hold them.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn fill_buffer<T: Default + Clone, E>(buf: &mut Vec<T>, mut call: impl FnMut(&mut [T]) -> std::result::Result<Fill, E>) -> std::result::Result<Option<usize>, E> {
    for _ in 0..4 {
        match call(buf.as_mut_slice())? {
            Fill::Done(n) => return Ok((n <= buf.len()).then_some(n)),
            Fill::Need(n) if n > buf.len() => buf.resize(n, T::default()),
            Fill::Need(_) => return Ok(None),
        }
    }
    Ok(None)
}

/// A frame of no pixels, for a region that lies off the `display`.
fn empty_frame(display: (u32, u32)) -> ScreenFrame<'static> {
    ScreenFrame { width: 0, height: 0, stride: 0, bgra: &[], dirty: &[], moved: &[], whole: true, at: Instant::now(), origin: (0, 0), display }
}

/// The part of a `display`-sized display that `region` covers; `None` when none of it does.
pub(crate) fn clamp_region(region: Rect, display: (u32, u32)) -> Option<Rect> {
    clipped(region.x as i64, region.y as i64, region.x as i64 + region.width as i64, region.y as i64 + region.height as i64, display.0, display.1)
}

/// Copies `rows` rows of `row` bytes, `pitch` bytes apart in `src`, into `dst` (replacing it). The last
/// row need not be followed by padding. False, leaving `dst` empty, when `src` is too short.
pub(crate) fn copy_rows(src: &[u8], pitch: usize, row: usize, rows: usize, dst: &mut Vec<u8>) -> bool {
    dst.clear();
    if rows == 0 || row == 0 {
        return true;
    }
    let need = pitch.checked_mul(rows - 1).and_then(|n| n.checked_add(row));
    if (rows > 1 && pitch < row) || need.is_none_or(|n| src.len() < n) {
        return false;
    }
    dst.reserve(row * rows);
    for y in 0..rows {
        dst.extend_from_slice(&src[y * pitch..y * pitch + row]);
    }
    true
}

/// Copies `view` out of a BGRA picture whose rows are `stride` bytes apart into `dst`, tightly packed.
/// False when the picture does not hold it.
pub(crate) fn crop_bgra(data: &[u8], stride: usize, view: Rect, dst: &mut Vec<u8>) -> bool {
    let (x, y, w) = (view.x as usize, view.y as usize, view.width as usize);
    let start = y.checked_mul(stride).and_then(|n| n.checked_add(x * 4));
    let ok = (x + w) * 4 <= stride;
    match start.and_then(|s| data.get(s..)) {
        Some(src) if ok => copy_rows(src, stride, w * 4, view.height as usize, dst),
        _ => {
            dst.clear();
            false
        }
    }
}

/// Writes into `out` the changes of the display (`dirty` and `moved`, in display coordinates) as they
/// are in `view`, in the view's own coordinates: rectangles are intersected with the view; a move
/// keeps its source when that lies inside the view too, and is a dirty rectangle otherwise. `out.whole`
/// is not touched.
pub(crate) fn crop_changes(view: Rect, dirty: &[Rect], moved: &[MoveRect], out: &mut Changes) {
    out.dirty.clear();
    out.moved.clear();
    let (vx, vy) = (view.x as i64, view.y as i64);
    let inside = |r: &Rect| clipped(r.x as i64 - vx, r.y as i64 - vy, r.x as i64 + r.width as i64 - vx, r.y as i64 + r.height as i64 - vy, view.width, view.height);
    for d in dirty {
        out.dirty.extend(inside(d));
    }
    for m in moved {
        let Some(to) = inside(&m.to) else { continue };
        // the clipped destination's source: the move's own, shifted as the destination was clipped
        let fx = m.from.0 as i64 + (to.x as i64 + vx - m.to.x as i64) - vx;
        let fy = m.from.1 as i64 + (to.y as i64 + vy - m.to.y as i64) - vy;
        if fx >= 0 && fy >= 0 && fx + to.width as i64 <= view.width as i64 && fy + to.height as i64 <= view.height as i64 {
            out.moved.push(MoveRect { from: (fx as u32, fy as u32), to });
        } else {
            out.dirty.push(to);
        }
    }
}

/// The rectangle with the given edges (right and bottom exclusive) clipped to a `w` by `h` picture;
/// `None` when none of it lies on the picture.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clipped(left: i64, top: i64, right: i64, bottom: i64, w: u32, h: u32) -> Option<Rect> {
    let (l, t) = (left.max(0), top.max(0));
    let (r, b) = (right.min(w as i64), bottom.min(h as i64));
    (r > l && b > t).then(|| Rect { x: l as u32, y: t as u32, width: (r - l) as u32, height: (b - t) as u32 })
}

/// Opens `device://screen[/N]`./// Opens `device://screen[/N]`.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<ThreadSource> {
    let wants = parse_wants(u, parse_target(&u.target)?)?;
    #[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
    {
        use crate::source::SourceInfo;
        use std::sync::mpsc::sync_channel;
        let info = SourceInfo { url: shown, kind: "device", live: true, synthetic: false };
        let (tx, rx) = sync_channel(4);
        let (ok_tx, ok_rx) = sync_channel(1);
        // the backend's objects belong to the thread that made them, so it opens the display
        std::thread::Builder::new()
            .name("ironav-screen".into())
            .spawn(move || {
                let mut g = match imp::open(wants.index, wants.cursor) {
                    Ok(g) => g,
                    Err(e) => {
                        let _ = ok_tx.send(Err(e));
                        return;
                    }
                };
                let (w, h) = fit_size(g.size(), wants.size);
                let _ = ok_tx.send(Ok(()));
                if send(&tx, super::y4m_header(w, h, rational(wants.fps))) {
                    if let Err(e) = pump(&mut g, &wants, w as usize, h as usize, &tx) {
                        let _ = tx.send(Err(std::io::Error::other(e.message)));
                    }
                }
            })
            .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start capturing: {e}")))?;
        ok_rx.recv().unwrap_or_else(|_| Err(Error::new(ErrorCode::Internal, "the capture thread stopped before the screen opened")))?;
        Ok(ThreadSource::from_channel(info, rx).stall_after(super::DEVICE_STALL))
    }
    #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
    {
        let _ = (wants, shown);
        Err(Error::new(ErrorCode::HardwareUnavailable, "screen capture is not supported on this platform yet").hint("Windows, macOS and 64-bit Linux (X11) are supported"))
    }
}

/// The name a display is listed by.
#[cfg_attr(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))), allow(dead_code))]
pub(crate) fn display_name(label: &str, index: usize) -> String {
    if index == 0 {
        format!("{label} (primary)")
    } else {
        label.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::y4m_header;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn rectangles_are_clipped_to_the_picture() {
        let r = |x, y, width, height| Some(Rect { x, y, width, height });
        assert_eq!(clipped(0, 0, 10, 10, 100, 100), r(0, 0, 10, 10));
        assert_eq!(clipped(-5, -5, 5, 5, 100, 100), r(0, 0, 5, 5));
        assert_eq!(clipped(95, 90, 120, 120, 100, 100), r(95, 90, 5, 10));
        assert_eq!(clipped(-20, 0, -10, 5, 100, 100), None, "left of the picture");
        assert_eq!(clipped(100, 0, 110, 5, 100, 100), None, "the right edge is exclusive");
        assert_eq!(clipped(5, 5, 5, 9, 100, 100), None, "empty");
        assert_eq!(clipped(i64::MIN, i64::MIN, i64::MAX, i64::MAX, u32::MAX, 1), r(0, 0, u32::MAX, 1));
    }

    #[test]
    fn a_buffer_grows_to_what_a_call_needs() {
        let mut buf = vec![0u8; 2];
        let mut calls = 0;
        let got = fill_buffer(&mut buf, |b: &mut [u8]| -> std::result::Result<Fill, ()> {
            calls += 1;
            if b.len() < 5 {
                return Ok(Fill::Need(5));
            }
            b[..3].copy_from_slice(&[1, 2, 3]);
            Ok(Fill::Done(3))
        });
        assert_eq!(got, Ok(Some(3)));
        assert_eq!((buf.len(), calls, &buf[..3]), (5, 2, &[1u8, 2, 3][..]));
        // a count past the buffer is refused, a call that keeps asking for more gives up, and an error passes
        let mut two = vec![0u8; 2];
        assert_eq!(fill_buffer(&mut two, |_: &mut [u8]| Ok::<_, ()>(Fill::Done(3))), Ok(None));
        assert_eq!(fill_buffer(&mut two, |_: &mut [u8]| Ok::<_, ()>(Fill::Need(1 << 20))), Ok(None));
        assert_eq!(fill_buffer(&mut two, |_: &mut [u8]| Err::<Fill, _>("gone")), Err("gone"));
    }

    #[test]
    #[ignore = "needs an interactive desktop"]
    fn the_primary_display_gives_a_whole_first_frame_and_its_size() {
        // only counts, sizes and timing are looked at; no picture is kept or printed
        let mut cap = ScreenCapture::open(0, ScreenOptions::default()).expect("the primary display");
        let start = Instant::now();
        let (mut frames, mut first_whole, mut size, mut sizes_ok) = (0usize, None::<bool>, None::<(u32, u32)>, true);
        while start.elapsed() < Duration::from_secs(2) {
            let Some(f) = cap.next_frame(Duration::from_millis(100)).expect("a frame") else {
                continue;
            };
            frames += 1;
            first_whole.get_or_insert(f.whole);
            let s = (f.width, f.height);
            sizes_ok &= *size.get_or_insert(s) == s;
            sizes_ok &= f.stride >= f.width as usize * 4 && f.bgra.len() >= f.stride * f.height as usize;
            sizes_ok &= f.dirty.iter().all(|r| r.x + r.width <= f.width && r.y + r.height <= f.height);
        }
        let took = start.elapsed();
        println!("screen: {frames} frames in {:.2} s, first frame whole: {first_whole:?}, size {size:?}", took.as_secs_f64());
        assert!(frames > 0, "no frame in {took:?}");
        assert_eq!(first_whole, Some(true));
        assert!(sizes_ok);
    }

    fn shot(px: &[[u8; 4]], w: u32, h: u32) -> Vec<u8> {
        assert_eq!(px.len(), (w * h) as usize);
        px.iter().flatten().copied().collect()
    }

    fn convert(px: &[[u8; 4]], w: u32, h: u32, dw: usize, dh: usize) -> Vec<u8> {
        let d = shot(px, w, h);
        let f = bgra_to_y4m(&Shot::new(&d, w as usize * 4, w, h), dw, dh).unwrap();
        assert_eq!(&f[..6], b"FRAME\n");
        f[6..].to_vec()
    }

    fn close(a: u8, b: u8) -> bool {
        a.abs_diff(b) <= 1
    }

    #[test]
    fn known_colours_convert_with_bt709_limited_range() {
        // (blue, green, red, alpha) -> the Y, U, V of BT.709 limited range
        let cases: [([u8; 4], [u8; 3]); 7] = [
            ([255, 255, 255, 255], [235, 128, 128]),
            ([0, 0, 0, 255], [16, 128, 128]),
            ([0, 0, 255, 255], [63, 102, 240]),
            ([0, 255, 0, 255], [173, 42, 26]),
            ([255, 0, 0, 255], [32, 240, 118]),
            ([128, 128, 128, 255], [126, 128, 128]),
            // the alpha byte (often 0 or garbage in a capture) is not used
            ([255, 255, 255, 0], [235, 128, 128]),
        ];
        for (bgra, want) in cases {
            let out = convert(&[bgra; 4], 2, 2, 2, 2);
            assert_eq!(out.len(), 6, "four luma and two chroma samples");
            assert!(out[..4].iter().all(|&y| close(y, want[0])), "{bgra:?} luma {:?} vs {want:?}", &out[..4]);
            assert!(close(out[4], want[1]) && close(out[5], want[2]), "{bgra:?} chroma {:?} vs {want:?}", &out[4..]);
        }
    }

    #[test]
    fn chroma_averages_a_block_and_rows_may_be_padded() {
        // a 2x2 block of red, red, blue, blue: luma per pixel, chroma of the mean
        let (red, blue) = ([0, 0, 255, 255], [255, 0, 0, 255]);
        let mut d = shot(&[red, red, blue, blue], 2, 2);
        let out = convert(&[red, red, blue, blue], 2, 2, 2, 2);
        assert!(close(out[0], 63) && close(out[1], 63) && close(out[2], 32) && close(out[3], 32));
        // the mean is (127.5, 0, 127.5)
        let (_, u, v) = yuv(128, 0, 128);
        assert!(close(out[4], u) && close(out[5], v));
        // each row padded by 4 bytes
        let mut padded = Vec::new();
        for row in d.chunks(8) {
            padded.extend_from_slice(row);
            padded.extend_from_slice(&[9; 4]);
        }
        let f = bgra_to_y4m(&Shot::new(&padded, 12, 2, 2), 2, 2).unwrap();
        assert_eq!(&f[6..], &out[..]);
        // too short, or a stride narrower than the picture
        d.truncate(11);
        assert!(bgra_to_y4m(&Shot::new(&d, 8, 2, 2), 2, 2).is_none());
        assert!(bgra_to_y4m(&Shot::new(&padded, 4, 2, 2), 2, 2).is_none());
        assert!(bgra_to_y4m(&Shot::new(&padded, 12, 2, 2), 3, 2).is_none(), "an odd size");
    }

    #[test]
    fn a_picture_is_scaled_down_by_nearest_pixels() {
        // 4x4: the left half white, the right half black, scaled to 2x2
        let (w, k) = ([255, 255, 255, 255], [0, 0, 0, 255]);
        let out = convert(&[w, w, k, k, w, w, k, k, w, w, k, k, w, w, k, k], 4, 4, 2, 2);
        assert!(close(out[0], 235) && close(out[1], 16) && close(out[2], 235) && close(out[3], 16));
        assert!(close(out[4], 128) && close(out[5], 128));
    }

    #[test]
    fn the_output_is_read_as_y4m() {
        let d = vec![200u8; 8 * 6 * 4];
        let mut y = y4m_header(8, 6, rational(30.0));
        for _ in 0..2 {
            y.extend(bgra_to_y4m(&Shot::new(&d, 32, 8, 6), 8, 6).unwrap());
        }
        let mut s = ironav_formats::stream::stream_demuxer(&y, &Default::default()).unwrap();
        s.feed(&y).unwrap();
        let p = s.next_packet().unwrap().unwrap();
        assert_eq!(p.packet.data.len(), 8 * 6 * 3 / 2);
        assert_eq!(black(8, 6).len(), 6 + p.packet.data.len());
    }

    #[test]
    fn sizes_fit_the_box_or_the_default_width() {
        // by default: the native size, but no wider than 1920
        assert_eq!(fit_size((1920, 1080), None), (1920, 1080));
        assert_eq!(fit_size((1366, 768), None), (1366, 768));
        assert_eq!(fit_size((3840, 2160), None), (1920, 1080));
        assert_eq!(fit_size((2560, 1600), None), (1920, 1200));
        assert_eq!(fit_size((1921, 1081), None), (1920, 1080));
        // a box: scaled down to fit, the aspect kept, never scaled up
        assert_eq!(fit_size((2560, 1440), Some((1280, 1280))), (1280, 720));
        assert_eq!(fit_size((1920, 1080), Some((640, 360))), (640, 360));
        assert_eq!(fit_size((1920, 1080), Some((4000, 4000))), (1920, 1080));
        assert_eq!(fit_size((1000, 2000), Some((500, 500))), (250, 500));
        // a box wider than 1920 is the caller's choice
        assert_eq!(fit_size((3840, 2160), Some((3840, 2160))), (3840, 2160));
        // odd native sizes become even
        assert_eq!(fit_size((1365, 767), None), (1364, 766));
        assert_eq!(fit_size((1, 1), None), (2, 2));
    }

    #[test]
    fn the_target_and_options_are_parsed() {
        assert_eq!(parse_target("screen").unwrap(), 0);
        assert_eq!(parse_target("screen/").unwrap(), 0);
        assert_eq!(parse_target("screen/2").unwrap(), 2);
        assert_eq!(parse_target("screen/main").unwrap_err().code, ErrorCode::InvalidArgument);

        let wants = |url: &str| parse_wants(&SourceUrl::parse(url).unwrap(), 0);
        let w = wants("device://screen").unwrap();
        assert_eq!((w.fps, w.size, w.cursor), (30.0, None, true));
        let w = wants("device://screen/1?fps=15&size=1280x720&cursor=0").unwrap();
        assert_eq!((w.fps, w.size, w.cursor), (15.0, Some((1280, 720)), false));
        assert_eq!(wants("device://screen?fps=60").unwrap().fps, 60.0);
        assert!(wants("device://screen?fps=61").is_err());
        assert!(wants("device://screen?fps=0.5").is_err());
        assert!(wants("device://screen?size=big").is_err());
        assert!(wants("device://screen?cursor=maybe").is_err());
        // the cursor option is recognized as an option, not taken for part of the target
        assert_eq!(SourceUrl::parse("device://screen?cursor=0").unwrap().target, "screen");
    }

    #[test]
    fn frame_rates_become_fractions_and_displays_get_names() {
        assert_eq!((rational(30.0).num, rational(30.0).den), (30, 1));
        assert_eq!((rational(29.97).num, rational(29.97).den), (2997, 100));
        assert_eq!((rational(7.5).num, rational(7.5).den), (15, 2));
        assert_eq!(display_name("Display 1", 0), "Display 1 (primary)");
        assert_eq!(display_name("Display 2", 1), "Display 2");
    }

    /// A display that shows a picture once, then nothing new.
    struct Still {
        data: Vec<u8>,
        sent: bool,
    }

    impl Grabber for Still {
        fn size(&self) -> (u32, u32) {
            (4, 2)
        }
        fn grab(&mut self) -> Result<Option<Shot<'_>>> {
            Ok(if std::mem::replace(&mut self.sent, true) { None } else { Some(Shot::new(&self.data, 16, 4, 2)) })
        }
    }

    #[test]
    fn an_unchanged_screen_repeats_its_last_frame_at_a_constant_rate() {
        let (tx, rx) = sync_channel(8);
        let wants = Wants { index: 0, fps: 60.0, size: None, cursor: true };
        let mut g = Still { data: vec![255; 32], sent: false };
        let t = std::thread::spawn(move || {
            let r = pump(&mut g, &wants, 4, 2, &tx);
            (r.is_ok(), ())
        });
        let start = Instant::now();
        let frames: Vec<Vec<u8>> = (0..7).map(|_| rx.recv().unwrap().unwrap()).collect();
        let took = start.elapsed();
        drop(rx);
        assert!(t.join().unwrap().0, "closing the source ends the pump quietly");
        assert!(frames.iter().all(|f| f.len() == 6 + 12 && *f == frames[0]));
        // six intervals of 1/60 s lie between seven frames
        assert!(took >= Duration::from_millis(90), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn a_display_that_has_shown_nothing_sends_black() {
        struct Nothing;
        impl Grabber for Nothing {
            fn size(&self) -> (u32, u32) {
                (4, 2)
            }
            fn grab(&mut self) -> Result<Option<Shot<'_>>> {
                Ok(None)
            }
        }
        let (tx, rx) = sync_channel(2);
        let wants = Wants { index: 0, fps: 60.0, size: None, cursor: true };
        let t = std::thread::spawn(move || pump(&mut Nothing, &wants, 4, 2, &tx).is_ok());
        assert_eq!(rx.recv().unwrap().unwrap(), black(4, 2));
        drop(rx);
        assert!(t.join().unwrap());
    }

    /// A `w` by `h` display of which pixel (x, y) is [x, y, 7, 255], rows `pitch` bytes apart (the padding is 0xEE).
    fn pattern(w: u32, h: u32, pitch: usize) -> Vec<u8> {
        let mut d = vec![0xEE; pitch * h as usize];
        for y in 0..h {
            for x in 0..w {
                let i = y as usize * pitch + x as usize * 4;
                d[i..i + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        d
    }

    /// Whether `data` (rows `stride` apart) holds, at `origin`, the pattern's pixels for `w` by `h`.
    fn is_pattern(data: &[u8], stride: usize, origin: (u32, u32), w: u32, h: u32) -> bool {
        (0..h).all(|y| (0..w).all(|x| data[y as usize * stride + x as usize * 4..][..4] == [(origin.0 + x) as u8, (origin.1 + y) as u8, 7, 255]))
    }

    #[test]
    fn a_region_is_clamped_to_the_display() {
        let r = |x, y, width, height| Some(Rect { x, y, width, height });
        let c = |x, y, w, h| clamp_region(Rect::new(x, y, w, h), (100, 50));
        assert_eq!(c(0, 0, 100, 50), r(0, 0, 100, 50), "the whole display");
        assert_eq!(c(10, 20, 30, 10), r(10, 20, 30, 10), "inside");
        assert_eq!(c(90, 40, 30, 30), r(90, 40, 10, 10), "past the lower right corner");
        assert_eq!(c(99, 49, 1, 1), r(99, 49, 1, 1), "the last pixel");
        assert_eq!(c(100, 0, 10, 10), None, "just right of the display");
        assert_eq!(c(0, 50, 10, 10), None, "just below the display");
        assert_eq!(c(5, 5, 0, 10), None, "no width");
        assert_eq!(c(5, 5, 10, 0), None, "no height");
        assert_eq!(c(u32::MAX, u32::MAX, u32::MAX, u32::MAX), None, "far off, and no overflow");
        assert_eq!(c(1, 1, u32::MAX, u32::MAX), r(1, 1, 99, 49), "huge");
        assert_eq!(c(3, 3, 7, 5), r(3, 3, 7, 5), "odd sizes and origins are kept");
    }

    #[test]
    fn rows_are_copied_from_a_buffer_with_padding() {
        // 3 rows of 2 pixels (8 bytes) a 12-byte pitch apart; the last row has no padding after it
        let src: Vec<u8> = (0..3).flat_map(|y| (0..12).map(move |i| if i < 8 { (y * 10 + i) as u8 } else { 0xEE })).collect();
        let mut dst = vec![9u8; 100];
        assert!(copy_rows(&src[..12 * 2 + 8], 12, 8, 3, &mut dst));
        assert_eq!(dst, [0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 13, 14, 15, 16, 17, 20, 21, 22, 23, 24, 25, 26, 27]);
        // too short for the last row, a pitch narrower than a row, and nothing to copy
        assert!(!copy_rows(&src[..12 * 2 + 7], 12, 8, 3, &mut dst));
        assert!(dst.is_empty());
        assert!(!copy_rows(&src, 4, 8, 3, &mut dst));
        assert!(copy_rows(&src, 12, 8, 0, &mut dst) && dst.is_empty());
        assert!(copy_rows(&src[..8], 4, 8, 1, &mut dst), "a single row needs no pitch");
        assert_eq!(dst, &src[..8]);
    }

    #[test]
    fn a_region_is_cropped_from_a_picture_exactly() {
        // 10x6 display, rows padded to 48 bytes
        let d = pattern(10, 6, 48);
        let mut out = Vec::new();
        for (x, y, w, h) in [(0, 0, 10, 6), (3, 2, 4, 3), (9, 5, 1, 1), (0, 5, 10, 1), (7, 0, 3, 6), (1, 1, 3, 2)] {
            assert!(crop_bgra(&d, 48, Rect::new(x, y, w, h), &mut out), "{x},{y} {w}x{h}");
            assert_eq!(out.len(), (w * h * 4) as usize);
            assert!(is_pattern(&out, w as usize * 4, (x, y), w, h), "{x},{y} {w}x{h}");
        }
        // a region that does not fit the stride or the data is refused
        assert!(!crop_bgra(&d, 48, Rect::new(8, 0, 5, 1), &mut out));
        assert!(!crop_bgra(&d[..48 * 5], 48, Rect::new(0, 5, 2, 1), &mut out));
        assert!(out.is_empty());
    }

    fn changes(view: Rect, dirty: &[Rect], moved: &[MoveRect]) -> (Vec<Rect>, Vec<MoveRect>) {
        let mut out = Changes { whole: false, dirty: vec![Rect::new(9, 9, 9, 9)], moved: vec![MoveRect::new((1, 1), Rect::new(1, 1, 1, 1))] };
        crop_changes(view, dirty, moved, &mut out);
        (out.dirty, out.moved)
    }

    #[test]
    fn changes_are_intersected_with_a_region_and_moved_to_its_origin() {
        let view = Rect::new(100, 50, 40, 30);
        let (dirty, moved) = changes(view, &[Rect::new(110, 60, 5, 5), Rect::new(90, 40, 20, 20), Rect::new(0, 0, 50, 50), Rect::new(130, 70, 50, 50), Rect::new(140, 50, 5, 5), Rect::new(100, 80, 5, 5)], &[]);
        assert_eq!(dirty, [Rect::new(10, 10, 5, 5), Rect::new(0, 0, 10, 10), Rect::new(30, 20, 10, 10)], "outside, and just outside, are dropped; the old contents are replaced");
        assert!(moved.is_empty());
        // a move inside the region keeps its source, translated
        let (dirty, moved) = changes(view, &[], &[MoveRect::new((105, 55), Rect::new(110, 60, 10, 10))]);
        assert!(dirty.is_empty());
        assert_eq!(moved, [MoveRect::new((5, 5), Rect::new(10, 10, 10, 10))]);
        // a destination cut by the region's edge moves its source with it
        let (dirty, moved) = changes(view, &[], &[MoveRect::new((120, 70), Rect::new(95, 60, 10, 5))]);
        assert!(dirty.is_empty());
        assert_eq!(moved, [MoveRect::new((25, 20), Rect::new(0, 10, 5, 5))]);
        // a source outside the region becomes dirty; a destination outside it is nothing
        let (dirty, moved) = changes(view, &[], &[MoveRect::new((10, 10), Rect::new(110, 60, 10, 10)), MoveRect::new((110, 60), Rect::new(10, 10, 10, 10))]);
        assert_eq!(dirty, [Rect::new(10, 10, 10, 10)]);
        assert!(moved.is_empty());
        // a source that sticks out of the region below makes the move dirty too
        let (dirty, moved) = changes(view, &[], &[MoveRect::new((130, 70), Rect::new(100, 50, 20, 20))]);
        assert_eq!((dirty, moved), (vec![Rect::new(0, 0, 20, 20)], vec![]));
    }

    /// A display that shows `data` (a whole picture) each time, and changes `all` of it.
    struct Pic {
        data: Vec<u8>,
        pitch: usize,
        size: (u32, u32),
        dirty: Vec<Rect>,
    }

    impl Grabber for Pic {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn grab(&mut self) -> Result<Option<Shot<'_>>> {
            Ok(Some(Shot::new(&self.data, self.pitch, self.size.0, self.size.1)))
        }
        fn grab_within(&mut self, _: Duration, changes: &mut Changes) -> Result<Option<Shot<'_>>> {
            changes.whole = self.dirty.is_empty();
            changes.dirty = self.dirty.clone();
            changes.moved.clear();
            self.grab()
        }
    }

    fn capture_of(w: u32, h: u32, pitch: usize, dirty: Vec<Rect>) -> ScreenCapture {
        ScreenCapture { grabber: Box::new(Pic { data: pattern(w, h, pitch), pitch, size: (w, h), dirty }), changes: Changes::default(), region: None, native: false, last_view: None, crop: Vec::new(), crop_changes: Changes::default() }
    }

    #[test]
    fn without_a_region_the_frame_is_the_whole_display() {
        let mut cap = capture_of(10, 6, 48, Vec::new());
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height, f.stride, f.origin, f.display), (10, 6, 48, (0, 0), (10, 6)));
        assert!(f.whole && is_pattern(f.bgra, f.stride, (0, 0), 10, 6));
    }

    #[test]
    fn a_region_frame_holds_exactly_the_regions_pixels() {
        let mut cap = capture_of(10, 6, 48, Vec::new());
        for (x, y, w, h) in [(3, 2, 4, 3), (1, 1, 3, 1), (0, 0, 10, 6), (9, 5, 1, 1), (2, 4, 5, 2)] {
            cap.set_region(Some(Rect::new(x, y, w, h)));
            assert_eq!(cap.region(), Some(Rect::new(x, y, w, h)));
            let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
            assert_eq!((f.width, f.height, f.stride, f.origin, f.display), (w, h, w as usize * 4, (x, y), (10, 6)));
            assert_eq!(f.bgra.len(), (w * h * 4) as usize);
            assert!(f.whole, "the region moved");
            assert!(is_pattern(f.bgra, f.stride, (x, y), w, h), "{x},{y} {w}x{h}");
        }
        // a region partly off the display is cut to it, and says where it starts
        cap.set_region(Some(Rect::new(7, 4, 20, 20)));
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height, f.origin), (3, 2, (7, 4)));
        assert!(is_pattern(f.bgra, f.stride, (7, 4), 3, 2));
        // back to the whole display
        cap.set_region(None);
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height, f.origin), (10, 6, (0, 0)));
    }

    #[test]
    fn a_region_off_the_display_gives_a_frame_of_no_size() {
        let mut cap = capture_of(10, 6, 40, Vec::new());
        for r in [Rect::new(10, 0, 4, 4), Rect::new(0, 6, 4, 4), Rect::new(2, 2, 0, 3), Rect::new(u32::MAX, 0, 5, 5)] {
            cap.set_region(Some(r));
            let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
            assert_eq!((f.width, f.height, f.stride, f.bgra.len(), f.display, f.origin, f.whole), (0, 0, 0, 0, (10, 6), (0, 0), true), "{r:?}");
        }
    }

    #[test]
    fn a_region_frames_changes_are_the_regions_own() {
        let mut cap = capture_of(10, 6, 40, vec![Rect::new(0, 0, 2, 2), Rect::new(4, 3, 3, 2), Rect::new(8, 0, 2, 6)]);
        cap.set_region(Some(Rect::new(3, 2, 4, 3)));
        // the first frame at a region is whole, with no rectangles
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        assert!(f.whole && f.dirty.is_empty());
        // then only what touches the region, in its coordinates
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        assert!(!f.whole);
        assert_eq!(f.dirty, [Rect::new(1, 1, 3, 2)]);
        // moving the region makes the next frame whole again; keeping it does not
        cap.set_region(Some(Rect::new(4, 2, 4, 3)));
        assert!(cap.next_frame(Duration::ZERO).unwrap().unwrap().whole);
        cap.set_region(Some(Rect::new(4, 2, 4, 3)));
        assert!(!cap.next_frame(Duration::ZERO).unwrap().unwrap().whole);
    }

    #[test]
    fn a_frame_can_be_made_outside_a_capture() {
        let px = vec![0u8; 16 * 2];
        let at = Instant::now();
        let f = ScreenFrame::new(3, 2, 16, &px, at);
        assert!(f.whole && f.dirty.is_empty() && f.moved.is_empty());
        let dirty = [Rect::new(1, 0, 2, 1)];
        let moved = [MoveRect::new((0, 1), Rect::new(0, 0, 3, 1))];
        let f = f.with_changes(false, &dirty, &moved);
        assert_eq!((f.width, f.height, f.stride, f.at), (3, 2, 16, at));
        assert_eq!((f.origin, f.display), ((0, 0), (3, 2)));
        let f = f.with_region((5, 6), (10, 10));
        assert_eq!((f.origin, f.display), ((5, 6), (10, 10)));
        assert!(!f.whole);
        assert_eq!(f.dirty, &dirty);
        assert_eq!(f.moved[0].from, (0, 1));
    }

    #[test]
    #[should_panic(expected = "needed")]
    fn a_frame_refuses_too_few_pixels() {
        let px = vec![0u8; 16 * 2 - 1];
        ScreenFrame::new(3, 2, 16, &px, Instant::now());
    }

    #[test]
    #[should_panic(expected = "stride")]
    fn a_frame_refuses_a_short_stride() {
        let px = vec![0u8; 64];
        ScreenFrame::new(5, 2, 16, &px, Instant::now());
    }

    #[test]
    #[should_panic(expected = "no changes")]
    fn a_whole_frame_lists_no_changes() {
        let px = vec![0u8; 16];
        let dirty = [Rect::new(0, 0, 1, 1)];
        ScreenFrame::new(4, 1, 16, &px, Instant::now()).with_changes(true, &dirty, &[]);
    }
}
