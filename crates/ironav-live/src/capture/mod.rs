//! Capture devices: cameras and microphones, as `device://video[/NAME]` and
//! `device://audio[/NAME]` sources: Media Foundation on Windows, AVFoundation on macOS,
//! Video4Linux2 and ALSA (through `arecord`) on 64-bit Linux.
//!
//! A device only gives pictures or samples, so the source writes them as the byte streams the
//! stream demuxers already read: a camera as Y4M (8-bit 4:2:0, converted from the NV12 or YUY2 the
//! device gives), a camera that sends only Motion JPEG as live Matroska (each picture a keyframe,
//! timed by the driver's clock), a microphone as WAV with an open-ended `data` chunk. NAME is a device's index
//! from [`devices`] or a part of its name (any case); without it, the first device is used.
//! `size=WxH` and `fps` ask the camera for a mode, the nearest it has being taken.
//!
//! `device://screen[/N]` is a display, as Y4M too (see [`screen`]): DXGI Desktop Duplication on
//! Windows, CoreGraphics on macOS, X11 on Linux.
//!
//! A Rust program that wants the pictures themselves, and what changed in them, opens a
//! [`ScreenCapture`] instead.
//!
//! Y4M carries no timestamps, so pictures are timed at the mode's frame rate: a camera that
//! drops frames (in low light, say) runs behind the clock by as many.

// the converters wait for the other platforms' backends (macOS reads NV12 only)
#![cfg_attr(not(any(windows, all(target_os = "linux", target_pointer_width = "64"))), allow(dead_code))]

#[cfg(target_os = "macos")]
mod avf;
#[cfg(target_os = "macos")]
mod cg;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod mf;
mod screen;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod v4l2;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod x11;

pub use screen::{list_screens, MoveRect, Rect, ScreenCapture, ScreenFrame, ScreenInfo, ScreenOptions};

use crate::source::{SourceInfo, ThreadSource};
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Rational, Result};
use serde::Serialize;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

/// A capture device.
#[derive(Debug, Clone, Serialize)]
pub struct Device {
    /// `video`, `audio` or `screen`.
    pub kind: &'static str,
    /// Its place among the devices of its kind, as `device://video/0` names it.
    pub index: usize,
    pub name: String,
    /// A screen's native size in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl Device {
    pub(crate) fn new(kind: &'static str, index: usize, name: String) -> Device {
        Device { kind, index, name, width: None, height: None }
    }
}

/// What a source asks of a camera.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct VideoWants {
    pub size: Option<(u32, u32)>,
    pub fps: Option<f64>,
}

/// The capture devices present: cameras and microphones, then screens (a screen that cannot be
/// listed, with no X server to ask, say, is left out rather than failing the list).
pub fn devices() -> Result<Vec<Device>> {
    #[cfg(windows)]
    let mut all = mf::devices()?;
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    let mut all = v4l2::devices()?;
    #[cfg(target_os = "macos")]
    let mut all = avf::devices()?;
    #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
    let mut all = Vec::new();
    all.extend(screen::list().into_iter().enumerate().map(|(index, s)| Device { width: Some(s.width), height: Some(s.height), ..Device::new("screen", index, s.name) }));
    Ok(all)
}

/// How long an open device may send nothing before reads fail: a driver can block forever when
/// another program holds the device.
const DEVICE_STALL: std::time::Duration = std::time::Duration::from_secs(5);

/// Opens `device://video[/NAME]` or `device://audio[/NAME]`.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<ThreadSource> {
    let (kind, name) = match u.target.split_once('/') {
        Some((k, n)) => (k.to_ascii_lowercase(), Some(n).filter(|n| !n.is_empty())),
        None => (u.target.to_ascii_lowercase(), None),
    };
    if kind == "screen" {
        return screen::open(u, shown);
    }
    if kind != "video" && kind != "audio" {
        return Err(Error::invalid_arg(format!("device sources are device://video[/NAME], device://audio[/NAME] or device://screen[/N], got device://{}", u.target)).alternatives(["device://video", "device://audio", "device://screen"]));
    }
    let wants = VideoWants {
        size: u.option("size").map(parse_size).transpose()?,
        fps: u.option("fps").map(parse_fps).transpose()?,
    };
    let info = SourceInfo { url: shown, kind: "device", live: true, synthetic: false };
    let (tx, rx) = sync_channel(4);
    start(&kind, name.map(str::to_string), wants, tx, &rx)?;
    Ok(ThreadSource::from_channel(info, rx).stall_after(DEVICE_STALL))
}

