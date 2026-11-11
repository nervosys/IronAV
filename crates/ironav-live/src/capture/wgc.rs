//! One window on Windows through Windows.Graphics.Capture (WGC): a `GraphicsCaptureItem` made for the
//! window's handle, a free-threaded `Direct3D11CaptureFramePool` and a `GraphicsCaptureSession`. WGC
//! gives the window's own pixels wherever it is (under other windows, partly off the display, while
//! it is dragged), copied by the compositor; each is copied to a staging texture, mapped, and handed
//! over as tightly packed 32-bit BGRA.
//!
//! Threads and lifetimes. The capture is used on the thread that opened it (it is neither `Send` nor
//! `Sync`): that thread joins the Windows Runtime's multithreaded apartment for as long as the capture
//! lives, and makes every Direct3D call. Windows calls two callbacks on threads of its own, `FrameArrived`
//! and the item's `Closed`; they own only an `Arc<Signal>` (a mutex and a condition variable) and touch
//! nothing else, so they stay harmless if one is still running when the capture is dropped. The Direct3D
//! device is made multithread-protected because Windows' threads use it too.

use super::window::{client_crop, pack_rows, plan_frame, mapped_len, Crop, Signal, WindowCaptureOptions, WindowFrame, WindowState};
use ironav_core::{Error, ErrorCode, Result};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsIconic, IsWindow, IsWindowVisible};

/// The longest `next_frame` waits, whatever it is asked.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// Frames the pool holds. Two is the least WGC allows; the newest is taken and older ones dropped.
const BUFFERS: i32 = 2;

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

fn fail(what: &str, e: windows::core::Error) -> Error {
    let code = e.code();
    let err = Error::new(ErrorCode::HardwareUnavailable, format!("{what}: {}", e.message()));
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        err.hint("the graphics device was lost (a driver reset, or the adapter was removed); open the capture again")
    } else {
        err
    }
}

fn unsupported() -> Error {
    Error::new(ErrorCode::HardwareUnavailable, "Windows.Graphics.Capture is not available on this system").hint("per-window capture needs Windows 10 version 1903 or later and an interactive desktop session; fall back to capturing the display (ScreenCapture) and cutting the window out of it")
}

fn closed() -> Error {
    Error::new(ErrorCode::NotFound, "the window was closed").hint("open a new capture if the window comes back")
}

/// Whether per-window capture can be used on this system. Fall back to [`ScreenCapture`](super::ScreenCapture)
/// when it cannot.
pub fn window_capture_supported() -> bool {
    // the class is activated through the Windows Runtime, which this thread must have joined
    let _apartment = Apartment::enter();
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// This thread's membership of the Windows Runtime's multithreaded apartment, for as long as it is held.
///
/// Windows Runtime classes (the capture classes among them) cannot be activated on a thread that has not
/// joined an apartment. A thread that is already in a single-threaded apartment keeps it (`RoInitialize`
/// answers `RPC_E_CHANGED_MODE`): the capture classes are agile and work from there too. Only a
/// successful `RoInitialize` is balanced by `RoUninitialize`, on the same thread: the guard is not `Send`.
struct Apartment {
    joined: bool,
    _not_send: PhantomData<*const ()>,
}

/// Keeps the process's multithreaded apartment alive for good, once.
///
/// Without it the apartment ends when the last thread that joined it leaves, and the Windows Runtime
/// factories this module has already activated (windows-rs keeps them for the life of the process)
/// would then belong to an apartment that is gone: the next capture would call into them. With it, a
/// thread can join and leave as often as it likes.
fn keep_mta_alive() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // the cookie is never given back: the apartment stays until the process ends
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

impl Apartment {
    fn enter() -> Apartment {
        keep_mta_alive();
        // S_OK and S_FALSE (already in this mode) both count one more join
        // RPC_E_CHANGED_MODE (an STA thread) and any other failure join nothing, and leave nothing to undo
        let joined = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
        Apartment { joined, _not_send: PhantomData }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.joined {
            unsafe { RoUninitialize() };
        }
    }
}

/// A frame taken from the pool, returned to it (`Close`) when dropped, on every path.
struct Held(Direct3D11CaptureFrame);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

/// A mapped staging texture, unmapped when dropped.
struct Mapped<'a> {
    ctx: &'a ID3D11DeviceContext,
    tex: &'a ID3D11Texture2D,
    data: *const u8,
    pitch: usize,
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe { self.ctx.Unmap(self.tex, 0) };
    }
}

