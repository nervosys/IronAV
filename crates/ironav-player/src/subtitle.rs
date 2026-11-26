//! A subtitle timeline independent of the audio and video demuxers. A cue can still be active
//! after the video keyframe a seek starts from, so keep its original start and end for lookup.

use ironav_core::{Error, ErrorCode, Result, StreamParams};
use ironav_formats::{open_demuxer_lazy, subtitle::{plain_text, truncate, MAX_CUES, MAX_CUE_BYTES, MAX_TOTAL_BYTES}};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

struct Caption {
    start: f64,
    end: f64,
    text: String,
    /// The latest end among this cue and all earlier cues in start order.
    latest_end: f64,
}

/// Timed plain text from an embedded track or a standalone subtitle file.
pub struct SubtitleTrack {
    cues: Vec<Caption>,
}

impl SubtitleTrack {
    /// Read the track with this stream index, or use `None` for a standalone subtitle file.
    /// This may scan the media, so call it on a background thread for interactive playback.
    pub fn load(path: &Path, index: Option<usize>, cancelled: &AtomicBool) -> Result<Self> {
        let check_cancelled = || {
            if cancelled.load(Ordering::Relaxed) {
                Err(Error::new(ErrorCode::Cancelled, "subtitle loading cancelled"))
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
        let mut demux = open_demuxer_lazy(path)?;
        let stream = match (index, demux.streams()) {
            (None, [s]) => Some(s),
            (Some(index), streams) => streams.iter().find(|s| s.index == index),
            _ => None,
        }
        .filter(|s| s.params == StreamParams::Subtitle)
        .ok_or_else(|| Error::invalid_input(if index.is_some() { "the subtitle track is missing" } else { "choose a standalone SRT, WebVTT, ASS or SSA subtitle file" }))?;
        let (index, codec, tick) = (stream.index, stream.codec, stream.time_base.as_f64());
        let discard: Vec<_> = demux.streams().iter().map(|s| s.index).filter(|&i| i != index).collect();
        for i in discard {
            demux.discard(i);
        }
        let mut cues = Vec::new();
        let mut bytes = 0;
        loop {
            check_cancelled()?;
            let Some(packet) = demux.read_packet()? else { break };
            if packet.stream != index || packet.duration <= 0 {
                continue;
            }
            let start = packet.pts as f64 * tick;
            let end = packet.pts.saturating_add(packet.duration) as f64 * tick;
            if !start.is_finite() || !end.is_finite() || end <= start {
                continue;
            }
            let text = truncate(plain_text(codec, &packet.data), MAX_CUE_BYTES);
            if text.is_empty() {
                continue;
            }
            bytes += text.len();
            if cues.len() == MAX_CUES || bytes > MAX_TOTAL_BYTES {
                return Err(Error::invalid_input("the subtitle track exceeds IronAV's cue or text limit"));
            }
            cues.push(Caption { start, end, text, latest_end: end });
        }
        check_cancelled()?;
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut latest_end = f64::NEG_INFINITY;
        for cue in &mut cues {
            latest_end = latest_end.max(cue.end);
            cue.latest_end = latest_end;
        }
        Ok(Self { cues })
    }

    /// Active text and the next change's media timestamp, in seconds. A cue is active from its
    /// start up to its end. If cues overlap, show them together in start order.
    pub fn at(&self, seconds: f64) -> (String, Option<f64>) {
        const DISPLAY_BYTES: usize = 2048;
        let end = self.cues.partition_point(|c| c.start <= seconds);
        let start = self.cues[..end].partition_point(|c| c.latest_end <= seconds);
        let mut next = self.cues.get(end).map(|c| c.start);
        let mut text = String::new();
        for cue in &self.cues[start..end] {
            if cue.end <= seconds {
                continue;
            }
            next = Some(next.map_or(cue.end, |n| n.min(cue.end)));
            if !text.is_empty() {
                text.push('\n');
            }
            let remaining = DISPLAY_BYTES - text.len();
            let mut len = remaining.min(cue.text.len());
            while !cue.text.is_char_boundary(len) {
                len -= 1;
            }
            text.push_str(&cue.text[..len]);
            if len < cue.text.len() || text.len() == DISPLAY_BYTES {
                break;
            }
        }
        (text, next)
    }
}