/// Starts the device's thread, and waits for it to say whether the device opened.
fn start(kind: &str, name: Option<String>, wants: VideoWants, tx: SyncSender<std::io::Result<Vec<u8>>>, rx: &Receiver<std::io::Result<Vec<u8>>>) -> Result<()> {
    #[cfg(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64")))]
    {
        #[cfg(windows)]
        use mf::run;
        #[cfg(target_os = "macos")]
        use avf::run;
        #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
        use v4l2::run;
        let (ok_tx, ok_rx) = sync_channel(1);
        let kind = kind.to_string();
        std::thread::Builder::new()
            .name("ironav-capture".into())
            .spawn(move || run(&kind, name.as_deref(), wants, ok_tx, tx))
            .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start capturing: {e}")))?;
        let _ = rx;
        ok_rx.recv().unwrap_or_else(|_| Err(Error::new(ErrorCode::Internal, "the capture thread stopped before the device opened")))
    }
    #[cfg(not(any(windows, target_os = "macos", all(target_os = "linux", target_pointer_width = "64"))))]
    {
        let _ = (kind, name, wants, tx, rx);
        Err(Error::new(ErrorCode::HardwareUnavailable, "capture devices are not supported on this platform yet").hint("Windows, macOS and 64-bit Linux are supported"))
    }
}

/// Picks a device by index or by a part of its name.
pub(crate) fn choose<'a>(list: &'a [Device], name: Option<&str>, kind: &str) -> Result<&'a Device> {
    let found = match name {
        None => list.first(),
        Some(n) => match n.parse::<usize>() {
            Ok(i) => list.get(i),
            Err(_) => {
                let n = n.to_lowercase();
                list.iter().find(|d| d.name.to_lowercase() == n).or_else(|| list.iter().find(|d| d.name.to_lowercase().contains(&n)))
            }
        },
    };
    found.ok_or_else(|| {
        let what = name.map_or(String::new(), |n| format!(" matching '{n}'"));
        Error::new(ErrorCode::NotFound, format!("no {kind} capture device{what}")).alternatives(list.iter().map(|d| d.name.clone()))
    })
}

pub(crate) fn parse_size(s: &str) -> Result<(u32, u32)> {
    let bad = || Error::invalid_arg(format!("size must be WIDTHxHEIGHT, got '{s}'"));
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h) = (w.parse::<u32>().map_err(|_| bad())?, h.parse::<u32>().map_err(|_| bad())?);
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(bad());
    }
    Ok((w, h))
}

pub(crate) fn parse_fps(s: &str) -> Result<f64> {
    let f = match s.split_once('/') {
        Some((n, d)) => n.parse::<f64>().ok().zip(d.parse::<f64>().ok()).map(|(n, d)| n / d),
        None => s.parse::<f64>().ok(),
    };
    f.filter(|f| f.is_finite() && *f > 0.0 && *f <= 1000.0).ok_or_else(|| Error::invalid_arg(format!("fps must be a rate such as 30 or 30000/1001, got '{s}'")))
}

/// The Y4M stream header for 8-bit 4:2:0 pictures.
pub(crate) fn y4m_header(w: u32, h: u32, fps: Rational) -> Vec<u8> {
    format!("YUV4MPEG2 W{w} H{h} F{}:{} Ip A1:1 C420jpeg\n", fps.num, fps.den).into_bytes()
}

/// An EBML element with an eight-byte size.
#[cfg_attr(windows, allow(dead_code))] // Media Foundation decodes Motion JPEG itself
fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(id.len() + 8 + body.len());
    v.extend_from_slice(id);
    v.push(0x01);
    v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    v.extend_from_slice(body);
    v
}