/// What reading one frame found.
struct Taken {
    width: u32,
    height: u32,
    content: (u32, u32),
    truncated: bool,
    dropped: u32,
    client_only: bool,
    composed: Duration,
    at: Instant,
    copy: Duration,
}

/// An open window, read a picture at a time. Opening it starts Windows' capture of that window; open
/// only a window the person has agreed to share.
///
/// What the capture gives is the window's own pixels (its frame and title bar included unless
/// [`WindowCaptureOptions::client_area`] asks for the client area), wherever the window is, whatever
/// covers it. Windows sends a picture when the window's content changes, not at a steady rate.
///
/// The capture is used on the thread that opened it, which joins the Windows Runtime's multithreaded
/// apartment until the capture is dropped (a thread that is already in a single-threaded apartment
/// stays in it). It is neither `Send` nor `Sync`. This type has no `Debug` output.
#[non_exhaustive]
pub struct WindowCapture {
    hwnd: HWND,
    opts: WindowCaptureOptions,
    signal: Arc<Signal>,
    // Direct3D, shared with Windows' capture threads (multithread-protected)
    ctx: ID3D11DeviceContext,
    device: ID3D11Device,
    winrt_device: IDirect3DDevice,
    // the capture itself
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: Option<GraphicsCaptureSession>,
    arrived_token: Option<i64>,
    closed_token: Option<i64>,
    /// The size the pool was last made or recreated with.
    pool_size: (u32, u32),
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    packed: Vec<u8>,
    /// The size of the last frame given.
    last_size: Option<(u32, u32)>,
    sequence: u64,
    border_hidden: bool,
    _not_send: PhantomData<*const ()>,
    // last: the apartment is left after everything above has been released
    _apartment: Apartment,
}

impl WindowCapture {
    /// Starts capturing the window `hwnd` (a window handle, as an integer).
    ///
    /// Fails with `NotFound` when there is no such window, with `HardwareUnavailable` when
    /// Windows.Graphics.Capture is not available (see [`window_capture_supported`]) or Windows refuses
    /// to capture that window, and with `InvalidArgument` for a null handle. The window need not be
    /// visible: a minimized window gives no frames until it is restored (see [`WindowCapture::state`]).
    pub fn open(hwnd: isize, opts: WindowCaptureOptions) -> Result<WindowCapture> {
        WindowCapture::open_traced(hwnd, opts, |_| {})
    }

