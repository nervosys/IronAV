//! Owner-run check of region screen capture on a real display, and its speed.
//!
//! For each test rectangle it opens the primary display, takes a region frame and then a full frame
//! of the same display, cuts the rectangle out of the full frame and compares the two row by row,
//! byte by byte. It also checks that the region frame's origin and size are the rectangle clamped to
//! the display. A rectangle that lies wholly off the display must give an empty frame. A pair that
//! differs is taken again (the screen may have changed between the two captures) up to three times.
//! Finally it times 60 whole-display calls and 60 region calls of `next_frame(50 ms)`.
//!
//! It prints the rectangles, the origins and sizes it got, the verdicts, and counts of differing bytes
//! and rows. It prints, saves and hashes no screen content. Keep the screen still during the checks; for
//! the timing, move a window or type so that frames keep coming.
//!
//! Exit code 0 when every rectangle matched (or was correctly empty), 1 otherwise.
//!
//! `cargo run --release -p ironav-live --example region_check`

#![cfg_attr(not(windows), allow(unused))]

use ironav_core::{Error, ErrorCode};
use ironav_live::capture::{Rect, ScreenCapture, ScreenFrame, ScreenOptions};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// How long one call waits for the display to change, and how many calls a picture may take.
const WAIT: Duration = Duration::from_millis(500);
const TRIES: usize = 20;
/// Attempts at one rectangle's pair of captures before its verdict is final.
const ATTEMPTS: usize = 3;
const TIMING_CALLS: usize = 60;

/// A rectangle as (x, y, width, height).
type Span = (u32, u32, u32, u32);

/// A picture copied out of the capture, with its stride, so that it outlives the next call.
struct Picture {
    width: u32,
    height: u32,
    stride: usize,
    origin: (u32, u32),
    display: (u32, u32),
    bgra: Vec<u8>,
}

impl Picture {
    fn of(f: &ScreenFrame<'_>) -> Picture {
        Picture { width: f.width, height: f.height, stride: f.stride, origin: f.origin, display: f.display, bgra: f.bgra.to_vec() }
    }

    /// The `w` pixels of row `y` from column `x` of the picture, or None when the picture does not hold them.
    fn row(&self, x: u32, y: u32, w: u32) -> Option<&[u8]> {
        let start = (y as usize).checked_mul(self.stride)?.checked_add(x as usize * 4)?;
        self.bgra.get(start..start + w as usize * 4)
    }

    /// The pixels of a rectangle of the picture, rows packed together.
    fn cut(&self, (x, y, w, h): Span) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for j in 0..h {
            out.extend_from_slice(self.row(x, y + j, w)?);
        }
        Some(out)
    }
}

/// Takes the next picture, waiting up to `wait` per call and making at most `tries` calls.
fn pick(cap: &mut ScreenCapture, wait: Duration, tries: usize) -> Result<Option<Picture>, Error> {
    for _ in 0..tries {
        if let Some(f) = cap.next_frame(wait)? {
            return Ok(Some(Picture::of(&f)));
        }
    }
    Ok(None)
}

/// The part of a `display` that `r` covers, as the capture clips it; None when none of it does.
fn clamp(r: Rect, display: (u32, u32)) -> Option<Span> {
    if r.width == 0 || r.height == 0 || r.x >= display.0 || r.y >= display.1 {
        return None;
    }
    let right = (r.x as u64 + r.width as u64).min(display.0 as u64);
    let bottom = (r.y as u64 + r.height as u64).min(display.1 as u64);
    Some((r.x, r.y, (right - r.x as u64) as u32, (bottom - r.y as u64) as u32))
}

/// Compares a region picture with the same rectangle of a full picture: the bytes and rows that differ,
/// or None when a picture does not hold what its size says.
fn compare(a: &Picture, f: &Picture, (x, y, w, h): Span) -> Option<(usize, usize)> {
    let (mut bytes, mut rows) = (0, 0);
    for j in 0..h {
        let (ra, rf) = (a.row(0, j, w)?, f.row(x, y + j, w)?);
        let d = ra.iter().zip(rf).filter(|(p, q)| p != q).count();
        bytes += d;
        if d > 0 {
            rows += 1;
        }
    }
    Some((bytes, rows))
}

/// Checks one rectangle that lies on the display. Returns whether it passed, and the text after the rectangle.
fn check_on_display(cap: &mut ScreenCapture, rect: Rect, display: (u32, u32), want: Span) -> Result<(bool, String), Error> {
    let (x, y, w, h) = want;
    let mut prev_cut: Option<Vec<u8>> = None;
    let mut screen_changed = false;
    let mut last = (0, 0);
    let mut got = String::from("no picture");
    for attempt in 1..=ATTEMPTS {
        cap.set_region(Some(rect));
        let Some(a) = pick(cap, WAIT, TRIES)? else {
            return Ok((false, format!("{got}: MISMATCH (no region frame)")));
        };
        got = format!("origin ({}, {}) size {}x{}", a.origin.0, a.origin.1, a.width, a.height);
        cap.set_region(None);
        let Some(f) = pick(cap, WAIT, TRIES)? else {
            return Ok((false, format!("{got}: MISMATCH (no full frame)")));
        };
        if (f.width, f.height) != display || f.display != display || f.origin != (0, 0) {
            return Ok((false, format!("{got}: MISMATCH (the full frame is not the display)")));
        }
        let Some(cut) = f.cut((x, y, w, h)) else {
            return Ok((false, format!("{got}: MISMATCH (the full frame is too short)")));
        };
        // the same rectangle of the display changed between two full frames: the screen was moving
        screen_changed |= prev_cut.as_ref().is_some_and(|p| *p != cut);
        prev_cut = Some(cut);
        if a.display != display || a.origin != (x, y) || (a.width, a.height) != (w, h) {
            return Ok((false, format!("{got}: MISMATCH (origin or size, want origin ({x}, {y}) size {w}x{h})")));
        }
        match compare(&a, &f, (x, y, w, h)) {
            None => return Ok((false, format!("{got}: MISMATCH (the region picture is too short)"))),
            Some((0, _)) => {
                let again = if attempt == 1 { String::new() } else { format!(" (attempt {attempt})") };
                return Ok((true, format!("{got}: match{again}")));
            }
            Some(counts) => last = counts,
        }
    }
    let verdict = if screen_changed {
        "screen kept changing".to_string()
    } else {
        format!("MISMATCH ({} bytes differ in {} rows)", last.0, last.1)
    };
    Ok((false, format!("{got}: {verdict}")))
}

