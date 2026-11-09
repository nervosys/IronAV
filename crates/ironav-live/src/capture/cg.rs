//! Screens on macOS through CoreGraphics: `CGDisplayCreateImage` of the display on every picture,
//! read as 32-bit BGRA. The functions are declared here rather than taken from a binding crate, as
//! there are few of them.
//!
//! macOS shows a program only its own windows, and the desktop picture, until the person allows
//! it under System Settings > Privacy & Security > Screen Recording; without that permission the
//! source fails with an error that says so, rather than sending a picture without the other
//! programs' windows. `CGDisplayCreateImage` leaves the mouse cursor out, and the `cursor` option
//! has no effect here.

use super::screen::{display_name, Grabber, Listed, Shot};
use ironav_core::{Error, ErrorCode, Result};
use std::ffi::c_void;

type DisplayId = u32;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut DisplayId, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> DisplayId;
    fn CGDisplayCopyDisplayMode(display: DisplayId) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut c_void);
    fn CGDisplayCreateImage(display: DisplayId) -> *mut c_void;
    fn CGImageGetWidth(image: *mut c_void) -> usize;
    fn CGImageGetHeight(image: *mut c_void) -> usize;
    fn CGImageGetBytesPerRow(image: *mut c_void) -> usize;
    fn CGImageGetBitsPerPixel(image: *mut c_void) -> usize;
    fn CGImageGetBitmapInfo(image: *mut c_void) -> u32;
    fn CGImageGetDataProvider(image: *mut c_void) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
    fn CGDataProviderCopyData(provider: *mut c_void) -> *mut c_void;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *mut c_void) -> *const u8;
    fn CFDataGetLength(data: *mut c_void) -> isize;
    fn CFRelease(object: *mut c_void);
}

/// `kCGBitmapByteOrder32Little` and its mask: with a skipped or premultiplied alpha first, bytes
/// in memory are blue, green, red, alpha.
const BYTE_ORDER_MASK: u32 = 0x7000;
const BYTE_ORDER_32_LITTLE: u32 = 0x2000;

/// The active displays' ids, the main display first.
fn ids() -> Vec<DisplayId> {
    let mut list = [0 as DisplayId; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(list.len() as u32, list.as_mut_ptr(), &mut n) } != 0 {
        return Vec::new();
    }
    let mut ids: Vec<DisplayId> = list[..(n as usize).min(list.len())].to_vec();
    let main = unsafe { CGMainDisplayID() };
    if let Some(p) = ids.iter().position(|&d| d == main) {
        ids.swap(0, p);
    }
    ids
}

/// A display's size in pixels (a Retina display's, not its size in points).
fn pixels(id: DisplayId) -> (u32, u32) {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            return (0, 0);
        }
        let size = (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32);
        CGDisplayModeRelease(mode);
        size
    }
}

pub(super) fn list() -> Result<Vec<Listed>> {
    Ok(ids()
        .into_iter()
        .enumerate()
        .map(|(i, id)| {
            let (width, height) = pixels(id);
            Listed { name: display_name(&format!("Display {}", i + 1), i), width, height }
        })
        .collect())
}

pub(super) struct Cg {
    id: DisplayId,
    size: (u32, u32),
    pixels: Vec<u8>,
}

fn denied() -> Error {
    Error::new(ErrorCode::PermissionDenied, "this program may not record the screen")
        .hint("allow it in System Settings > Privacy & Security > Screen Recording (the terminal or application that runs ironav), then open the source again")
}

pub(super) fn open(index: usize, _cursor: bool) -> Result<Cg> {
    let ids = ids();
    let Some(&id) = ids.get(index) else {
        return Err(Error::new(ErrorCode::NotFound, format!("no display {index}")).alternatives((0..ids.len()).map(|i| format!("device://screen/{i}"))));
    };
    // asking shows the system's prompt the first time; the permission then takes effect only after
    // the program is restarted, so a refusal here ends the open
    if !unsafe { CGPreflightScreenCaptureAccess() } && !unsafe { CGRequestScreenCaptureAccess() } {
        return Err(denied());
    }
    let size = pixels(id);
    if size.0 == 0 || size.1 == 0 {
        return Err(Error::new(ErrorCode::HardwareUnavailable, format!("display {index} has no size")));
    }
    Ok(Cg { id, size, pixels: Vec::new() })
}

impl Grabber for Cg {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn grab(&mut self) -> Result<Option<Shot<'_>>> {
        unsafe {
            let image = CGDisplayCreateImage(self.id);
            if image.is_null() {
                return Err(Error::new(ErrorCode::HardwareUnavailable, "the display gave no picture").hint("it may have been disconnected or be asleep; open the source again (and check System Settings > Privacy & Security > Screen Recording)"));
            }
            let (w, h, stride) = (CGImageGetWidth(image), CGImageGetHeight(image), CGImageGetBytesPerRow(image));
            let layout_ok = CGImageGetBitsPerPixel(image) == 32 && CGImageGetBitmapInfo(image) & BYTE_ORDER_MASK == BYTE_ORDER_32_LITTLE && stride >= w * 4;
            let data = if layout_ok { CGDataProviderCopyData(CGImageGetDataProvider(image)) } else { std::ptr::null_mut() };
            let got = if data.is_null() {
                false
            } else {
                let len = CFDataGetLength(data).max(0) as usize;
                let ptr = CFDataGetBytePtr(data);
                let ok = !ptr.is_null() && h > 0 && len >= stride * (h - 1) + w * 4;
                if ok {
                    // SAFETY: the data holds `h` rows of `stride` bytes, the last perhaps without padding
                    let src = std::slice::from_raw_parts(ptr, len);
                    let row = w * 4;
                    self.pixels.resize(row * h, 0);
                    for y in 0..h {
                        self.pixels[y * row..(y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
                    }
                }
                CFRelease(data);
                ok
            };
            CGImageRelease(image);
            if !got {
                return Err(Error::new(ErrorCode::UnsupportedFormat, "the display's picture is not in 32-bit BGRA pixels"));
            }
            self.size = (w as u32, h as u32);
            Ok(Some(Shot::new(&self.pixels, w * 4, w as u32, h as u32)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_are_listed_with_sizes() {
        // a runner without a display lists none
        for d in list().unwrap() {
            assert!(d.width > 0 && d.height > 0, "{d:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_setting() {
        let e = denied();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.hint.as_deref().unwrap_or("").contains("Privacy & Security > Screen Recording"));
    }
}