    /// [`WindowCapture::open`] that names each step to `trace` just before taking it, so that a program
    /// that dies inside a Windows call can say which one (the last name printed). The names are fixed
    /// words, never anything about the window.
    #[doc(hidden)]
    pub fn open_traced(hwnd: isize, opts: WindowCaptureOptions, mut trace: impl FnMut(&str)) -> Result<WindowCapture> {
        trace("validate the window handle");
        if hwnd == 0 {
            return Err(Error::invalid_arg("the window handle is null"));
        }
        let hwnd = HWND(hwnd as *mut c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(Error::new(ErrorCode::NotFound, "there is no window with that handle"));
        }
        trace("roinit");
        let apartment = Apartment::enter();
        trace("IsSupported");
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(unsupported());
        }
        trace("d3d device");
        let (device, ctx) = create_device()?;
        trace("winrt device");
        let winrt_device = {
            let dxgi: IDXGIDevice = device.cast().map_err(|e| fail("could not use the Direct3D device", e))?;
            let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| fail("could not share the Direct3D device with Windows.Graphics.Capture", e))?;
            inspectable.cast::<IDirect3DDevice>().map_err(|e| fail("could not share the Direct3D device with Windows.Graphics.Capture", e))?
        };
        trace("item interop factory");
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|_| unsupported())?;
        trace("item (CreateForWindow)");
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(|e| {
            Error::new(ErrorCode::HardwareUnavailable, format!("Windows will not capture that window: {}", e.message())).hint("a window that is not a top-level application window, or that belongs to the system's shell, cannot be captured")
        })?;
        trace("item size");
        let size = item.Size().map_err(|e| fail("reading the window's size", e))?;
        let pool_size = (size.Width.max(1) as u32, size.Height.max(1) as u32);
        trace("pool");
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, FORMAT, BUFFERS, SizeInt32 { Width: pool_size.0 as i32, Height: pool_size.1 as i32 }).map_err(|e| fail("could not make the frame pool", e))?;
        // from here the struct owns everything made so far, and its Drop undoes whatever has been set up
        let mut cap = WindowCapture {
            hwnd,
            opts,
            signal: Arc::new(Signal::default()),
            ctx,
            device,
            winrt_device,
            item,
            pool,
            session: None,
            arrived_token: None,
            closed_token: None,
            pool_size,
            staging: None,
            packed: Vec::new(),
            last_size: None,
            sequence: 0,
            border_hidden: false,
            _not_send: PhantomData,
            _apartment: apartment,
        };
        cap.start(&mut trace)?;
        Ok(cap)
    }

    fn start(&mut self, trace: &mut impl FnMut(&str)) -> Result<()> {
        trace("frame handler");
        let sig = self.signal.clone();
        let on_frame = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
            sig.frame_arrived();
            Ok(())
        });
        self.arrived_token = Some(self.pool.FrameArrived(&on_frame).map_err(|e| fail("could not listen for frames", e))?);
        trace("closed handler");
        let sig = self.signal.clone();
        let on_closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            sig.close();
            Ok(())
        });
        self.closed_token = Some(self.item.Closed(&on_closed).map_err(|e| fail("could not listen for the window closing", e))?);
        trace("session");
        let session = self.pool.CreateCaptureSession(&self.item).map_err(|e| fail("could not start a capture session", e))?;
        // The cursor property needs Windows 10 2004. Leaving the cursor in is the default, so a failure
        // to say so is harmless; a failure to leave it out is not, and fails the open.
        trace("session cursor");
        let cursor = session.SetIsCursorCaptureEnabled(self.opts.cursor);
        if let (false, Err(e)) = (self.opts.cursor, cursor) {
            return Err(Error::new(ErrorCode::HardwareUnavailable, format!("could not leave the cursor out: {}", e.message())).hint("turning the cursor off needs Windows 10 version 2004 or later"));
        }
        // The border can be taken off on Windows 11 (and some Windows 10 builds) when the system allows
        // it; otherwise this fails, and the capture goes on with the border.
        trace("session border");
        self.border_hidden = !self.opts.border && session.SetIsBorderRequired(false).is_ok();
        trace("start");
        session.StartCapture().map_err(|e| fail("could not start capturing the window", e))?;
        self.session = Some(session);
        Ok(())
    }

    /// The window's next picture. Waits up to `timeout` (at most an hour) for the window to change,
    /// and returns `Ok(None)` when it did not (or cannot: see [`WindowCapture::state`]). Every frame
    /// returned is new; if several were ready, the newest is given and the others counted in
    /// [`WindowFrame::dropped`].
    ///
    /// The frame has the size of the window's picture, which is given with every frame: when the
    /// window is resized the frame pool is recreated at the new size, as WGC requires, and
    /// [`WindowFrame::resized`] is set. A frame is always whole and consistent; the one frame made
    /// while a window is growing may hold only part of it ([`WindowFrame::truncated`]).
    ///
    /// Fails with `NotFound` ("the window was closed") when the window has gone.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<WindowFrame<'_>>> {
        let deadline = Instant::now() + timeout.min(MAX_WAIT);
        loop {
            self.check_open()?;
            // forget old arrivals before looking, so that one that comes after the look is not missed
            self.signal.clear_arrived();
            if let Some(t) = self.take()? {
                let size = (t.width, t.height);
                let resized = self.last_size != Some(size);
                self.last_size = Some(size);
                self.sequence += 1;
                return Ok(Some(WindowFrame {
                    width: t.width,
                    height: t.height,
                    stride: t.width as usize * 4,
                    bgra: &self.packed,
                    sequence: self.sequence,
                    at: t.at,
                    composed: t.composed,
                    content: t.content,
                    resized,
                    truncated: t.truncated,
                    dropped: t.dropped,
                    client_only: t.client_only,
                    copy: t.copy,
                }));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.signal.wait(left);
        }
    }

    /// What the window is doing. While it is `Minimized` or `Hidden`, Windows sends no frames.
    pub fn state(&self) -> WindowState {
        if self.signal.is_closed() || !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            WindowState::Closed
        } else if unsafe { IsIconic(self.hwnd) }.as_bool() {
            WindowState::Minimized
        } else if !unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
            WindowState::Hidden
        } else {
            WindowState::Running
        }
    }

    /// The size of the last frame given, if there was one.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.last_size
    }

    /// The window's size as Windows reports it now (the window with its frame, not the client area).
    pub fn window_size(&self) -> Option<(u32, u32)> {
        let s = self.item.Size().ok()?;
        Some((s.Width.max(0) as u32, s.Height.max(0) as u32))
    }

    /// Whether the system accepted the request to leave out the border it draws around a captured window.
    /// False when the options kept the border, or when Windows refused to remove it.
    pub fn border_hidden(&self) -> bool {
        self.border_hidden
    }

    fn check_open(&self) -> Result<()> {
        if self.signal.is_closed() || !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            self.signal.close();
            return Err(closed());
        }
        Ok(())
    }

    /// Takes the newest frame the pool has, and reads it into `packed`. `None` when the pool has none
    /// (or the one it had held nothing of the window).
    fn take(&mut self) -> Result<Option<Taken>> {
        let mut latest: Option<Held> = None;
        let mut dropped = 0u32;
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(f) => {
                    // the older frame goes back to the pool as it is replaced
                    if latest.replace(Held(f)).is_some() {
                        dropped += 1;
                    }
                }
                // a null frame: none is ready (the error carries no code)
                Err(e) if e.code().is_ok() => break,
                Err(e) => {
                    self.check_open()?;
                    return Err(fail("reading the window", e));
                }
            }
        }
        let Some(held) = latest else { return Ok(None) };
        let started = Instant::now();
        let content = held.0.ContentSize().map_err(|e| fail("reading the window's size", e))?;
        let content = (content.Width.max(0) as u32, content.Height.max(0) as u32);
        let composed = held.0.SystemRelativeTime().map_or(Duration::ZERO, |t| Duration::from_nanos(t.Duration.max(0) as u64 * 100));
        let tex = frame_texture(&held.0)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("the window's pictures are in DXGI format {}, not 8-bit BGRA", desc.Format.0)));
        }
        let client = if self.opts.client_area && content.0 > 0 && content.1 > 0 { self.client_area(content) } else { None };
        let plan = plan_frame(content, (desc.Width, desc.Height), self.pool_size, client);
        let read = match plan.region {
            Some(r) => {
                self.copy_region(&tex, r)?;
                Some((r.w, r.h))
            }
            None => None,
        };
        let copy = started.elapsed();
        let at = Instant::now();
        // give the frame back before the pool is recreated
        drop(held);
        if let Some((w, h)) = plan.recreate {
            self.pool.Recreate(&self.winrt_device, FORMAT, BUFFERS, SizeInt32 { Width: w as i32, Height: h as i32 }).map_err(|e| fail("following the window's new size", e))?;
            self.pool_size = (w, h);
        }
        Ok(read.map(|(width, height)| Taken { width, height, content, truncated: plan.truncated, dropped, client_only: client.is_some(), composed, at, copy }))
    }

    /// Copies `r` of the frame's texture through the staging texture into `packed`.
    fn copy_region(&mut self, src: &ID3D11Texture2D, r: Crop) -> Result<()> {
        if self.staging.as_ref().is_none_or(|s| (s.1, s.2) != (r.w, r.h)) {
            // released first, so that two do not exist at once while the window is resized
            self.staging = None;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: r.w,
                Height: r.h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| fail("could not make a texture to read the window with", e))?;
            let tex = tex.ok_or_else(|| Error::new(ErrorCode::HardwareUnavailable, "the graphics adapter made no texture"))?;
            self.staging = Some((tex, r.w, r.h));
        }
        let Some((staging, ..)) = &self.staging else { unreachable!("the staging texture was just made") };
        let region = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
        unsafe { self.ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, src, 0, Some(&region)) };
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)) }.map_err(|e| fail("reading the window", e))?;
        let mapped = Mapped { ctx: &self.ctx, tex: staging, data: m.pData as *const u8, pitch: m.RowPitch as usize };
        let (w, h) = (r.w as usize, r.h as usize);
        let Some(len) = mapped_len(mapped.pitch, w * 4, h).filter(|_| !mapped.data.is_null()) else {
            return Err(Error::new(ErrorCode::Internal, "the graphics adapter mapped a texture with an impossible row pitch"));
        };
        // SAFETY: Map succeeded, so `data` points to the staging texture's `RowPitch * Height` bytes, which
        // stay mapped (and are not written by the GPU) until `mapped` is dropped; `len` is at most that.
        let src = unsafe { std::slice::from_raw_parts(mapped.data, len) };
        if !pack_rows(src, mapped.pitch, w, h, &mut self.packed) {
            return Err(Error::new(ErrorCode::Internal, "the mapped texture was shorter than its size says"));
        }
        Ok(())
    }

    /// Where the client area lies in the window's `content`-sized picture, if Windows' numbers can be matched up.
    fn client_area(&self, content: (u32, u32)) -> Option<Crop> {
        let mut frame = RECT::default();
        unsafe { DwmGetWindowAttribute(self.hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut frame as *mut RECT as *mut c_void, std::mem::size_of::<RECT>() as u32) }.ok()?;
        let mut client = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut client) }.ok()?;
        let mut origin = POINT { x: 0, y: 0 };
        if !unsafe { ClientToScreen(self.hwnd, &mut origin) }.as_bool() {
            return None;
        }
        client_crop(content, ((frame.left, frame.top), (frame.right - frame.left, frame.bottom - frame.top)), ((origin.x, origin.y), (client.right - client.left, client.bottom - client.top)))
    }
}

