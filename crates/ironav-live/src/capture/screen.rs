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
}

/// What a backend gives: the display's latest picture.
pub(crate) trait Grabber {
    /// The display's size in pixels.
    fn size(&self) -> (u32, u32);
    /// The latest picture, or `None` when the display has not changed since the last call (or has
    /// shown nothing yet).
    fn grab(&mut self) -> Result<Option<Shot<'_>>>;
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

/// The displays present; none when the platform cannot say.
pub(crate) fn list() -> Vec<Listed> {
    #[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
    {
        imp::list().unwrap_or_default()
    }
    #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
    {
        Vec::new()
    }
}

/// Opens `device://screen[/N]`.
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

    fn shot(px: &[[u8; 4]], w: u32, h: u32) -> Vec<u8> {
        assert_eq!(px.len(), (w * h) as usize);
        px.iter().flatten().copied().collect()
    }

    fn convert(px: &[[u8; 4]], w: u32, h: u32, dw: usize, dh: usize) -> Vec<u8> {
        let d = shot(px, w, h);
        let f = bgra_to_y4m(&Shot { data: &d, stride: w as usize * 4, width: w, height: h }, dw, dh).unwrap();
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
        let f = bgra_to_y4m(&Shot { data: &padded, stride: 12, width: 2, height: 2 }, 2, 2).unwrap();
        assert_eq!(&f[6..], &out[..]);
        // too short, or a stride narrower than the picture
        d.truncate(11);
        assert!(bgra_to_y4m(&Shot { data: &d, stride: 8, width: 2, height: 2 }, 2, 2).is_none());
        assert!(bgra_to_y4m(&Shot { data: &padded, stride: 4, width: 2, height: 2 }, 2, 2).is_none());
        assert!(bgra_to_y4m(&Shot { data: &padded, stride: 12, width: 2, height: 2 }, 3, 2).is_none(), "an odd size");
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
            y.extend(bgra_to_y4m(&Shot { data: &d, stride: 32, width: 8, height: 6 }, 8, 6).unwrap());
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
            Ok(if std::mem::replace(&mut self.sent, true) { None } else { Some(Shot { data: &self.data, stride: 16, width: 4, height: 2 }) })
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
}
