//! Owner-run check of per-window capture (Windows.Graphics.Capture) on a real window, and its speed.
//!
//! It opens one window, takes up to 120 frames, and prints verdicts and timings: how many frames came,
//! the sizes seen, the time between frames and the time each spent copying (GPU copy, read-back and
//! packing) as median and maximum, and whether 60 frames a second is possible at that size. It also
//! checks that each frame is consistent (its pixel buffer is exactly width x height x 4 bytes, and
//! the sequence counts up by one). It prints, saves and hashes no pixels, and prints no window titles.
//!
//! Windows sends a frame only when the window changes: keep the window changing while this runs
//! (play a video in it, scroll, type, or drag it about). To check the other cases, minimize the
//! window for a moment (the verdict says it was seen minimized and that frames resumed), resize it
//! (the sizes seen list gets longer and the resized count rises), and close it (the run ends with the
//! "closed" verdict).
//!
//! `cargo run --release -p ironav-live --example window_check -- --process notepad`
//! `cargo run --release -p ironav-live --example window_check -- --hwnd 197234 --client --no-cursor`
//!
//! Options: `--hwnd N` (a window handle, decimal or 0x...) or `--process NAME` (the largest visible
//! window of the first process with that executable name, `.exe` optional); `--client` for the client
//! area only; `--no-cursor`; `--border` to keep the system's capture border; `--frames N` (120);
//! `--seconds N` (60), the longest the run waits for them.
//!
//! Exit code 0 when frames came and every frame was consistent, 1 otherwise (and 2 when WGC is not
//! available, so that a caller knows to fall back).

#![cfg_attr(not(windows), allow(unused))]

use std::process::ExitCode;

/// Median and maximum of milliseconds, or None when there are no samples.
fn median_max(v: &mut [f64]) -> Option<(f64, f64)> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some((v[v.len() / 2], v[v.len() - 1]))
}

