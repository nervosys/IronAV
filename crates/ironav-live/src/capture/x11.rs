//! Screens on Linux through X11: `XGetImage` on the root window, with libX11 (and, when present,
//! libXinerama for the list of monitors and libXfixes for the cursor) loaded at run time, so
//! nothing is needed to build or to run on a machine without X.
//!
//! The monitors are those Xinerama reports (the whole screen as one display when it does not
//! answer). `XGetImage` copies through the X connection rather than shared memory, which keeps this
//! simple and is fast enough for a screen scaled down to 1080p at 30 frames per second.
//!
//! Wayland compositors do not let a program read the screen over X (an X client only sees other
//! X clients' windows), so a session with only Wayland is told so rather than given a black picture.

use super::screen::{display_name, Grabber, Listed, Shot};
use ironav_core::{Error, ErrorCode, Result};
use std::ffi::{c_char, c_int, c_short, c_uint, c_ulong, c_ushort, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};

/// `ZPixmap`
const Z_PIXMAP: c_int = 2;
const ALL_PLANES: c_ulong = !0;

/// The prefix of Xlib's `XImage` that is read here.
#[repr(C)]
struct XImage {
    width: c_int,
    height: c_int,
    xoffset: c_int,
    format: c_int,
    data: *mut c_char,
    byte_order: c_int,
    bitmap_unit: c_int,
    bitmap_bit_order: c_int,
    bitmap_pad: c_int,
    depth: c_int,
    bytes_per_line: c_int,
    bits_per_pixel: c_int,
    red_mask: c_ulong,
    green_mask: c_ulong,
    blue_mask: c_ulong,
}

#[repr(C)]
struct XineramaScreenInfo {
    screen_number: c_int,
    x_org: c_short,
    y_org: c_short,
    width: c_short,
    height: c_short,
}

#[repr(C)]
struct XFixesCursorImage {
    x: c_short,
    y: c_short,
    width: c_ushort,
    height: c_ushort,
    xhot: c_ushort,
    yhot: c_ushort,
    cursor_serial: c_ulong,
    pixels: *mut c_ulong,
    atom: c_ulong,
    name: *const c_char,
}

type Display = c_void;

/// An Xlib error (a window that went away, say) must not end the process, as Xlib's own handler
/// would; it is noted here and the call that caused it fails.
static X_ERROR: AtomicBool = AtomicBool::new(false);

extern "C" fn on_error(_: *mut Display, _: *mut c_void) -> c_int {
    X_ERROR.store(true, Ordering::SeqCst);
    0
}

/// The functions used from the libraries.
struct Lib {
    open_display: unsafe extern "C" fn(*const c_char) -> *mut Display,
    close_display: unsafe extern "C" fn(*mut Display) -> c_int,
    default_root: unsafe extern "C" fn(*mut Display) -> c_ulong,
    default_screen: unsafe extern "C" fn(*mut Display) -> c_int,
    display_width: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    display_height: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    get_image: unsafe extern "C" fn(*mut Display, c_ulong, c_int, c_int, c_uint, c_uint, c_ulong, c_int) -> *mut XImage,
    destroy_image: unsafe extern "C" fn(*mut XImage) -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
    sync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    set_error_handler: unsafe extern "C" fn(Option<extern "C" fn(*mut Display, *mut c_void) -> c_int>) -> *mut c_void,
    xinerama_active: Option<unsafe extern "C" fn(*mut Display) -> c_int>,
    xinerama_query: Option<unsafe extern "C" fn(*mut Display, *mut c_int) -> *mut XineramaScreenInfo>,
    fixes_cursor: Option<unsafe extern "C" fn(*mut Display) -> *mut XFixesCursorImage>,
}

unsafe fn library(names: &[&str]) -> Option<*mut c_void> {
    names.iter().find_map(|n| {
        let n = CString::new(*n).ok()?;
        let h = libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        (!h.is_null()).then_some(h)
    })
}

unsafe fn symbol<T: Copy>(lib: *mut c_void, name: &str) -> Option<T> {
    let n = CString::new(name).ok()?;
    let p = libc::dlsym(lib, n.as_ptr());
    // SAFETY: T is a function pointer type, the same size as the pointer dlsym gives
    (!p.is_null()).then(|| std::mem::transmute_copy::<*mut c_void, T>(&p))
}

