//! Helpers the FFmpeg comparison tests share: which FFmpeg is on PATH, and how its decode of a file
//! relates to what IronAV keeps of it when that FFmpeg predates a change in how it reads the file.

#![allow(dead_code)]

use ironav_formats::open_demuxer;
use std::path::Path;
use std::process::Command;

/// The `(major, minor)` of the `ffmpeg` on PATH (`n9.0.2`, `8.0.1-1ubuntu1` and `9.0.2-essentials_build`
/// all parse); `None` for a version it cannot read, such as a git build's.
pub fn ffmpeg_version() -> Option<(u32, u32)> {
    let out = Command::new("ffmpeg").arg("-version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let v = text.split_whitespace().nth(2)?.trim_start_matches('n');
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    Some((major, parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)))
}

/// Whether the FFmpeg on PATH is `major.minor` or later. A version it cannot parse counts as new.
pub fn ffmpeg_at_least(major: u32, minor: u32) -> bool {
    ffmpeg_version().is_none_or(|v| v >= (major, minor))
}

/// The part of FFmpeg's decode `want` (interleaved samples of a `channels`-channel stream) that is
/// comparable with what IronAV decoded of `file` (`got_len` interleaved samples), for a `format` FFmpeg
/// writes ("matroska" or "mp4"). A current FFmpeg decodes what the file's edit keeps, as IronAV does, and
/// `want` is returned whole. An older one differs in a way that is exact for the file:
/// * Matroska before FFmpeg 8.1 (matroskadec commit 0880458e4c) does not trim the track's CodecDelay, so
///   its decode starts with `lead_in` more frames than IronAV's.
/// * MP4 before FFmpeg 9.0 (mov commits da04251772 and 68b53a8dbf) ends an edit at a packet boundary: it
///   keeps every packet up to the first one that reaches the end of the edit, in whole. Its decode, from
///   the edit's start, ends at the end of that packet.
///
/// Both are asserted exactly, and the remaining samples are the ones that must match.
pub fn comparable<'a>(file: &Path, format: &str, channels: usize, got_len: usize, want: &'a [f32]) -> &'a [f32] {
    if format == "matroska" && !ffmpeg_at_least(8, 1) {
        let (lead_in, _, _) = edit(file);
        let lead = lead_in as usize * channels;
        assert_eq!(want.len(), got_len + lead, "an FFmpeg before 8.1 decodes the CodecDelay of {lead_in} frames too");
        return &want[lead..];
    }
    if format == "mp4" && !ffmpeg_at_least(9, 0) {
        let (lead_in, duration, packets) = edit(file);
        let end = lead_in.saturating_add(duration.unwrap_or(i64::MAX));
        // whole packets: IronAV's last packet is already cut to the edit's end, so each counts at the
        // full length (AC-3 and E-AC-3 packets are all the same length)
        let full = packets.iter().copied().max().unwrap_or(0);
        let mut at = 0i64;
        for _ in &packets {
            at += full;
            if at >= end {
                break;
            }
        }
        let frames = (at - lead_in) as usize;
        assert_eq!(want.len(), frames * channels, "an FFmpeg before 9.0 keeps the packet in which the edit ends, whole");
        assert!(want.len() >= got_len);
        return &want[..got_len];
    }
    want
}

/// What the file declares of its audio stream: the lead-in and the duration after it (in frames), and the
/// duration of each packet.
fn edit(file: &Path) -> (i64, Option<i64>, Vec<i64>) {
    let mut d = open_demuxer(file).expect("the file opens");
    let s = d.streams()[0].clone();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        packets.push(p.duration);
    }
    (s.lead_in, s.duration, packets)
}