#[cfg(windows)]
mod imp {
    use super::median_max;
    use ironav_live::capture::{window_capture_supported, WindowCapture, WindowCaptureOptions, WindowState};
    use std::time::{Duration, Instant};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible};

    struct Args {
        hwnd: Option<isize>,
        process: Option<String>,
        frames: usize,
        seconds: u64,
        opts: WindowCaptureOptions,
    }

    fn parse() -> Result<Args, String> {
        let mut a = Args { hwnd: None, process: None, frames: 120, seconds: 60, opts: WindowCaptureOptions::default() };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
            match arg.as_str() {
                "--hwnd" => {
                    let v = value("--hwnd")?;
                    let n = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                        Some(h) => isize::from_str_radix(h, 16),
                        None => v.parse::<isize>(),
                    };
                    a.hwnd = Some(n.map_err(|_| format!("--hwnd must be a number, got '{v}'"))?);
                }
                "--process" => a.process = Some(value("--process")?),
                "--frames" => a.frames = value("--frames")?.parse().map_err(|_| "--frames must be a number".to_string())?,
                "--seconds" => a.seconds = value("--seconds")?.parse().map_err(|_| "--seconds must be a number".to_string())?,
                "--client" => a.opts.client_area = true,
                "--no-cursor" => a.opts.cursor = false,
                "--border" => a.opts.border = true,
                other => return Err(format!("unknown option '{other}'")),
            }
        }
        if a.hwnd.is_none() == a.process.is_none() {
            return Err("give exactly one of --hwnd N or --process NAME".into());
        }
        Ok(a)
    }

    /// The executable's file name (lower case, without `.exe`) of process `pid`.
    fn exe_name(pid: u32) -> Option<String> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            let got = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            got.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let file = path.rsplit(['\\', '/']).next()?.to_lowercase();
            Some(file.strip_suffix(".exe").map(str::to_string).unwrap_or(file))
        }
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            list.push(hwnd);
        }
        BOOL(1)
    }

    /// The largest visible top-level window of a process named `name`, and how many such windows there are.
    fn find_by_process(name: &str) -> Option<(isize, usize)> {
        let want = name.to_lowercase();
        let want = want.strip_suffix(".exe").unwrap_or(&want).to_string();
        let mut all: Vec<HWND> = Vec::new();
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize)) };
        let mut best: Option<(i64, isize)> = None;
        let mut count = 0;
        for h in all {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            if pid == 0 || exe_name(pid).as_deref() != Some(want.as_str()) {
                continue;
            }
            let mut r = RECT::default();
            if unsafe { GetWindowRect(h, &mut r) }.is_err() {
                continue;
            }
            let area = i64::from(r.right - r.left).max(0) * i64::from(r.bottom - r.top).max(0);
            if area == 0 || unsafe { IsIconic(h) }.as_bool() && area < 100_000 {
                continue;
            }
            count += 1;
            if best.is_none_or(|(a, _)| area > a) {
                best = Some((area, h.0 as isize));
            }
        }
        best.map(|(_, h)| (h, count))
    }

    pub fn run() -> Result<u8, String> {
        let args = parse()?;
        // the client area is matched to the picture in physical pixels: be per-monitor DPI aware
        let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let supported = window_capture_supported();
        println!("WGC supported: {}", if supported { "yes" } else { "NO" });
        if !supported {
            return Ok(2);
        }
        let hwnd = match (args.hwnd, &args.process) {
            (Some(h), _) => h,
            (None, Some(p)) => match find_by_process(p) {
                Some((h, n)) => {
                    println!("found {n} visible window(s) of the process; capturing the largest");
                    h
                }
                None => return Err(format!("no visible window of a process named '{p}'")),
            },
            _ => unreachable!(),
        };
        println!("options: cursor={} border={} client_area={}", args.opts.cursor, args.opts.border, args.opts.client_area);
        let opened = Instant::now();
        let mut cap = WindowCapture::open(hwnd, args.opts).map_err(|e| format!("open failed: {e}"))?;
        println!("opened in {:.1} ms; border hidden: {}; state: {:?}", opened.elapsed().as_secs_f64() * 1000.0, cap.border_hidden(), cap.state());

        let (mut intervals, mut copies) = (Vec::new(), Vec::new());
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        let (mut frames, mut inconsistent, mut resized, mut truncated, mut dropped, mut client, mut seq_breaks) = (0usize, 0usize, 0usize, 0usize, 0u64, 0usize, 0usize);
        let (mut saw_minimized, mut frames_after_minimized, mut closed) = (false, 0usize, false);
        let (mut timeouts, mut last_seq, mut last_at) = (0usize, 0u64, None::<Instant>);
        let (mut first_at, mut final_at) = (None::<Instant>, None::<Instant>);
        let (mut time_first, mut time_last) = (Duration::ZERO, Duration::ZERO);
        let deadline = Instant::now() + Duration::from_secs(args.seconds);
        while frames < args.frames && Instant::now() < deadline {
            match cap.next_frame(Duration::from_millis(500)) {
                Ok(Some(f)) => {
                    frames += 1;
                    let ok = f.stride == f.width as usize * 4 && f.bgra.len() == f.stride * f.height as usize && f.width > 0 && f.height > 0;
                    if !ok {
                        inconsistent += 1;
                    }
                    if f.sequence != last_seq + 1 {
                        seq_breaks += 1;
                    }
                    last_seq = f.sequence;
                    if let Some(prev) = last_at {
                        intervals.push(f.at.duration_since(prev).as_secs_f64() * 1000.0);
                    }
                    last_at = Some(f.at);
                    first_at.get_or_insert(f.at);
                    final_at = Some(f.at);
                    if frames == 1 {
                        time_first = f.composed;
                    }
                    time_last = f.composed;
                    copies.push(f.copy.as_secs_f64() * 1000.0);
                    if !sizes.contains(&(f.width, f.height)) && sizes.len() < 12 {
                        sizes.push((f.width, f.height));
                    }
                    resized += f.resized as usize;
                    truncated += f.truncated as usize;
                    dropped += u64::from(f.dropped);
                    client += f.client_only as usize;
                    if saw_minimized {
                        frames_after_minimized += 1;
                    }
                }
                Ok(None) => {
                    timeouts += 1;
                    match cap.state() {
                        WindowState::Minimized | WindowState::Hidden => saw_minimized = true,
                        _ => {}
                    }
                    if timeouts % 4 == 0 && frames == 0 {
                        println!("... no frame yet (state {:?}); change the window's content", cap.state());
                    }
                }
                Err(e) => {
                    closed = cap.state() == WindowState::Closed;
                    println!("capture ended: {}{}", e.message, if closed { " (verdict: closed)" } else { "" });
                    break;
                }
            }
        }

        println!("frames: {frames} (timeouts of 500 ms: {timeouts}, skipped by the pool: {dropped}, sequence breaks: {seq_breaks})");
        println!("sizes seen: {sizes:?}; frames marked resized: {resized}; truncated: {truncated}; client-only: {client}");
        if let Some((m, x)) = median_max(&mut intervals) {
            println!("time between frames: median {m:.2} ms, max {x:.2} ms");
        }
        let copy_stats = median_max(&mut copies);
        if let Some((m, x)) = copy_stats {
            println!("copy (GPU copy + read-back + pack): median {m:.2} ms, max {x:.2} ms");
        }
        if let (Some(a), Some(b)) = (first_at, final_at) {
            let span = b.duration_since(a).as_secs_f64();
            if span > 0.0 && frames > 1 {
                println!("delivery: {:.1} frames per second over {:.1} s (composed-clock span {:.1} s)", (frames - 1) as f64 / span, span, (time_last.saturating_sub(time_first)).as_secs_f64());
            }
        }
        if let (Some((m, _)), Some(&(w, h))) = (copy_stats, sizes.last()) {
            let budget = 1000.0 / 60.0;
            println!("60 fps at {w}x{h}: {} (median copy {m:.2} ms against {budget:.2} ms per frame)", if m < budget * 0.5 { "possible, with room" } else if m < budget { "possible, tight" } else { "NOT possible with this copy" });
        }
        if saw_minimized {
            println!("minimized: seen (state reported Minimized/Hidden), frames after it: {frames_after_minimized}");
        }
        if args.opts.client_area {
            println!("client area: {client} of {frames} frames were cut to it{}", if client == 0 && frames > 0 { " (NOT matched: the whole window was given)" } else { "" });
        }
        println!("closed: {}", if closed { "reported" } else { "no" });
        let pass = frames > 0 && inconsistent == 0 && seq_breaks == 0;
        println!("consistent frames: {}", if inconsistent == 0 { "yes" } else { "NO" });
        println!("verdict: {}", if pass { "PASS" } else { "FAIL" });
        Ok(if pass { 0 } else { 1 })
    }
}

#[cfg(windows)]
fn main() -> ExitCode {
    match imp::run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => ExitCode::from(n),
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