/// The start of a live Matroska stream (a Segment of unknown size) holding one Motion JPEG track
/// of `w` by `h`, with times in milliseconds.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn mjpeg_header(w: u32, h: u32, fps: Rational) -> Vec<u8> {
    let uint = |id: &[u8], v: u64| ebml(id, &v.to_be_bytes());
    let mut video = uint(&[0xB0], w as u64);
    video.extend(uint(&[0xBA], h as u64));
    let mut track = [uint(&[0xD7], 1), uint(&[0x73, 0xC5], 1), uint(&[0x83], 1), ebml(&[0x86], b"V_MJPEG")].concat();
    if fps.num > 0 && fps.den > 0 {
        track.extend(uint(&[0x23, 0xE3, 0x83], (1_000_000_000 * fps.den as u64) / fps.num as u64));
    }
    track.extend(ebml(&[0xE0], &video));
    let mut d = ebml(&[0x1A, 0x45, 0xDF, 0xA3], &[ebml(&[0x42, 0x86], &[1]), ebml(&[0x42, 0x82], b"matroska"), ebml(&[0x42, 0x87], &[4]), ebml(&[0x42, 0x85], &[2])].concat());
    d.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    d.extend(ebml(&[0x15, 0x49, 0xA9, 0x66], &[uint(&[0x2A, 0xD7, 0xB1], 1_000_000), ebml(&[0x4D, 0x80], b"ironav")].concat()));
    d.extend(ebml(&[0x16, 0x54, 0xAE, 0x6B], &ebml(&[0xAE], &track)));
    d
}

/// One Motion JPEG picture at `ms` milliseconds, as a Cluster holding a keyframe.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn mjpeg_frame(jpeg: &[u8], ms: u64) -> Vec<u8> {
    let mut block = vec![0x81, 0, 0, 0x80];
    block.extend_from_slice(jpeg);
    ebml(&[0x1F, 0x43, 0xB6, 0x75], &[ebml(&[0xE7], &ms.to_be_bytes()), ebml(&[0xA3], &block)].concat())
}

/// A WAV header for 16-bit PCM whose length is not known.
pub(crate) fn wav_header(rate: u32, channels: u16) -> Vec<u8> {
    let align = channels * 2;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * align as u32).to_le_bytes());
    h.extend_from_slice(&align.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// The layouts a camera's pictures come in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// Planar Y, U, V (I420).
    I420,
    /// Planar Y, then interleaved U and V.
    Nv12,
    /// Y0 U Y1 V, packed.
    Yuy2,
}

/// One Y4M frame (`FRAME\n` and 4:2:0 planes) of the top-left `w` by `h` (both even) of a
/// picture `rows` high whose rows are `stride` bytes apart (of the luma plane; packed YUY2 rows
/// hold both). `None` when `src` is too short for it, or the stride too narrow.
pub(crate) fn y4m_frame(src: &[u8], layout: Layout, w: usize, h: usize, stride: usize, rows: usize) -> Option<Vec<u8>> {
    let (cw, ch) = (w / 2, h / 2);
    if w < 2 || h < 2 || rows < h || stride < if layout == Layout::Yuy2 { 2 * w } else { w } {
        return None;
    }
    let mut out = Vec::with_capacity(6 + w * h + 2 * cw * ch);
    out.extend_from_slice(b"FRAME\n");
    match layout {
        Layout::I420 => {
            // the chroma planes follow the whole picture, at half its stride and rows (rounded up)
            let (cs, crows) = (stride.div_ceil(2), rows.div_ceil(2));
            let (u, v) = (stride * rows, stride * rows + cs * crows);
            if src.len() < v + cs * (ch - 1) + cw {
                return None;
            }
            for r in 0..h {
                out.extend_from_slice(&src[r * stride..r * stride + w]);
            }
            for base in [u, v] {
                for r in 0..ch {
                    out.extend_from_slice(&src[base + r * cs..base + r * cs + cw]);
                }
            }
        }
        Layout::Nv12 => {
            if src.len() < stride * rows + stride * (ch - 1) + w {
                return None;
            }
            for r in 0..h {
                out.extend_from_slice(&src[r * stride..r * stride + w]);
            }
            let uv = &src[stride * rows..];
            for p in 0..2 {
                for r in 0..ch {
                    out.extend(uv[r * stride..r * stride + w].iter().skip(p).step_by(2));
                }
            }
        }
        Layout::Yuy2 => {
            if src.len() < stride * (h - 1) + w * 2 {
                return None;
            }
            for r in 0..h {
                out.extend(src[r * stride..r * stride + w * 2].iter().step_by(2));
            }
            // chroma from each pair of rows, averaged
            for p in [1, 3] {
                for r in 0..ch {
                    let (a, b) = (&src[2 * r * stride..], &src[(2 * r + 1) * stride..]);
                    out.extend((0..cw).map(|i| ((a[i * 4 + p] as u16 + b[i * 4 + p] as u16).div_ceil(2)) as u8));
                }
            }
        }
    }
    Some(out)
}