/// Checks one rectangle that lies wholly off the display: it must give an empty frame.
fn check_off_display(cap: &mut ScreenCapture, rect: Rect, display: (u32, u32)) -> Result<(bool, String), Error> {
    cap.set_region(Some(rect));
    let Some(a) = pick(cap, WAIT, TRIES)? else {
        return Ok((false, "MISMATCH (no frame)".to_string()));
    };
    if (a.width, a.height) == (0, 0) && a.bgra.is_empty() && a.display == display {
        Ok((true, "empty (off the display, as expected)".to_string()))
    } else {
        Ok((false, format!("MISMATCH (expected empty, got {}x{})", a.width, a.height)))
    }
}

/// Times `TIMING_CALLS` calls of `next_frame(50 ms)` for `region` and prints the frame count and the median and max
/// duration of the calls that gave a frame.
fn time_calls(cap: &mut ScreenCapture, label: &str, region: Option<Rect>) -> Result<(), Error> {
    cap.set_region(region);
    let mut ms = Vec::new();
    for _ in 0..TIMING_CALLS {
        let t = Instant::now();
        if cap.next_frame(Duration::from_millis(50))?.is_some() {
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    ms.sort_by(f64::total_cmp);
    let n = ms.len();
    if n == 0 {
        println!("{label}: 0 of {TIMING_CALLS} calls gave a frame");
        return Ok(());
    }
    let median = if n % 2 == 1 { ms[n / 2] } else { (ms[n / 2 - 1] + ms[n / 2]) / 2.0 };
    println!("{label}: {n} of {TIMING_CALLS} calls gave a frame, median {median:.2} ms, max {:.2} ms", ms[n - 1]);
    Ok(())
}

fn run() -> Result<bool, Error> {
    println!("region_check: compares region frames with the same pixels of full frames. It prints no screen content.");
    println!("Keep the screen still during the checks. During the timing test, move a window or type so frames keep coming.");

    let mut options = ScreenOptions::default();
    options.cursor = false;
    let mut cap = ScreenCapture::open(0, options)?;
    let full = pick(&mut cap, WAIT, TRIES)?.ok_or_else(|| Error::new(ErrorCode::HardwareUnavailable, "the display gave no full frame"))?;
    let display = (full.width, full.height);
    if full.origin != (0, 0) || full.display != display {
        return Err(Error::new(ErrorCode::Internal, "the full frame is not the whole display"));
    }
    println!("display {}x{}", display.0, display.1);

    let (w, h) = display;
    let mut rects = vec![
        Rect::new(0, 0, 64, 64),
        Rect::new(w.saturating_sub(64), h.saturating_sub(64), 64, 64),
        Rect::new(w / 4, h / 4, w / 2, h / 2),
        Rect::new(1, 1, 333, 217),
        Rect::new(w.saturating_sub(100), 10, 300, 50),
        Rect::new(17, h / 3, 1520, 1200),
    ];
    // six pseudo-random rectangles from a fixed-seed LCG, some of them off the display
    let mut seed: u64 = 0x5EED_1A2B;
    let mut rand = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as u32
    };
    for _ in 0..6 {
        let x = rand() % (w + w / 4).max(1);
        let y = rand() % (h + h / 4).max(1);
        let rw = 1 + rand() % w.max(1);
        let rh = 1 + rand() % h.max(1);
        rects.push(Rect::new(x, y, rw, rh));
    }

    let mut all_ok = true;
    println!("checks (rectangle as x, y, width x height):");
    for (i, rect) in rects.iter().enumerate() {
        let (r, want) = (*rect, clamp(*rect, display));
        let (ok, text) = match want {
            Some(span) => check_on_display(&mut cap, r, display, span)?,
            None => check_off_display(&mut cap, r, display)?,
        };
        all_ok &= ok;
        println!("{:>2}  ({}, {}) {}x{}  {}", i + 1, r.x, r.y, r.width, r.height, text);
    }

    println!("timing, {TIMING_CALLS} calls of next_frame(50 ms) each:");
    time_calls(&mut cap, "whole display", None)?;
    time_calls(&mut cap, "region (17, H/3, 1520x1200)", Some(Rect::new(17, h / 3, 1520, 1200)))?;

    println!("{}", if all_ok { "all rectangles matched" } else { "some rectangles FAILED" });
    Ok(all_ok)
}

#[cfg(windows)]
fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    println!("Windows only");
    ExitCode::FAILURE
}