impl Lib {
    fn load() -> Option<Lib> {
        unsafe {
            let x = library(&["libX11.so.6", "libX11.so"])?;
            let xin = library(&["libXinerama.so.1", "libXinerama.so"]);
            let xfixes = library(&["libXfixes.so.3", "libXfixes.so"]);
            Some(Lib {
                open_display: symbol(x, "XOpenDisplay")?,
                close_display: symbol(x, "XCloseDisplay")?,
                default_root: symbol(x, "XDefaultRootWindow")?,
                default_screen: symbol(x, "XDefaultScreen")?,
                display_width: symbol(x, "XDisplayWidth")?,
                display_height: symbol(x, "XDisplayHeight")?,
                get_image: symbol(x, "XGetImage")?,
                destroy_image: symbol(x, "XDestroyImage")?,
                free: symbol(x, "XFree")?,
                sync: symbol(x, "XSync")?,
                set_error_handler: symbol(x, "XSetErrorHandler")?,
                xinerama_active: xin.and_then(|l| symbol(l, "XineramaIsActive")),
                xinerama_query: xin.and_then(|l| symbol(l, "XineramaQueryScreens")),
                fixes_cursor: xfixes.and_then(|l| symbol(l, "XFixesGetCursorImage")),
            })
        }
    }
}

/// Why no display can be reached: no libX11, no X server, or only Wayland.
fn unreachable_display(lib_missing: bool) -> Error {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    if wayland {
        Error::new(ErrorCode::HardwareUnavailable, "this session is Wayland, and no X server is reachable: capturing a Wayland screen needs the PipeWire desktop portal, which is not supported yet")
            .hint("run the program under an X11 session, or with XWayland where only X clients are to be captured")
    } else if lib_missing {
        Error::new(ErrorCode::HardwareUnavailable, "screen capture needs libX11, which could not be loaded").hint("install the X11 client library (libx11-6)")
    } else {
        Error::new(ErrorCode::HardwareUnavailable, "could not connect to an X server").hint("set DISPLAY to the X server to capture (and XAUTHORITY if it asks for one)")
    }
}

/// A monitor's rectangle on the root window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Monitor {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// An open connection, closed when dropped.
struct Conn {
    lib: Lib,
    dpy: *mut Display,
}

impl Conn {
    fn open() -> Result<Conn> {
        let lib = Lib::load().ok_or_else(|| unreachable_display(true))?;
        let dpy = unsafe { (lib.open_display)(std::ptr::null()) };
        if dpy.is_null() {
            return Err(unreachable_display(false));
        }
        unsafe { (lib.set_error_handler)(Some(on_error)) };
        Ok(Conn { lib, dpy })
    }