/// One Y4M frame of the top-left `w` by `h` (both even) of an NV12 picture held as two separate
/// planes: luma rows `ys` bytes apart, and interleaved chroma rows `uvs` bytes apart (AVFoundation
/// gives the planes apart, each with its own stride). `None` when a plane is too short or a stride
/// too narrow.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn y4m_frame_nv12_planes(y: &[u8], ys: usize, uv: &[u8], uvs: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    let ch = h / 2;
    if w < 2 || h < 2 || ys < w || uvs < w || y.len() < ys * (h - 1) + w || uv.len() < uvs * (ch - 1) + w {
        return None;
    }
    let mut out = Vec::with_capacity(6 + w * h + w * ch);
    out.extend_from_slice(b"FRAME\n");
    for r in 0..h {
        out.extend_from_slice(&y[r * ys..r * ys + w]);
    }
    for p in 0..2 {
        for r in 0..ch {
            out.extend(uv[r * uvs..r * uvs + w].iter().skip(p).step_by(2));
        }
    }
    Some(out)
}

/// The camera mode nearest what is asked, of `modes` (width, height, lowest and highest frame
/// rate): its index, and the rate to set when one was asked for (that rate, or the nearest the mode
/// has). Size counts before rate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pick_mode(modes: &[(u32, u32, f64, f64)], wants: VideoWants) -> Option<(usize, Option<f64>)> {
    let mut best: Option<(f64, f64, usize, Option<f64>)> = None;
    for (i, &(w, h, lo, hi)) in modes.iter().enumerate() {
        if !lo.is_finite() || !hi.is_finite() {
            continue;
        }
        let ds = wants.size.map_or(0.0, |(ww, wh)| (w as f64 - ww as f64).abs() + (h as f64 - wh as f64).abs());
        let rate = wants.fps.map(|f| f.clamp(lo.min(hi), hi.max(lo)));
        let df = wants.fps.zip(rate).map_or(0.0, |(f, r)| (f - r).abs());
        if best.is_none_or(|b| (ds, df) < (b.0, b.1)) {
            best = Some((ds, df, i, rate));
        }
    }
    best.map(|b| (b.2, b.3))
}

/// The sample types a microphone's buffers may hold (little-endian, interleaved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum Samples {
    S16,
    S32,
    F32,
}