impl Drop for WindowCapture {
    /// Stops the callbacks, then the session, then the pool, then lets the GPU finish with what was queued.
    /// The item, the devices and the staging texture are released after, in the order of the fields, and the
    /// apartment last. A callback that is still running on one of Windows' threads holds only the `Signal`,
    /// which its own reference keeps alive.
    fn drop(&mut self) {
        if let Some(t) = self.arrived_token.take() {
            let _ = self.pool.RemoveFrameArrived(t);
        }
        if let Some(t) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(t);
        }
        if let Some(s) = self.session.take() {
            let _ = s.Close();
        }
        let _ = self.pool.Close();
        self.staging = None;
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
    }
}

/// The Direct3D texture behind a captured frame.
fn frame_texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
    let surface = frame.Surface().map_err(|e| fail("reading the window", e))?;
    let access = surface.cast::<IDirect3DDxgiInterfaceAccess>().map_err(|e| fail("reading the window", e))?;
    unsafe { access.GetInterface::<ID3D11Texture2D>() }.map_err(|e| fail("reading the window", e))
}

/// A Direct3D 11 device with BGRA support, on the default adapter or, failing that, in software.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let make = |driver: D3D_DRIVER_TYPE| {
        let (mut device, mut ctx) = (None, None);
        unsafe { D3D11CreateDevice(None::<&IDXGIAdapter>, driver, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx)) }.map(|()| (device, ctx))
    };
    let (device, ctx) = match make(D3D_DRIVER_TYPE_HARDWARE) {
        Ok(p) => p,
        Err(_) => make(D3D_DRIVER_TYPE_WARP).map_err(|e| fail("could not make a Direct3D 11 device", e))?,
    };
    let (Some(device), Some(ctx)) = (device, ctx) else {
        return Err(Error::new(ErrorCode::HardwareUnavailable, "the graphics adapter gave no Direct3D device"));
    };
    // Windows' capture threads use this device and its context while this one does: the context must lock
    let mt = device.cast::<ID3D11Multithread>().map_err(|e| fail("could not make the Direct3D device safe to share", e))?;
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok((device, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The capture must stay on its thread (its apartment, and the Direct3D context, belong to it): this
    // does not compile if `WindowCapture` ever becomes `Send` or `Sync`.
    trait AmbiguousIfSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    trait AmbiguousIfSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

    #[test]
    fn the_capture_is_neither_send_nor_sync() {
        <WindowCapture as AmbiguousIfSend<_>>::check();
        <WindowCapture as AmbiguousIfSync<_>>::check();
    }

    #[test]
    fn a_null_handle_is_refused_without_capturing_anything() {
        assert_eq!(WindowCapture::open(0, WindowCaptureOptions::default()).err().map(|e| e.code), Some(ErrorCode::InvalidArgument));
    }
}
