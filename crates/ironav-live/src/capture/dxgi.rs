//! Screens on Windows through DXGI Desktop Duplication: the display's picture is copied to a
//! staging texture on the GPU that drives it, mapped, and handed over as 32-bit BGRA.
//!
//! The duplication gives the desktop without the mouse cursor, whose position and shape come with
//! it; the cursor is drawn here, when asked for, into a copy of the picture. When the duplication is
//! lost (a mode change, or the secure desktop of a UAC prompt or the lock screen), it is made again
//! on the following pictures, and the last picture is repeated meanwhile.

use super::screen::{clipped, display_name, fill_buffer, Changes, Fill, Grabber, Listed, MoveRect, Rect, Shot};
use ironav_core::{Error, ErrorCode, Result};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MORE_DATA, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

/// How long a duplication may stay lost before the stream fails: a UAC prompt or the lock screen
/// can hold the secure desktop for a while.
const LOST_FOR: Duration = Duration::from_secs(30);

/// E_ACCESSDENIED
const ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;

fn fail(what: &str, e: windows::core::Error) -> Error {
    let code = e.code();
    let err = Error::new(ErrorCode::HardwareUnavailable, format!("{what}: {}", e.message()));
    if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        err.hint("the system limits how many programs may duplicate the desktop at once; close the others (screen recorders, remote-desktop tools) and try again")
    } else if code == DXGI_ERROR_UNSUPPORTED || code == DXGI_ERROR_SESSION_DISCONNECTED {
        err.hint("Desktop Duplication is not available in this session (a remote desktop, a disconnected session, or a driver without support); capture needs an interactive desktop on a display adapter")
    } else if code.0 == ACCESS_DENIED {
        err.hint("Windows refuses the capture on the secure desktop (a UAC prompt or the lock screen) and in sessions that are not interactive")
    } else {
        err
    }
}

/// A display found by the factory.
struct Found {
    adapter: windows::Win32::Graphics::Dxgi::IDXGIAdapter1,
    output: IDXGIOutput,
    name: String,
    width: u32,
    height: u32,
}

/// The displays attached to the desktop, the primary one (whose corner is the desktop's origin)
/// first, then in the order adapters and outputs are enumerated.
fn found() -> Result<Vec<Found>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| fail("listing displays", e))?;
    let mut out = Vec::new();
    let mut primary = None;
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let end = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
            let name = String::from_utf16_lossy(&desc.DeviceName[..end]);
            if r.left == 0 && r.top == 0 && primary.is_none() {
                primary = Some(out.len());
            }
            out.push(Found { adapter: adapter.clone(), output, name, width: (r.right - r.left).max(0) as u32, height: (r.bottom - r.top).max(0) as u32 });
        }
    }
    if let Some(p) = primary {
        let d = out.remove(p);
        out.insert(0, d);
    }
    Ok(out)
}

pub(super) fn list() -> Result<Vec<Listed>> {
    Ok(found()?.into_iter().enumerate().map(|(i, f)| Listed { name: display_name(&f.name, i), width: f.width, height: f.height }).collect())
}

/// The mouse cursor's picture, as Desktop Duplication describes it.
struct Pointer {
    kind: u32,
    width: usize,
    height: usize,
    pitch: usize,
    data: Vec<u8>,
    x: i32,
    y: i32,
    visible: bool,
}

impl Pointer {
    /// The rectangle the cursor covers on a `w` by `h` picture.
    fn rect(&self, w: u32, h: u32) -> Option<Rect> {
        let rows = if self.kind == 1 { self.height / 2 } else { self.height };
        clipped(self.x as i64, self.y as i64, self.x as i64 + self.width as i64, self.y as i64 + rows as i64, w, h)
    }
}