/// Appends `src` as 16-bit little-endian samples to `out`; a trailing partial sample is left out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pcm_to_s16le(src: &[u8], kind: Samples, out: &mut Vec<u8>) {
    match kind {
        Samples::S16 => out.extend_from_slice(&src[..src.len() & !1]),
        Samples::S32 => out.extend(src.as_chunks::<4>().0.iter().flat_map(|c| ((i32::from_le_bytes(*c) >> 16) as i16).to_le_bytes())),
        Samples::F32 => out.extend(src.as_chunks::<4>().0.iter().flat_map(|c| ((f32::from_le_bytes(*c).clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())),
    }
}

/// Sends a piece of the byte stream; false once the source has been closed.
pub(crate) fn send(tx: &SyncSender<std::io::Result<Vec<u8>>>, b: Vec<u8>) -> bool {
    tx.send(Ok(b)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planes(f: &[u8]) -> &[u8] {
        assert_eq!(&f[..6], b"FRAME\n");
        &f[6..]
    }

    #[test]
    fn camera_layouts_become_planar_420() {
        // 4x2, with rows 6 bytes apart for the planar layouts
        let (w, h, stride) = (4, 2, 6);
        let mut nv12 = vec![0u8; stride * h + stride];
        for r in 0..h {
            for c in 0..w {
                nv12[r * stride + c] = (10 * r + c) as u8;
            }
        }
        nv12[stride * h..stride * h + 4].copy_from_slice(&[100, 200, 101, 201]);
        let f = y4m_frame(&nv12, Layout::Nv12, w, h, stride, h).unwrap();
        assert_eq!(planes(&f), &[0, 1, 2, 3, 10, 11, 12, 13, 100, 101, 200, 201]);

        let mut i420 = vec![0u8; stride * h + 2 * 3];
        i420[..stride * h].copy_from_slice(&nv12[..stride * h]);
        i420[stride * h..].copy_from_slice(&[100, 101, 9, 200, 201, 9]);
        let f = y4m_frame(&i420, Layout::I420, w, h, stride, h).unwrap();
        assert_eq!(planes(&f), &[0, 1, 2, 3, 10, 11, 12, 13, 100, 101, 200, 201]);
        // the last row's padding may be left out, but nothing the picture needs
        assert!(y4m_frame(&i420[..i420.len() - 1], Layout::I420, w, h, stride, h).is_some());
        assert!(y4m_frame(&i420[..i420.len() - 2], Layout::I420, w, h, stride, h).is_none());
        // an odd height: the chroma follows all three rows, and the last row is left out
        let mut odd = vec![0u8; stride * 3 + 2 * 3 * 2];
        odd[stride * 3..stride * 3 + 2].copy_from_slice(&[100, 101]);
        odd[stride * 3 + 6..stride * 3 + 8].copy_from_slice(&[200, 201]);
        let f = y4m_frame(&odd, Layout::I420, w, h, stride, 3).unwrap();
        assert_eq!(&planes(&f)[8..], &[100, 101, 200, 201]);
        assert!(y4m_frame(&i420, Layout::I420, w, h, 3, h).is_none(), "a stride narrower than the picture");

        // YUY2: Y0 U Y1 V; chroma averaged over the two rows
        let yuy2 = [0, 100, 1, 200, 2, 102, 3, 202, 10, 102, 11, 202, 12, 104, 13, 204];
        let f = y4m_frame(&yuy2, Layout::Yuy2, w, h, 8, h).unwrap();
        assert_eq!(planes(&f), &[0, 1, 2, 3, 10, 11, 12, 13, 101, 103, 201, 203]);

        assert!(y4m_frame(&yuy2[..10], Layout::Yuy2, w, h, 8, h).is_none());
        assert!(y4m_frame(&nv12[..10], Layout::Nv12, w, h, stride, h).is_none());
    }

    #[test]
    fn nv12_planes_with_their_own_strides_become_planar_420() {
        // 4x2: luma rows 6 bytes apart, one chroma row 8 bytes apart
        let y = [0, 1, 2, 3, 9, 9, 10, 11, 12, 13, 9, 9];
        let uv = [100, 200, 101, 201, 9, 9, 9, 9];
        let f = y4m_frame_nv12_planes(&y, 6, &uv, 8, 4, 2).unwrap();
        assert_eq!(planes(&f), &[0, 1, 2, 3, 10, 11, 12, 13, 100, 101, 200, 201]);
        // the last row's padding may be left out, but nothing the picture needs
        assert!(y4m_frame_nv12_planes(&y[..10], 6, &uv, 8, 4, 2).is_some());
        assert!(y4m_frame_nv12_planes(&y[..9], 6, &uv, 8, 4, 2).is_none());
        assert!(y4m_frame_nv12_planes(&y, 6, &uv[..3], 8, 4, 2).is_none());
        assert!(y4m_frame_nv12_planes(&y, 3, &uv, 8, 4, 2).is_none(), "a stride narrower than the picture");
        assert!(y4m_frame_nv12_planes(&y, 6, &uv, 8, 0, 2).is_none());
    }

    #[test]
    fn the_nearest_camera_mode_is_picked() {
        let modes = [(640, 480, 1.0, 30.0), (1280, 720, 1.0, 30.0), (1920, 1080, 1.0, 60.0), (1920, 1080, 1.0, 15.0)];
        let w = |size, fps| VideoWants { size, fps };
        assert_eq!(pick_mode(&modes, w(Some((1280, 720)), None)), Some((1, None)));
        assert_eq!(pick_mode(&modes, w(Some((1280, 700)), Some(24.0))), Some((1, Some(24.0))));
        // the size first, then the rate: 1080p at 60 is the mode that has it
        assert_eq!(pick_mode(&modes, w(Some((1920, 1080)), Some(60.0))), Some((2, Some(60.0))));
        assert_eq!(pick_mode(&modes, w(Some((1920, 1080)), Some(10.0))), Some((2, Some(10.0))));
        // no mode has 120 fps: the fastest the best size has
        assert_eq!(pick_mode(&modes, w(Some((1920, 1080)), Some(120.0))), Some((2, Some(60.0))));
        assert_eq!(pick_mode(&modes, w(None, Some(45.0))), Some((2, Some(45.0))));
        assert_eq!(pick_mode(&[], VideoWants::default()), None);
        assert_eq!(pick_mode(&[(1, 1, f64::NAN, 1.0)], VideoWants::default()), None);
    }

    #[test]
    fn microphone_samples_become_16_bit() {
        let mut o = Vec::new();
        pcm_to_s16le(&[1, 0, 0xFF, 0x7F, 5], Samples::S16, &mut o);
        assert_eq!(o, [1, 0, 0xFF, 0x7F]);
        o.clear();
        pcm_to_s16le(&[0, 0, 0x34, 0x12, 0, 0, 0, 0x80, 7], Samples::S32, &mut o);
        assert_eq!(o, [0x34, 0x12, 0x00, 0x80]);
        o.clear();
        let f: Vec<u8> = [0.0f32, 1.0, -1.0, 0.5, 2.0, -3.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        pcm_to_s16le(&f, Samples::F32, &mut o);
        let s: Vec<i16> = o.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(s, [0, 32767, -32767, 16384, 32767, -32767]);
    }

    #[test]
    fn the_byte_streams_are_read_as_y4m_and_wav() {
        let mut y = y4m_header(4, 2, Rational::new(30, 1));
        y.extend(y4m_frame(&[0; 12], Layout::I420, 4, 2, 4, 2).unwrap());
        let mut d = ironav_formats::stream::stream_demuxer(&y, &Default::default()).unwrap();
        d.feed(&y).unwrap();
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.packet.data.len(), 12);
        assert_eq!(d.streams()[0].codec, ironav_core::CodecId::RawVideo);

        let mut a = wav_header(48000, 2);
        a.extend(vec![0u8; 4 * 48000]);
        let mut d = ironav_formats::stream::stream_demuxer(&a, &Default::default()).unwrap();
        d.feed(&a).unwrap();
        assert!(d.next_packet().unwrap().is_some());
        assert_eq!(d.streams()[0].codec, ironav_core::CodecId::PcmS16Le);
    }

    #[test]
    fn motion_jpeg_is_read_as_matroska() {
        let mut d = mjpeg_header(64, 48, Rational::new(30, 1));
        let pics: Vec<Vec<u8>> = (0..5u8).map(|i| [&[0xFF, 0xD8][..], &[i; 40], &[0xFF, 0xD9]].concat()).collect();
        for (i, p) in pics.iter().enumerate() {
            d.extend(mjpeg_frame(p, 1000 + 33 * i as u64));
        }
        let mut s = ironav_formats::stream::stream_demuxer(&d, &Default::default()).unwrap();
        s.feed(&d).unwrap();
        let mut got = Vec::new();
        while let Some(p) = s.next_packet().unwrap() {
            got.push(p);
        }
        let st = &s.streams()[0];
        assert_eq!(st.codec, ironav_core::CodecId::Mjpeg);
        assert!(matches!(st.params, ironav_core::StreamParams::Video { width: 64, height: 48, .. }), "{:?}", st.params);
        assert_eq!(got.len(), 5);
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p.packet.data, pics[i]);
            assert!(p.key);
            assert_eq!(p.packet.pts * st.time_base.num * 1000 / st.time_base.den, 1000 + 33 * i as i64);
        }
    }

    #[test]
    fn a_device_is_chosen_by_index_or_name() {
        let list = [Device::new("video", 0, "Integrated Camera".into()), Device::new("video", 1, "USB Capture HDMI".into())];
        assert_eq!(choose(&list, None, "video").unwrap().index, 0);
        assert_eq!(choose(&list, Some("1"), "video").unwrap().index, 1);
        assert_eq!(choose(&list, Some("hdmi"), "video").unwrap().index, 1);
        assert_eq!(choose(&list, Some("2"), "video").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert!(parse_size("1280").is_err());
    }
}
