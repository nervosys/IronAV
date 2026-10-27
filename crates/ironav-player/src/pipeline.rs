//! The decoding threads. Each owns a demuxer opened on the same file, so audio and video are
//! read, decoded and queued independently; the clock in `lib.rs` decides what is shown.

use crate::output::{AudioOutput, NullOutput};
use crate::{AudioMode, Cmd, Inner, TimedFrame};
use ironav_codec::is_implemented;
use ironav_core::{Decoder, Error, ErrorCode, Frame, MediaType, Result, StreamInfo, StreamParams};
use ironav_formats::open_demuxer;
use ironav_hwaccel::{new_video_decoder, probe_all, select, Direction, HwPolicy};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Audio kept queued ahead of the device, in seconds.
const AUDIO_AHEAD: f64 = 0.5;
/// Audio needed before playback may start, in seconds.
const AUDIO_READY: f64 = 0.1;
/// Decoded pictures kept ahead of the clock.
const VIDEO_FRAMES_AHEAD: usize = 16;
const VIDEO_BYTES_AHEAD: usize = 256 << 20;

/// A decoder for the video stream: software if one exists, otherwise a GPU decoder.
pub fn open_video_decoder(s: &StreamInfo, hwaccel: &str) -> Result<(Box<dyn Decoder>, &'static str)> {
    let soft = is_implemented(s.codec, false);
    let policy = HwPolicy::parse(hwaccel)?;
    if soft && (!ironav_codec::prefers_hardware_decode(s.codec) || matches!(policy, HwPolicy::Off)) {
        return Ok((ironav_codec::new_decoder_for(s)?, "software"));
    }
    let sel = select(s.codec, Direction::Decode, policy, &probe_all())?;
    match sel.chosen {
        Some(backend) => Ok((new_video_decoder(backend, s.codec, &s.params)?, backend.name())),
        None if soft && !matches!(policy, HwPolicy::Require(_) | HwPolicy::RequireAny) => Ok((ironav_codec::new_decoder_for(s)?, "software")),
        None => {
            let why = sel.considered.iter().map(|c| format!("{}: {}", c.backend.name(), c.reason)).collect::<Vec<_>>().join("; ");
            Err(Error::new(ErrorCode::HardwareUnavailable, format!("{} needs a hardware decoder and none is usable ({why})", s.codec.name()))
                .hint("run `ironav hw probe`; set hwaccel to \"auto\" if it was \"off\""))
        }
    }
}

pub fn open_output(mode: AudioMode, rate: u32, channels: u16, warnings: &mut Vec<String>) -> Arc<dyn AudioOutput> {
    #[cfg(feature = "audio")]
    if mode == AudioMode::Auto {
        match crate::output::CpalOutput::open() {
            Ok(o) => return Arc::new(o),
            Err(e) => warnings.push(format!("no sound output ({}); playing silently", e.message)),
        }
    }
    #[cfg(not(feature = "audio"))]
    if mode == AudioMode::Auto {
        warnings.push("this build has no sound output; playing silently".into());
    }
    let _ = (&warnings, mode);
    Arc::new(NullOutput::new(rate, channels.clamp(1, 8)))
}

type Spawned = (Sender<Cmd>, JoinHandle<()>);

pub fn spawn_video(inner: Arc<Inner>, path: String, stream: StreamInfo, dec: Box<dyn Decoder>, start: f64) -> Spawned {
    let (tx, rx) = channel();
    let h = std::thread::Builder::new().name("ironav-video".into()).spawn(move || {
        if let Err(e) = video_loop(&inner, &path, &stream, dec, start, rx) {
            inner.fail(e);
        }
    });
    (tx, h.expect("spawn video thread"))
}

pub fn spawn_audio(inner: Arc<Inner>, path: String, stream: StreamInfo, dec: Box<dyn Decoder>, start: f64) -> Spawned {
    let (tx, rx) = channel();
    let h = std::thread::Builder::new().name("ironav-audio".into()).spawn(move || {
        if let Err(e) = audio_loop(&inner, &path, &stream, dec, start, rx) {
            inner.fail(e);
        }
    });
    (tx, h.expect("spawn audio thread"))
}

/// Open a demuxer that only reads `stream`.
fn demuxer_for(path: &str, stream: &StreamInfo) -> Result<Box<dyn ironav_core::Demuxer>> {
    let mut d = open_demuxer(Path::new(path))?;
    let others: Vec<usize> = d.streams().iter().map(|s| s.index).filter(|&i| i != stream.index).collect();
    for i in others {
        d.discard(i);
    }
    Ok(d)
}

enum Wake {
    Cmd(Cmd),
    Nothing,
    Closed,
}

fn poll(rx: &Receiver<Cmd>, wait: Duration) -> Wake {
    let r = if wait.is_zero() { rx.try_recv().map_err(|e| matches!(e, std::sync::mpsc::TryRecvError::Disconnected)) } else {
        rx.recv_timeout(wait).map_err(|e| matches!(e, RecvTimeoutError::Disconnected))
    };
    match r {
        Ok(c) => Wake::Cmd(c),
        Err(true) => Wake::Closed,
        Err(false) => Wake::Nothing,
    }
}