/// Draws the cursor into 32-bit BGRA pixels `stride` bytes apart (`w` by `h`), clipping at the
/// edges. Colour cursors blend by their alpha; masked colour cursors replace the picture where
/// their alpha is 0 and exclusive-or it where it is 255; monochrome ones are an AND mask over an
/// XOR mask (stacked, so their height is twice the cursor's).
fn draw_pointer(canvas: &mut [u8], stride: usize, w: usize, h: usize, p: &Pointer) {
    let rows = if p.kind == 1 { p.height / 2 } else { p.height };
    for cy in 0..rows {
        let y = p.y + cy as i32;
        if y < 0 || y as usize >= h {
            continue;
        }
        for cx in 0..p.width {
            let x = p.x + cx as i32;
            if x < 0 || x as usize >= w {
                continue;
            }
            let dst = y as usize * stride + x as usize * 4;
            let Some(dst) = canvas.get_mut(dst..dst + 4) else { continue };
            match p.kind {
                1 => {
                    let bit = |row: usize| p.data.get(row * p.pitch + cx / 8).map_or(1, |b| (b >> (7 - cx % 8)) & 1);
                    let (and, xor) = (if bit(cy) == 1 { 0xFF } else { 0 }, if bit(cy + rows) == 1 { 0xFF } else { 0 });
                    for c in &mut dst[..3] {
                        *c = (*c & and) ^ xor;
                    }
                }
                2 | 4 => {
                    let Some(s) = p.data.get(cy * p.pitch + cx * 4..cy * p.pitch + cx * 4 + 4) else { continue };
                    if p.kind == 2 {
                        let a = s[3] as u32;
                        for i in 0..3 {
                            dst[i] = ((s[i] as u32 * a + dst[i] as u32 * (255 - a) + 127) / 255) as u8;
                        }
                    } else if s[3] == 0 {
                        dst[..3].copy_from_slice(&s[..3]);
                    } else {
                        for i in 0..3 {
                            dst[i] ^= s[i];
                        }
                    }
                }
                _ => return,
            }
        }
    }
}

/// What an acquired frame brought.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Nothing to report: a timeout, a duplication that was lost, or a cursor the caller does not draw.
    Nothing,
    /// The desktop changed, and the cursor may have moved with it.
    Desktop,
    /// Only the cursor moved or changed its shape.
    Cursor,
}

pub(super) struct Dxgi {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// The latest desktop, without the cursor.
    desktop: Vec<u8>,
    /// The desktop with the cursor drawn, when it is asked for and visible.
    shown: Vec<u8>,
    size: (u32, u32),
    cursor: bool,
    pointer: Option<Pointer>,
    /// The rectangles of a frame, kept between frames.
    dirty_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    /// The changes of a `grab` that has no caller to write them to.
    scratch: Changes,
    /// The next frame is whole: the first, and the first after the duplication was lost.
    whole_pending: bool,
    /// The picture taken at open has not been given yet.
    first_ready: bool,
    lost_since: Option<Instant>,
}

pub(super) fn open(index: usize, cursor: bool) -> Result<Dxgi> {
    let list = found()?;
    let Some(f) = list.into_iter().nth(index) else {
        let shown = self::list().unwrap_or_default();
        return Err(Error::new(ErrorCode::NotFound, format!("no display {index}")).alternatives(shown.iter().enumerate().map(|(i, d)| format!("device://screen/{i} ({})", d.name))));
    };
    let (mut device, mut ctx) = (None, None);
    unsafe { D3D11CreateDevice(&f.adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx)) }.map_err(|e| fail("could not use the display adapter", e))?;
    let (Some(device), Some(ctx)) = (device, ctx) else {
        return Err(Error::new(ErrorCode::HardwareUnavailable, "the display adapter gave no Direct3D device"));
    };
    let output = f.output.cast::<IDXGIOutput1>().map_err(|e| fail("Desktop Duplication needs DXGI 1.2", e))?;
    let mut d = Dxgi { device, ctx, output, dup: None, staging: None, desktop: Vec::new(), shown: Vec::new(), size: (f.width, f.height), cursor, pointer: None, dirty_buf: Vec::new(), move_buf: Vec::new(), scratch: Changes::default(), whole_pending: true, first_ready: false, lost_since: None };
    d.dup = Some(d.duplicate()?);
    // a still desktop may not have a picture ready at once: give the first one a moment
    d.first_ready = matches!(d.acquire(500, &mut Changes::default())?, Outcome::Desktop);
    Ok(d)
}

impl Dxgi {
    fn duplicate(&self) -> Result<IDXGIOutputDuplication> {
        unsafe { self.output.DuplicateOutput(&self.device) }.map_err(|e| fail("could not duplicate the display", e))
    }

