//! Capture devices: cameras and microphones, as `device://video[/NAME]` and
//! `device://audio[/NAME]` sources.
//!
//! A device only gives pictures or samples, so the source writes them as the byte streams the
//! stream demuxers already read: a camera as Y4M (8-bit 4:2:0, converted from the NV12 or YUY2 the
//! device gives), a microphone as WAV with an open-ended `data` chunk. NAME is a device's index
//! from [`devices`] or a part of its name (any case); without it, the first device is used.
//! `size=WxH` and `fps` ask the camera for a mode, the nearest it has being taken.
//!
//! Y4M carries no timestamps, so pictures are timed at the mode's frame rate: a camera that
//! drops frames (in low light, say) runs behind the clock by as many.

// the converters wait for the other platforms' backends
#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod mf;

use crate::source::{SourceInfo, ThreadSource};
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Rational, Result};
use serde::Serialize;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

/// A capture device.
#[derive(Debug, Clone, Serialize)]
pub struct Device {
    /// `video` or `audio`.
    pub kind: &'static str,
    /// Its place among the devices of its kind, as `device://video/0` names it.
    pub index: usize,
    pub name: String,
}

/// What a source asks of a camera.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct VideoWants {
    pub size: Option<(u32, u32)>,
    pub fps: Option<f64>,
}

/// The capture devices present, cameras first.
pub fn devices() -> Result<Vec<Device>> {
    #[cfg(windows)]
    {
        mf::devices()
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

/// Opens `device://video[/NAME]` or `device://audio[/NAME]`.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<ThreadSource> {
    let (kind, name) = match u.target.split_once('/') {
        Some((k, n)) => (k.to_ascii_lowercase(), Some(n).filter(|n| !n.is_empty())),
        None => (u.target.to_ascii_lowercase(), None),
    };
    if kind != "video" && kind != "audio" {
        return Err(Error::invalid_arg(format!("device sources are device://video[/NAME] or device://audio[/NAME], got device://{}", u.target)).alternatives(["device://video", "device://audio"]));
    }
    let wants = VideoWants {
        size: u.option("size").map(parse_size).transpose()?,
        fps: u.option("fps").map(parse_fps).transpose()?,
    };
    let info = SourceInfo { url: shown, kind: "device", live: true, synthetic: false };
    let (tx, rx) = sync_channel(4);
    start(&kind, name.map(str::to_string), wants, tx, &rx)?;
    Ok(ThreadSource::from_channel(info, rx))
}

/// Starts the device's thread, and waits for it to say whether the device opened.
fn start(kind: &str, name: Option<String>, wants: VideoWants, tx: SyncSender<std::io::Result<Vec<u8>>>, rx: &Receiver<std::io::Result<Vec<u8>>>) -> Result<()> {
    #[cfg(windows)]
    {
        let (ok_tx, ok_rx) = sync_channel(1);
        let kind = kind.to_string();
        std::thread::Builder::new()
            .name("ironav-capture".into())
            .spawn(move || mf::run(&kind, name.as_deref(), wants, ok_tx, tx))
            .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start capturing: {e}")))?;
        let _ = rx;
        ok_rx.recv().unwrap_or_else(|_| Err(Error::new(ErrorCode::Internal, "the capture thread stopped before the device opened")))
    }
    #[cfg(not(windows))]
    {
        let _ = (kind, name, wants, tx, rx);
        Err(Error::new(ErrorCode::HardwareUnavailable, "capture devices are not supported on this platform yet").hint("Windows is supported; Linux (V4L2) and macOS are planned"))
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
                    out.extend((0..cw).map(|i| ((a[i * 4 + p] as u16 + b[i * 4 + p] as u16 + 1) / 2) as u8));
                }
            }
        }
    }
    Some(out)
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
    fn a_device_is_chosen_by_index_or_name() {
        let list = [Device { kind: "video", index: 0, name: "Integrated Camera".into() }, Device { kind: "video", index: 1, name: "USB Capture HDMI".into() }];
        assert_eq!(choose(&list, None, "video").unwrap().index, 0);
        assert_eq!(choose(&list, Some("1"), "video").unwrap().index, 1);
        assert_eq!(choose(&list, Some("hdmi"), "video").unwrap().index, 1);
        assert_eq!(choose(&list, Some("2"), "video").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert!(parse_size("1280").is_err());
    }
}