// ---- video ----------------------------------------------------------------------------------

fn video_loop(inner: &Inner, path: &str, stream: &StreamInfo, mut dec: Box<dyn Decoder>, start: f64, rx: Receiver<Cmd>) -> Result<()> {
    let tb = stream.time_base.as_f64();
    let mut demux = demuxer_for(path, stream)?;
    let mut gen = 1u64;
    let mut target = start;
    let mut need_seek = start > 0.0;
    let mut draining = false; // end of input reached and decoder flushed
    // The frame to show at the seek target is the last one at or before it, so one decoded
    // frame is held back until the next shows whether it is the one.
    let mut held: Option<TimedFrame> = None;
    let mut seeking = start > 0.0;

    loop {
        if inner.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let idle = draining || {
            let q = inner.video.lock().unwrap();
            q.frames.len() >= VIDEO_FRAMES_AHEAD || q.bytes >= VIDEO_BYTES_AHEAD
        };
        match poll(&rx, if idle { Duration::from_millis(5) } else { Duration::ZERO }) {
            Wake::Cmd(Cmd::Stop) | Wake::Closed => return Ok(()),
            Wake::Cmd(Cmd::Seek { to, gen: g }) => {
                gen = g;
                target = to;
                need_seek = true;
                seeking = true;
                draining = false;
                held = None;
                continue;
            }
            Wake::Nothing => {}
        }
        if idle {
            // Keep the queue moving even when nobody is displaying it (headless playback).
            if !draining {
                if let Some(pos) = inner.update() {
                    inner.video.lock().unwrap().discard_stale(pos);
                }
            }
            continue;
        }
        if need_seek {
            need_seek = false;
            if target > 0.0 || gen > 1 {
                demux.seek(target)?;
            }
            // A new decoder: the old one may hold frames from before the jump.
            dec = open_video_decoder(stream, &inner.hwaccel)?.0;
        }

        let mut frames: Vec<Frame> = Vec::new();
        match demux.read_packet()? {
            Some(p) => frames.extend(dec.decode(&p)?),
            None => {
                frames.extend(dec.flush()?);
                draining = true;
            }
        }
        for f in frames {
            let Frame::Video(v) = f else { continue };
            let t = TimedFrame { time: v.pts as f64 * tb, frame: v };
            let ready = if seeking && t.time <= target + 1e-6 {
                held = Some(t);
                None
            } else {
                seeking = false;
                let mut out = Vec::new();
                out.extend(held.take());
                out.push(t);
                Some(out)
            };
            if let Some(list) = ready {
                for t in list {
                    if !push_video(inner, gen, t) {
                        break;
                    }
                }
            }
        }
        if draining {
            // End of input: whatever is held is the last frame at or before the target.
            if let Some(t) = held.take() {
                push_video(inner, gen, t);
            }
            let mut q = inner.video.lock().unwrap();
            if q.gen == gen {
                q.ended = true;
                q.ready = true;
                q.end_time = q.frames.back().map(|f| f.time).or(q.last.as_ref().map(|f| f.time)).unwrap_or(target) + inner.frame_duration;
            }
        }
    }
}

/// Queue a decoded frame unless a newer seek has made it stale. Returns false if it was dropped.
fn push_video(inner: &Inner, gen: u64, t: TimedFrame) -> bool {
    let mut q = inner.video.lock().unwrap();
    if q.gen != gen {
        return false;
    }
    q.bytes += t.frame.planes.iter().map(Vec::len).sum::<usize>();
    q.frames.push_back(Arc::new(t));
    q.ready = true;
    true
}

// ---- audio ----------------------------------------------------------------------------------

/// Streaming sample-rate and channel conversion to the device format.
struct Convert {
    in_ch: usize,
    out_ch: usize,
    ratio: f64,
    pos: f64,
    carry: Vec<f32>,
}

impl Convert {
    fn new(in_rate: u32, in_ch: usize, out_rate: u32, out_ch: usize) -> Self {
        Self { in_ch, out_ch, ratio: in_rate as f64 / out_rate as f64, pos: 0.0, carry: Vec::new() }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.carry.clear();
    }

    /// Channel-map `input` (interleaved, `in_ch` wide) into `out_ch` channels.
    fn map_channels(&self, input: &[f64]) -> Vec<f32> {
        let (i, o) = (self.in_ch, self.out_ch);
        let frames = input.len() / i;
        let mut out = Vec::with_capacity(frames * o);
        for f in input.chunks_exact(i) {
            for c in 0..o {
                let v = if i == o {
                    f[c]
                } else if i == 1 {
                    f[0]
                } else if o == 1 {
                    f.iter().sum::<f64>() / i as f64
                } else if c < i {
                    f[c]
                } else {
                    0.0
                };
                out.push(v as f32);
            }
        }
        out
    }