    /// Takes the next frame if one comes within `ms` (0: only one that is ready), and writes what it
    /// changed into `out`, which is left empty when nothing shows.
    fn acquire(&mut self, ms: u32, out: &mut Changes) -> Result<Outcome> {
        out.whole = false;
        out.dirty.clear();
        out.moved.clear();
        if self.dup.is_none() {
            match self.duplicate() {
                Ok(d) => {
                    self.dup = Some(d);
                    self.lost_since = None;
                }
                Err(e) => {
                    // the picture stays as it was until the secure desktop is gone
                    let since = *self.lost_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > LOST_FOR {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(u64::from(ms.min(50))));
                    return Ok(Outcome::Nothing);
                }
            }
        }
        let dup = self.dup.clone().expect("a duplication");
        let (mut info, mut res) = (DXGI_OUTDUPL_FRAME_INFO::default(), None::<IDXGIResource>);
        match unsafe { dup.AcquireNextFrame(ms, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Outcome::Nothing),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // the duplication is made again for the next frame, which is whole
                self.dup = None;
                self.whole_pending = true;
                self.lost_since.get_or_insert_with(Instant::now);
                return Ok(Outcome::Nothing);
            }
            Err(e) => return Err(fail("reading the screen", e)),
        }
        let got = self.take(&dup, &info, res, out);
        unsafe {
            let _ = dup.ReleaseFrame();
        }
        got
    }

    /// Reads an acquired frame: its picture when the desktop changed, and the cursor when it moved.
    /// A cursor update has a last-present time of zero, and is reported only when the cursor is drawn.
    fn take(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>, out: &mut Changes) -> Result<Outcome> {
        let before = self.cursor_rect();
        let pointer = self.cursor && info.LastMouseUpdateTime != 0;
        if pointer {
            self.update_pointer(dup, info);
        }
        let old_size = self.size;
        let picture = match res {
            Some(res) if info.LastPresentTime != 0 => {
                self.copy(&res)?;
                true
            }
            _ => false,
        };
        if !picture && (!pointer || self.desktop.is_empty()) {
            return Ok(Outcome::Nothing);
        }
        let known = picture && self.read_rects(dup, out);
        out.whole = self.whole_pending || (picture && (!known || self.size != old_size));
        if out.whole {
            out.dirty.clear();
            out.moved.clear();
        } else if pointer {
            // the cursor's old and new places are both changed
            out.dirty.extend([before, self.cursor_rect()].into_iter().flatten());
        }
        Ok(if picture { Outcome::Desktop } else { Outcome::Cursor })
    }

    /// Reads the frame's dirty and move rectangles into `out`, clipped to the picture; false when the
    /// duplication does not give them or a move's source is not on the picture (the frame is then whole).
    fn read_rects(&mut self, dup: &IDXGIOutputDuplication, out: &mut Changes) -> bool {
        let (w, h) = self.size;
        let dirty = fill_buffer(&mut self.dirty_buf, |b| {
            let mut need = 0u32;
            match unsafe { dup.GetFrameDirtyRects(std::mem::size_of_val(b) as u32, b.as_mut_ptr(), &mut need) } {
                Ok(()) => Ok(Fill::Done(need as usize / std::mem::size_of::<RECT>())),
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => Ok(Fill::Need((need as usize).div_ceil(std::mem::size_of::<RECT>()))),
                Err(e) => Err(e),
            }
        });
        let moves = fill_buffer(&mut self.move_buf, |b| {
            let mut need = 0u32;
            match unsafe { dup.GetFrameMoveRects(std::mem::size_of_val(b) as u32, b.as_mut_ptr(), &mut need) } {
                Ok(()) => Ok(Fill::Done(need as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>())),
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => Ok(Fill::Need((need as usize).div_ceil(std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()))),
                Err(e) => Err(e),
            }
        });
        match (dirty, moves) {
            (Ok(Some(n)), Ok(Some(m))) => {
                rects_of(&self.dirty_buf[..n], w, h, &mut out.dirty);
                moves_of(&self.move_buf[..m], w, h, &mut out.moved)
            }
            _ => false,
        }
    }

    /// Takes the cursor's place and, when the duplication gives it, its picture.
    fn update_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        let pos = info.PointerPosition;
        if info.PointerShapeBufferSize > 0 {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let (mut need, mut shape) = (0u32, DXGI_OUTDUPL_POINTER_SHAPE_INFO::default());
            if unsafe { dup.GetFramePointerShape(data.len() as u32, data.as_mut_ptr().cast(), &mut need, &mut shape) }.is_ok() {
                self.pointer = Some(Pointer { kind: shape.Type, width: shape.Width as usize, height: shape.Height as usize, pitch: shape.Pitch as usize, data, x: 0, y: 0, visible: false });
            }
        }
        if let Some(p) = &mut self.pointer {
            (p.x, p.y, p.visible) = (pos.Position.x, pos.Position.y, pos.Visible.as_bool());
        }
    }

    /// Where the cursor is drawn on the picture, when it is shown.
    fn cursor_rect(&self) -> Option<Rect> {
        if !self.cursor {
            return None;
        }
        self.pointer.as_ref().filter(|p| p.visible).and_then(|p| p.rect(self.size.0, self.size.1))
    }

    /// The latest picture, with the cursor drawn into it when it is shown.
    fn shot(&mut self) -> Option<Shot<'_>> {
        if self.desktop.is_empty() {
            return None;
        }
        let (w, h) = self.size;
        let stride = w as usize * 4;
        let data = match &self.pointer {
            Some(p) if self.cursor && p.visible => {
                self.shown.clear();
                self.shown.extend_from_slice(&self.desktop);
                draw_pointer(&mut self.shown, stride, w as usize, h as usize, p);
                &self.shown
            }
            _ => &self.desktop,
        };
        Some(Shot { data, stride, width: w, height: h })
    }

    /// Copies the desktop texture into `desktop`.
    fn copy(&mut self, res: &IDXGIResource) -> Result<()> {
        let tex: ID3D11Texture2D = res.cast().map_err(|e| fail("reading the screen", e))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("the display's pictures are in DXGI format {}, not 8-bit BGRA", desc.Format.0)).hint("turn off HDR for this display in Windows Settings > System > Display"));
        }
        let (w, h) = (desc.Width, desc.Height);
        if self.staging.as_ref().is_none_or(|s| (s.1, s.2) != (w, h)) {
            let sd = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&sd, None, Some(&mut t)) }.map_err(|e| fail("reading the screen", e))?;
            self.staging = t.map(|t| (t, w, h));
        }
        let Some((staging, ..)) = &self.staging else {
            return Err(Error::new(ErrorCode::HardwareUnavailable, "the display adapter made no staging texture"));
        };
        unsafe { self.ctx.CopyResource(staging, &tex) };
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)) }.map_err(|e| fail("reading the screen", e))?;
        let (row, pitch) = (w as usize * 4, m.RowPitch as usize);
        self.desktop.resize(row * h as usize, 0);
        for y in 0..h as usize {
            // SAFETY: the mapped texture holds `h` rows of `pitch` bytes, each with `row` bytes of pixels
            let src = unsafe { std::slice::from_raw_parts((m.pData as *const u8).add(y * pitch), row) };
            self.desktop[y * row..(y + 1) * row].copy_from_slice(src);
        }
        unsafe { self.ctx.Unmap(staging, 0) };
        self.size = (w, h);
        Ok(())
    }
}