    /// The monitors, the first being the primary: Xinerama's list, or the whole screen.
    fn monitors(&self) -> Vec<Monitor> {
        unsafe {
            if let (Some(active), Some(query)) = (self.lib.xinerama_active, self.lib.xinerama_query) {
                if active(self.dpy) != 0 {
                    let mut n: c_int = 0;
                    let p = query(self.dpy, &mut n);
                    if !p.is_null() {
                        let all: Vec<Monitor> = std::slice::from_raw_parts(p, n.max(0) as usize).iter().map(|s| Monitor { x: s.x_org as i32, y: s.y_org as i32, w: s.width.max(0) as u32, h: s.height.max(0) as u32 }).collect();
                        (self.lib.free)(p.cast());
                        if !all.is_empty() {
                            return all;
                        }
                    }
                }
            }
            let screen = (self.lib.default_screen)(self.dpy);
            vec![Monitor { x: 0, y: 0, w: (self.lib.display_width)(self.dpy, screen).max(0) as u32, h: (self.lib.display_height)(self.dpy, screen).max(0) as u32 }]
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe { (self.lib.close_display)(self.dpy) };
    }
}

pub(super) fn list() -> Result<Vec<Listed>> {
    let c = Conn::open()?;
    Ok(c.monitors().into_iter().enumerate().map(|(i, m)| Listed { name: display_name(&format!("X11 display {i}"), i), width: m.w, height: m.h }).collect())
}

pub(super) struct X11 {
    conn: Conn,
    root: c_ulong,
    monitor: Monitor,
    cursor: bool,
    /// The pixels of the latest picture, blue first, with the cursor drawn.
    pixels: Vec<u8>,
}

pub(super) fn open(index: usize, cursor: bool) -> Result<X11> {
    let conn = Conn::open()?;
    let monitors = conn.monitors();
    let Some(&monitor) = monitors.get(index) else {
        return Err(Error::new(ErrorCode::NotFound, format!("no display {index}")).alternatives((0..monitors.len()).map(|i| format!("device://screen/{i}"))));
    };
    if monitor.w == 0 || monitor.h == 0 {
        return Err(Error::new(ErrorCode::HardwareUnavailable, format!("display {index} has no size")));
    }
    let root = unsafe { (conn.lib.default_root)(conn.dpy) };
    Ok(X11 { conn, root, monitor, cursor, pixels: Vec::new() })
}

impl X11 {
    /// Draws the cursor, if X says where it is, into the picture.
    fn draw_cursor(&mut self) {
        let Some(get) = self.conn.lib.fixes_cursor else { return };
        unsafe {
            let c = get(self.conn.dpy);
            if c.is_null() {
                return;
            }
            let c = &*c;
            let (w, h) = (self.monitor.w as i64, self.monitor.h as i64);
            // the position is that of the hot spot; the picture's corner is at the hot spot less its offset
            let (ox, oy) = (c.x as i64 - c.xhot as i64 - self.monitor.x as i64, c.y as i64 - c.yhot as i64 - self.monitor.y as i64);
            if !c.pixels.is_null() {
                let px = std::slice::from_raw_parts(c.pixels, c.width as usize * c.height as usize);
                for cy in 0..c.height as i64 {
                    for cx in 0..c.width as i64 {
                        let (x, y) = (ox + cx, oy + cy);
                        if x < 0 || y < 0 || x >= w || y >= h {
                            continue;
                        }
                        // premultiplied ARGB
                        let p = px[(cy * c.width as i64 + cx) as usize] as u32;
                        let (a, r, g, b) = (p >> 24, (p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
                        let d = &mut self.pixels[(y * w + x) as usize * 4..][..4];
                        for (i, s) in [b, g, r].into_iter().enumerate() {
                            d[i] = (s + d[i] as u32 * (255 - a) / 255).min(255) as u8;
                        }
                    }
                }
            }
            (self.conn.lib.free)((c as *const XFixesCursorImage).cast_mut().cast());
        }
    }
}

impl Grabber for X11 {
    fn size(&self) -> (u32, u32) {
        (self.monitor.w, self.monitor.h)
    }

    fn grab(&mut self) -> Result<Option<Shot<'_>>> {
        let (w, h) = (self.monitor.w, self.monitor.h);
        X_ERROR.store(false, Ordering::SeqCst);
        let lib = &self.conn.lib;
        let img = unsafe { (lib.get_image)(self.conn.dpy, self.root, self.monitor.x, self.monitor.y, w, h, ALL_PLANES, Z_PIXMAP) };
        unsafe { (lib.sync)(self.conn.dpy, 0) };
        if img.is_null() || X_ERROR.load(Ordering::SeqCst) {
            if !img.is_null() {
                unsafe { (lib.destroy_image)(img) };
            }
            return Err(Error::new(ErrorCode::HardwareUnavailable, "the X server would not give the screen's picture").hint("the display may have changed size or gone away; open the source again"));
        }
        let ok = {
            let i = unsafe { &*img };
            // 32 bits per pixel with blue in the lowest byte, which is what X servers give for their
            // usual 24-bit depth
            let ok = i.bits_per_pixel == 32 && i.byte_order == 0 && i.red_mask == 0xFF0000 && i.green_mask == 0xFF00 && i.blue_mask == 0xFF && i.width as u32 == w && i.height as u32 == h && i.bytes_per_line >= i.width * 4;
            if ok {
                let (row, stride) = (w as usize * 4, i.bytes_per_line as usize);
                // SAFETY: the image holds `h` rows of `bytes_per_line` bytes
                let src = unsafe { std::slice::from_raw_parts(i.data as *const u8, stride * h as usize) };
                self.pixels.resize(row * h as usize, 0);
                for y in 0..h as usize {
                    self.pixels[y * row..(y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
                }
            }
            ok
        };
        unsafe { (self.conn.lib.destroy_image)(img) };
        if !ok {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "the X server's screen is not in 32-bit RGB pixels (a 24-bit depth with 32 bits per pixel is expected)"));
        }
        if self.cursor {
            self.draw_cursor();
        }
        Ok(Some(Shot::new(&self.pixels, w as usize * 4, w, h)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_without_x_says_why() {
        let e = unreachable_display(true);
        assert_eq!(e.code, ErrorCode::HardwareUnavailable);
        assert!(e.message.contains("libX11") || e.message.contains("Wayland"), "{}", e.message);
    }

    #[test]
    fn listing_without_a_display_is_an_error_not_a_panic() {
        // with no X server this fails; with one it lists monitors with sizes
        match list() {
            Ok(l) => assert!(l.iter().all(|d| d.width > 0 && d.height > 0)),
            Err(e) => assert_eq!(e.code, ErrorCode::HardwareUnavailable),
        }
    }
}