    fn process(&mut self, input: &[f64]) -> Vec<f32> {
        let mapped = self.map_channels(input);
        if (self.ratio - 1.0).abs() < 1e-9 {
            return mapped;
        }
        // Linear interpolation between neighbouring input frames, continuing across calls.
        let o = self.out_ch;
        let mut src = std::mem::take(&mut self.carry);
        src.extend_from_slice(&mapped);
        let frames = src.len() / o;
        let mut out = Vec::new();
        while self.pos + 1.0 < frames as f64 {
            let (i, frac) = (self.pos as usize, (self.pos - self.pos.floor()) as f32);
            for c in 0..o {
                out.push(src[i * o + c] * (1.0 - frac) + src[(i + 1) * o + c] * frac);
            }
            self.pos += self.ratio;
        }
        let keep = (self.pos as usize).min(frames);
        self.carry = src[keep * o..].to_vec();
        self.pos -= keep as f64;
        out
    }
}

fn audio_loop(inner: &Inner, path: &str, stream: &StreamInfo, mut dec: Box<dyn Decoder>, start: f64, rx: Receiver<Cmd>) -> Result<()> {
    let out = inner.out.clone().expect("audio pipeline has an output");
    let StreamParams::Audio { sample_rate, channels } = stream.params else {
        return Err(Error::new(ErrorCode::Internal, "audio pipeline given a non-audio stream"));
    };
    let tb = stream.time_base.as_f64();
    let mut demux = demuxer_for(path, stream)?;
    let mut conv = Convert::new(sample_rate, channels as usize, out.rate(), out.channels() as usize);
    let (ahead, ready_at) = ((AUDIO_AHEAD * out.rate() as f64) as usize, (AUDIO_READY * out.rate() as f64) as usize);
    let mut gen = 1u64;
    let mut trim_before = start;
    let mut need_seek = start > 0.0;
    let mut finished = false;
    let mut last_end = start;

    loop {
        if inner.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let idle = finished || out.queued_frames() >= ahead;
        match poll(&rx, if idle { Duration::from_millis(5) } else { Duration::ZERO }) {
            Wake::Cmd(Cmd::Stop) | Wake::Closed => return Ok(()),
            Wake::Cmd(Cmd::Seek { to, gen: g }) => {
                gen = g;
                trim_before = to;
                need_seek = true;
                finished = false;
                continue;
            }
            Wake::Nothing => {}
        }
        if idle {
            continue;
        }
        if need_seek {
            need_seek = false;
            demux.seek(trim_before)?;
            dec = ironav_codec::new_decoder_for(stream)?;
            conv.reset();
        }
        let Some(p) = demux.read_packet()? else {
            finished = true;
            let mut a = inner.audio.lock().unwrap();
            if a.gen == gen {
                a.ended = true;
                a.ready = true;
                a.end_time = last_end;
            }
            continue;
        };
        let pkt_time = p.pts as f64 * tb;
        last_end = (p.pts + p.duration) as f64 * tb;
        for f in dec.decode(&p)? {
            let Frame::Audio(a) = f else { continue };
            // Drop the part of the first packet that lies before the wanted position.
            let skip = ((trim_before - pkt_time) * sample_rate as f64).round().max(0.0) as usize;
            let skip = skip.min(a.frames()) * channels as usize;
            let samples = conv.process(&a.samples[skip..]);
            if samples.is_empty() {
                continue;
            }
            let mut st = inner.audio.lock().unwrap();
            if st.gen != gen {
                break; // a seek happened while decoding: this audio is stale
            }
            out.write(&samples);
            if out.queued_frames() >= ready_at {
                st.ready = true;
            }
        }
        if pkt_time >= trim_before {
            trim_before = 0.0;
        }
    }
}

#[allow(dead_code)]
fn media_of(s: &StreamInfo) -> MediaType {
    s.params.media_type()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mapping_duplicates_mono_and_averages_to_mono() {
        let c = Convert::new(48000, 1, 48000, 2);
        assert_eq!(c.process_ref(&[0.5, -0.25]), vec![0.5, 0.5, -0.25, -0.25]);
        let c = Convert::new(48000, 2, 48000, 1);
        assert_eq!(c.process_ref(&[1.0, 0.0, 0.5, 0.5]), vec![0.5, 0.5]);
    }

    impl Convert {
        fn process_ref(&self, input: &[f64]) -> Vec<f32> {
            self.map_channels(input)
        }
    }

    #[test]
    fn resampling_keeps_the_duration_and_is_continuous_across_calls() {
        // A ramp at 8 kHz played at 16 kHz doubles the number of frames and stays a ramp.
        let mut c = Convert::new(8000, 1, 16000, 1);
        let ramp: Vec<f64> = (0..400).map(|i| i as f64 / 400.0).collect();
        let mut out = c.process(&ramp[..150]);
        out.extend(c.process(&ramp[150..]));
        assert!((out.len() as i64 - 800).abs() <= 2, "got {}", out.len());
        assert!(out.windows(2).all(|w| w[1] >= w[0]), "monotonic across the call boundary");
        assert!((out[400] - 0.5).abs() < 0.01);
        // Downsampling halves it.
        let mut c = Convert::new(16000, 1, 8000, 1);
        let n = c.process(&vec![0.25; 1000]).len() + c.process(&vec![0.25; 1000]).len();
        assert!((n as i64 - 1000).abs() <= 2, "got {n}");
    }
}