impl Grabber for Dxgi {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn grab(&mut self) -> Result<Option<Shot<'_>>> {
        // the device's own loop takes whatever is ready, and does not say what changed
        let mut scratch = std::mem::take(&mut self.scratch);
        let fresh = if std::mem::take(&mut self.first_ready) { Ok(true) } else { self.acquire(0, &mut scratch).map(|o| o != Outcome::Nothing) };
        self.scratch = scratch;
        if !fresh? {
            return Ok(None);
        }
        self.whole_pending = false;
        Ok(self.shot())
    }

    fn grab_within(&mut self, wait: Duration, changes: &mut Changes) -> Result<Option<Shot<'_>>> {
        if std::mem::take(&mut self.first_ready) {
            changes.whole = true;
            changes.dirty.clear();
            changes.moved.clear();
            self.whole_pending = false;
            return Ok(self.shot());
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = u32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(u32::MAX);
            if self.acquire(ms, changes)? != Outcome::Nothing {
                break;
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
        self.whole_pending = false;
        Ok(self.shot())
    }
}

/// The dirty rectangles DXGI gave, clipped to the `w` by `h` picture; empty ones are left out.
fn rects_of(raw: &[RECT], w: u32, h: u32, out: &mut Vec<Rect>) {
    out.extend(raw.iter().filter_map(|r| clipped(r.left as i64, r.top as i64, r.right as i64, r.bottom as i64, w, h)));
}

/// The move rectangles DXGI gave, clipped to the picture. Each keeps its source: a clipped destination
/// moves its source by the same amount. False when a source is not on the picture, which the caller
/// takes as an unknown frame.
fn moves_of(raw: &[DXGI_OUTDUPL_MOVE_RECT], w: u32, h: u32, out: &mut Vec<MoveRect>) -> bool {
    for m in raw {
        let d = m.DestinationRect;
        let Some(to) = clipped(d.left as i64, d.top as i64, d.right as i64, d.bottom as i64, w, h) else {
            continue;
        };
        let fx = m.SourcePoint.x as i64 + (to.x as i64 - d.left as i64);
        let fy = m.SourcePoint.y as i64 + (to.y as i64 - d.top as i64);
        if fx < 0 || fy < 0 || fx >= w as i64 || fy >= h as i64 {
            return false;
        }
        out.push(MoveRect { from: (fx as u32, fy as u32), to });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::POINT;

    fn rc(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    fn mv(sx: i32, sy: i32, dest: RECT) -> DXGI_OUTDUPL_MOVE_RECT {
        DXGI_OUTDUPL_MOVE_RECT { SourcePoint: POINT { x: sx, y: sy }, DestinationRect: dest }
    }

    #[test]
    fn dirty_rectangles_are_read_clipped_to_the_picture() {
        // a rectangle reaching past the left and top edges, one wholly right of the picture, one empty
        let raw = [rc(0, 0, 10, 10), rc(-5, -5, 5, 5), rc(90, 90, 120, 120), rc(200, 0, 300, 10), rc(5, 5, 5, 9)];
        let mut out = Vec::new();
        rects_of(&raw, 100, 100, &mut out);
        assert_eq!(out, [Rect { x: 0, y: 0, width: 10, height: 10 }, Rect { x: 0, y: 0, width: 5, height: 5 }, Rect { x: 90, y: 90, width: 10, height: 10 }]);
    }

    #[test]
    fn move_rectangles_keep_their_source_when_clipped() {
        let mut out = Vec::new();
        assert!(moves_of(&[mv(10, 20, rc(30, 40, 50, 60))], 100, 100, &mut out));
        assert_eq!(out, [MoveRect { from: (10, 20), to: Rect { x: 30, y: 40, width: 20, height: 20 } }]);
        // the destination's left edge is 4 off the picture: its source moves with it
        out.clear();
        assert!(moves_of(&[mv(10, 20, rc(-4, 0, 6, 10))], 100, 100, &mut out));
        assert_eq!(out, [MoveRect { from: (14, 20), to: Rect { x: 0, y: 0, width: 6, height: 10 } }]);
        // a source off the picture makes the frame unknown
        assert!(!moves_of(&[mv(-1, 0, rc(0, 0, 5, 5))], 100, 100, &mut out));
        assert!(!moves_of(&[mv(100, 0, rc(0, 0, 5, 5))], 100, 100, &mut out));
        // a destination wholly off the picture is nothing, and known
        out.clear();
        assert!(moves_of(&[mv(0, 0, rc(200, 0, 210, 10))], 100, 100, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn the_cursor_covers_its_picture_clipped_to_the_frame() {
        let p = pointer(2, 2, 3, 8, vec![0; 24], -1, 1);
        assert_eq!(p.rect(10, 10), Some(Rect { x: 0, y: 1, width: 1, height: 3 }));
        // a monochrome cursor's height is twice its picture
        let m = pointer(1, 4, 8, 1, vec![0; 64], 2, 2);
        assert_eq!(m.rect(10, 10), Some(Rect { x: 2, y: 2, width: 4, height: 4 }));
    }

    fn pointer(kind: u32, width: usize, height: usize, pitch: usize, data: Vec<u8>, x: i32, y: i32) -> Pointer {
        Pointer { kind, width, height, pitch, data, x, y, visible: true }
    }

    #[test]
    fn colour_cursors_blend_by_alpha_and_clip_at_the_edges() {
        // a 2x1 cursor: opaque white, then half-transparent white, on black
        let p = pointer(2, 2, 1, 8, vec![255, 255, 255, 255, 255, 255, 255, 128], 1, 0);
        let mut c = vec![0u8; 4 * 4 * 2];
        draw_pointer(&mut c, 16, 4, 2, &p);
        assert_eq!(&c[4..8], &[255, 255, 255, 0]);
        assert_eq!(&c[8..12], &[128, 128, 128, 0]);
        assert_eq!(&c[..4], &[0, 0, 0, 0]);
        // the second row is untouched
        assert!(c[16..].iter().all(|&b| b == 0));
        // partly off the right edge and off the top: only what lies inside is drawn
        let mut c = vec![0u8; 4 * 4 * 2];
        draw_pointer(&mut c, 16, 4, 2, &pointer(2, 2, 1, 8, vec![255; 8], 3, 0));
        assert_eq!(&c[12..16], &[255, 255, 255, 0]);
        draw_pointer(&mut c, 16, 4, 2, &pointer(2, 2, 1, 8, vec![255; 8], 0, -1));
        assert_eq!(c.iter().filter(|&&b| b == 255).count(), 3);
    }

    #[test]
    fn masked_colour_and_monochrome_cursors_replace_or_invert() {
        let screen = [10u8, 20, 30, 255];
        // masked colour: alpha 0 replaces, alpha 255 exclusive-ors
        let p = pointer(4, 2, 1, 8, vec![1, 2, 3, 0, 0xFF, 0xFF, 0xFF, 255], 0, 0);
        let mut c = [screen, screen].concat();
        draw_pointer(&mut c, 8, 2, 1, &p);
        assert_eq!(&c[..4], &[1, 2, 3, 255]);
        assert_eq!(&c[4..], &[10 ^ 0xFF, 20 ^ 0xFF, 30 ^ 0xFF, 255]);
        // monochrome 2x1 (the masks stacked: height 2): AND 0 / XOR 0 gives black, AND 1 / XOR 1 inverts
        let p = pointer(1, 2, 2, 1, vec![0b0100_0000, 0b0100_0000], 0, 0);
        let mut c = [screen, screen].concat();
        draw_pointer(&mut c, 8, 2, 1, &p);
        assert_eq!(&c[..3], &[0, 0, 0]);
        assert_eq!(&c[4..7], &[!10, !20, !30]);
    }

    #[test]
    fn displays_are_listed_when_a_desktop_exists() {
        // a session without a desktop (a service, say) lists none or fails; neither may panic
        if let Ok(l) = list() {
            for d in &l {
                assert!(d.width > 0 && d.height > 0, "{d:?}");
            }
        }
    }

    #[test]
    #[ignore = "needs an interactive desktop"]
    fn the_desktop_is_read_at_the_rate_asked_for() {
        use crate::source::{open_source, Chunk};
        let mut src = open_source("device://screen?fps=20&size=640x360").expect("a screen source");
        // only the stream's header, frame sizes and timing are looked at; no picture is kept or shown
        let (mut got, mut buf) = (0usize, vec![0u8; 1 << 20]);
        let (mut header, mut header_len) = (Vec::new(), 0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            match src.read(&mut buf, Duration::from_millis(200)).expect("bytes") {
                Chunk::Data(n) => {
                    if header_len == 0 {
                        header.extend_from_slice(&buf[..n]);
                        header_len = header.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
                        got = header.len().saturating_sub(header_len);
                    } else {
                        got += n;
                    }
                }
                Chunk::Idle => {}
                Chunk::End | Chunk::Restart => panic!("the screen source ended"),
            }
        }
        let took = start.elapsed().as_secs_f64();
        let header = String::from_utf8_lossy(&header[..header_len]).to_string();
        assert!(header.starts_with("YUV4MPEG2 W") && header.contains("F20:1") && header.contains("C420"), "{header}");
        let tok = |p: char| header.split(' ').find_map(|t| t.strip_prefix(p)).and_then(|v| v.trim().parse::<usize>().ok()).unwrap();
        let (w, h) = (tok('W'), tok('H'));
        assert!(w <= 640 && h <= 360 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
        let frame = 6 + w * h * 3 / 2;
        let fps = (got / frame) as f64 / took;
        println!("screen: {w}x{h}, {} frames in {took:.2} s, {fps:.1} fps", got / frame);
        assert!((14.0..=24.0).contains(&fps), "{fps} fps");
    }
}
