//! The decoding threads. Each owns a demuxer opened on the same file, so audio and video are
//! read, decoded and queued independently; the clock in `lib.rs` decides what is shown.

use crate::audio_fx::EffectsProcessor;
use crate::output::{AudioOutput, NullOutput};
use crate::spectrum::{Analyzer, CaptureMode, PcmTiming};
use crate::{AudioMode, Cmd, Inner, SeekMode, TimedFrame};
use ironav_codec::{deinterlace_frame, is_implemented};
use ironav_core::{Decoder, Demuxer, Error, ErrorCode, FieldOrder, Frame, MediaType, PixelFormat, Result, StreamInfo, StreamParams};
use ironav_formats::open_demuxer_lazy;
use ironav_hwaccel::{new_video_decoder, probe_all, select, Direction, HwPolicy};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Audio kept queued ahead of the device, in seconds.
const AUDIO_AHEAD: f64 = 0.5;
const AUDIO_TOOLS_AHEAD: f64 = 0.14;
/// Audio needed before playback may start, in seconds.
const AUDIO_READY: f64 = 0.1;
/// Decoded pictures kept ahead of the clock.
const VIDEO_FRAMES_AHEAD: usize = 16;
const VIDEO_BYTES_AHEAD: usize = 256 << 20;
/// Reverse playback decodes from a preceding keyframe and keeps a bounded tail of pictures.
const REVERSE_WINDOW: f64 = 1.0;
const REVERSE_FRAMES: usize = 96;
const REVERSE_BYTES: usize = 128 << 20;

/// A decoder for the video stream: software if one exists, otherwise a GPU decoder.
pub fn open_video_decoder(s: &StreamInfo, hwaccel: &str) -> Result<(Box<dyn Decoder>, &'static str)> {
    open_video_decoder_with(s, hwaccel, &ironav_codec::DecoderOptions::default())
}

/// [`open_video_decoder`] with options for a software decoder.
pub fn open_video_decoder_with(s: &StreamInfo, hwaccel: &str, o: &ironav_codec::DecoderOptions) -> Result<(Box<dyn Decoder>, &'static str)> {
    let soft = is_implemented(s.codec, false);
    let policy = HwPolicy::parse(hwaccel)?;
    if soft && (!ironav_codec::prefers_hardware_decode(s.codec, &s.params) || matches!(policy, HwPolicy::Off)) {
        return Ok((ironav_codec::new_decoder_with(s, o)?, "software"));
    }
    let sel = select(s.codec, Direction::Decode, policy, &probe_all())?;
    match sel.chosen {
        Some(backend) => match new_video_decoder(backend, s.codec, &s.params, &s.extradata) {
            Ok(d) => Ok((d, backend.name())),
            // the GPU cannot take this stream (for example 10-bit H.264); the software decoder can
            Err(_) if soft && !matches!(policy, HwPolicy::Require(_) | HwPolicy::RequireAny) => Ok((ironav_codec::new_decoder_with(s, o)?, "software")),
            Err(e) => Err(e),
        },
        None if soft && !matches!(policy, HwPolicy::Require(_) | HwPolicy::RequireAny) => Ok((ironav_codec::new_decoder_with(s, o)?, "software")),
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

pub fn spawn_video(inner: Arc<Inner>, path: String, stream: StreamInfo, dec: Box<dyn Decoder>, demux: Option<Box<dyn Demuxer>>, start: f64) -> Spawned {
    let (tx, rx) = channel();
    let h = std::thread::Builder::new().name("ironav-video".into()).spawn(move || {
        if let Err(e) = video_loop(&inner, &path, &stream, dec, demux, start, rx) {
            inner.fail(e);
        }
    });
    (tx, h.expect("spawn video thread"))
}

pub fn spawn_audio(inner: Arc<Inner>, path: String, stream: StreamInfo, dec: Box<dyn Decoder>, demux: Option<Box<dyn Demuxer>>, start: f64) -> Spawned {
    let (tx, rx) = channel();
    let h = std::thread::Builder::new().name("ironav-audio".into()).spawn(move || {
        if let Err(e) = audio_loop(&inner, &path, &stream, dec, demux, start, rx) {
            inner.fail(e);
        }
    });
    (tx, h.expect("spawn audio thread"))
}

/// A demuxer that only reads `stream`: the one the probe at open left (when it is handed over, so the
/// file is not indexed again), or a new one.
fn demuxer_for(path: &str, stream: &StreamInfo, given: Option<Box<dyn Demuxer>>) -> Result<Box<dyn Demuxer>> {
    let mut d = match given {
        Some(d) => d,
        None => open_demuxer_lazy(Path::new(path))?,
    };
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

/// The next command, if one is waiting. Waiting for one is `Bell::wait`, which a command rings.
fn poll(rx: &Receiver<Cmd>) -> Wake {
    match rx.try_recv() {
        Ok(c) => Wake::Cmd(c),
        Err(TryRecvError::Disconnected) => Wake::Closed,
        Err(TryRecvError::Empty) => Wake::Nothing,
    }
}

// ---- video ----------------------------------------------------------------------------------

struct ReverseWindow {
    cursor: f64,
    first: bool,
    at_start: bool,
    frames: VecDeque<(TimedFrame, FieldOrder)>,
}

impl ReverseWindow {
    fn new(cursor: f64) -> Self {
        Self { cursor, first: true, at_start: false, frames: VecDeque::new() }
    }

    /// Read towards the cursor from a preceding keyframe. Return those pictures from latest to
    /// earliest. The next window ends just before the oldest picture kept in this one.
    fn load(&mut self, inner: &Inner, stream: &StreamInfo, dec: &mut Box<dyn Decoder>, demux: &mut dyn Demuxer, gen: u64) -> Result<()> {
        let from = (self.cursor - REVERSE_WINDOW).max(0.0);
        demux.seek(from)?;
        if !dec.reset() {
            *dec = open_video_decoder(stream, &inner.hwaccel)?.0;
        }
        let tb = stream.time_base.as_f64();
        let (mut bytes, mut trimmed) = (0, false);
        'decode: loop {
            if inner.shutdown.load(std::sync::atomic::Ordering::Relaxed) || inner.video.lock().unwrap().gen != gen {
                self.frames.clear();
                return Ok(());
            }
            let packet = demux.read_packet()?;
            let frames = match &packet {
                Some(packet) => dec.decode(packet)?,
                None => dec.flush()?,
            };
            let orders = if frames.is_empty() { Vec::new() } else { dec.field_orders() };
            let color = if frames.is_empty() { None } else { dec.color_info() };
            for (index, frame) in frames.into_iter().enumerate() {
                let Frame::Video(frame) = frame else { continue };
                let time = frame.pts as f64 * tb;
                if time > self.cursor + 1e-6 || !self.first && time >= self.cursor - 1e-6 {
                    break 'decode;
                }
                bytes += frame.planes.iter().map(Vec::len).sum::<usize>();
                self.frames.push_back((TimedFrame { time, frame, color }, orders.get(index).copied().unwrap_or_default()));
                while self.frames.len() > 1 && (self.frames.len() > REVERSE_FRAMES || bytes > REVERSE_BYTES) {
                    if let Some((old, _)) = self.frames.pop_front() {
                        bytes -= old.frame.planes.iter().map(Vec::len).sum::<usize>();
                        trimmed = true;
                    }
                }
            }
            if packet.is_none() {
                break;
            }
        }
        self.at_start = from == 0.0 && !trimmed;
        self.cursor = self.frames.front().map_or(from, |(frame, _)| frame.time);
        self.first = false;
        Ok(())
    }
}

fn video_loop(inner: &Inner, path: &str, stream: &StreamInfo, mut dec: Box<dyn Decoder>, demux: Option<Box<dyn Demuxer>>, start: f64, rx: Receiver<Cmd>) -> Result<()> {
    let tb = stream.time_base.as_f64();
    let mut demux = demuxer_for(path, stream, demux)?;
    let mut gen = 1u64;
    let mut rate: f64 = 1.0;
    let mut reverse: Option<ReverseWindow> = None;
    let mut target = start;
    let mut need_seek = start > 0.0;
    let mut draining = false; // end of input reached and decoder flushed
    // Whether the decoder has been given packets since it was made: only then does a seek need to reset it.
    let mut fed = false;
    // The frame to show at the seek target is the last one at or before it, so one decoded
    // frame is held back until the next shows whether it is the one.
    let mut held = None;
    let mut seeking = start > 0.0;
    // A keyframe seek is over once its picture is queued: while paused nothing more is decoded, so a
    // burst of them (a seek bar being dragged) costs one picture each.
    let mut keyframe_only = false;
    let mut parked = false;
    // A keyframe seek asks the decoder for the keyframe's picture at once (a flush), which ends its stream
    // as far as it is concerned: the time of that picture, when playing has to start over from it.
    let mut flushed_at: Option<f64> = None;
    let (mut asked, mut got) = (false, false);

    loop {
        if inner.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        // read before looking at anything, so a ring after this point is not missed
        let seen = inner.video_bell.count();
        match poll(&rx) {
            Wake::Cmd(Cmd::Stop) | Wake::Closed => return Ok(()),
            Wake::Cmd(Cmd::Seek { to, gen: g, mode, rate: speed }) => {
                gen = g;
                rate = speed;
                reverse = (rate < 0.0).then(|| ReverseWindow::new(to));
                target = to;
                need_seek = true;
                seeking = mode == SeekMode::Accurate;
                keyframe_only = !seeking;
                parked = false;
                flushed_at = None;
                (asked, got) = (false, false);
                draining = false;
                held = None;
                continue;
            }
            Wake::Nothing => {}
        }
        if parked && inner.clock.lock().unwrap().playing {
            parked = false;
            if let Some(t) = flushed_at.take() {
                // the decoder was flushed to get the keyframe out: decode on from the keyframe again
                target = t;
                need_seek = true;
                seeking = true;
                held = None;
            }
        }
        let full = {
            let q = inner.video.lock().unwrap();
            q.frames.len() >= VIDEO_FRAMES_AHEAD || q.bytes >= VIDEO_BYTES_AHEAD
        };
        if draining || parked || full {
            // Keep the queue moving even when nobody is displaying it (headless playback).
            let mut wait = Duration::from_millis(250);
            if !draining && !parked {
                if let Some(pos) = inner.update() {
                    let mut q = inner.video.lock().unwrap();
                    q.discard_stale(pos);
                    // the next picture falls due: look again then (a display that takes pictures rings sooner)
                    if q.frames.len() >= 2 {
                        let distance = if q.reverse { pos - q.frames[0].time } else { q.frames[1].time - pos };
                        wait = Duration::from_secs_f64((distance / rate.abs()).clamp(0.001, 0.25));
                    }
                }
            }
            inner.video_bell.wait(seen, wait);
            continue;
        }
        if let Some(window) = &mut reverse {
            if window.frames.is_empty() && !window.at_start {
                fed = true;
                let result = window.load(inner, stream, &mut dec, &mut *demux, gen);
                if inner.video.lock().unwrap().gen != gen {
                    continue;
                }
                result?;
            }
            if let Some((t, order)) = window.frames.pop_back() {
                push_video(inner, gen, t, order)?;
            } else if window.at_start {
                let mut q = inner.video.lock().unwrap();
                if q.gen == gen {
                    q.ended = true;
                    q.ready = true;
                    q.end_time = 0.0;
                }
                draining = true;
            }
            continue;
        }
        if need_seek {
            need_seek = false;
            if target > 0.0 || gen > 1 {
                demux.seek(target)?;
            }
            // The decoder may hold frames and references from before the jump: start it over. It is
            // kept (a GPU decoder takes long to make); one that cannot be reset is made again. The
            // packet read next feeds it, so `fed` stays set.
            if fed && !dec.reset() {
                dec = open_video_decoder(stream, &inner.hwaccel)?.0;
            }
        }

        let frames = match demux.read_packet()? {
            Some(p) => {
                fed = true;
                let frames = dec.decode(&p)?;
                if frames.is_empty() && keyframe_only && !asked {
                    // a keyframe seek wants the keyframe's picture now, not when the decoder has been given
                    // enough packets to fill its threads and its reordering
                    asked = true;
                    let frames = dec.flush()?;
                    got = !frames.is_empty();
                    frames
                } else {
                    frames
                }
            }
            None => {
                draining = true;
                fed = true;
                dec.flush()?
            }
        };
        // Field orders belong to the decoder's most recent batch, including a seek or EOF flush.
        let orders = if frames.is_empty() { Vec::new() } else { dec.field_orders() };
        let color = if frames.is_empty() { None } else { dec.color_info() };
        for (i, f) in frames.into_iter().enumerate() {
            let Frame::Video(v) = f else { continue };
            let t = TimedFrame { time: v.pts as f64 * tb, frame: v, color };
            let order = orders.get(i).copied().unwrap_or_default();
            if seeking && t.time <= target + 1e-6 {
                held = Some((t, order));
                continue;
            }
            seeking = false;
            for (t, order) in held.take().into_iter().chain(std::iter::once((t, order))) {
                let time = t.time;
                if !push_video(inner, gen, t, order)? {
                    break;
                }
                if keyframe_only {
                    keyframe_only = false;
                    parked = true;
                    flushed_at = got.then_some(time);
                }
            }
            if parked {
                break; // the rest of the packet's pictures belong to the frames after the keyframe
            }
        }
        if draining {
            // End of input: whatever is held is the last frame at or before the target.
            if let Some((t, order)) = held.take() {
                push_video(inner, gen, t, order)?;
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

/// Queue a picture for playback or a seek preview, unless a newer seek made it stale.
fn push_video(inner: &Inner, gen: u64, mut t: TimedFrame, order: FieldOrder) -> Result<bool> {
    if order.is_interlaced() && t.frame.pix_fmt != PixelFormat::Rgb24 {
        if inner.video.lock().unwrap().gen != gen {
            return Ok(false);
        }
        // Filter only pictures that may be shown. Spatial interpolation needs no lookahead.
        t.frame = deinterlace_frame(&t.frame, order)?;
    }
    let mut q = inner.video.lock().unwrap();
    if q.gen != gen {
        return Ok(false);
    }
    q.bytes += t.frame.planes.iter().map(Vec::len).sum::<usize>();
    q.frames.push_back(Arc::new(t));
    q.ready = true;
    drop(q);
    inner.wake();
    Ok(true)
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

fn audio_loop(inner: &Inner, path: &str, stream: &StreamInfo, mut dec: Box<dyn Decoder>, demux: Option<Box<dyn Demuxer>>, start: f64, rx: Receiver<Cmd>) -> Result<()> {
    let out = inner.out.clone().expect("audio pipeline has an output");
    let StreamParams::Audio { sample_rate, channels } = stream.params else {
        return Err(Error::new(ErrorCode::Internal, "audio pipeline given a non-audio stream"));
    };
    let tb = stream.time_base.as_f64();
    let mut demux = demuxer_for(path, stream, demux)?;
    let mut conv = Convert::new(sample_rate, channels as usize, out.rate(), out.channels() as usize);
    let output_rate = f64::from(out.rate());
    let output_channels = usize::from(out.channels());
    let ready_at = (AUDIO_READY * output_rate) as usize;
    let mut effects = EffectsProcessor::new(out.rate(), output_channels);
    let mut analyzer = None;
    let mut analysis_start = start;
    let mut queued_total = 0u64;
    let mut play_rate = 1.0;
    let mut gen = 1u64;
    let mut trim_before = start;
    let mut need_seek = start > 0.0;
    let mut finished = false;
    let mut last_end = start;

    loop {
        if inner.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let seen = inner.audio_bell.count();
        match poll(&rx) {
            Wake::Cmd(Cmd::Stop) | Wake::Closed => return Ok(()),
            Wake::Cmd(Cmd::Seek { to, gen: g, rate, .. }) => {
                gen = g;
                trim_before = to;
                last_end = to;
                analysis_start = to;
                queued_total = 0;
                play_rate = rate;
                analyzer = None;
                effects.reset();
                need_seek = rate > 0.0;
                finished = rate < 0.0;
                if finished {
                    let mut audio = inner.audio.lock().unwrap();
                    if audio.gen == gen {
                        audio.ready = true;
                        audio.ended = true;
                    }
                } else {
                    conv.ratio = sample_rate as f64 / out.rate() as f64 * rate;
                }
                continue;
            }
            Wake::Nothing => {}
        }
        let capture = inner.spectrum.mode();
        let ahead = ((if capture != CaptureMode::Off { AUDIO_TOOLS_AHEAD } else { AUDIO_AHEAD }) * output_rate) as usize;
        let queued = out.queued_frames();
        if finished || queued >= ahead {
            // The device drains the queue at its own rate, so look again when a quarter of it is gone
            // (a command or a pause rings sooner).
            let wait = if finished { Duration::from_millis(250) } else { Duration::from_secs_f64(((queued - ahead * 3 / 4) as f64 / out.rate() as f64).clamp(0.002, 0.1)) };
            inner.audio_bell.wait(seen, wait);
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
            let mut samples = conv.process(&a.samples[skip..]);
            if samples.is_empty() {
                continue;
            }
            let settings = *inner.audio_effects.lock().unwrap();
            effects.process(&mut samples, settings);
            let accepted = {
                let mut st = inner.audio.lock().unwrap();
                if st.gen != gen {
                    break; // a seek happened while decoding: this audio is stale
                }
                let count = out.write_frames(&samples);
                if out.queued_frames() >= ready_at {
                    st.ready = true;
                }
                count
            };
            match capture {
                CaptureMode::Analysis => {
                    let analyzer = analyzer.get_or_insert_with(|| Analyzer::new(out.rate()));
                    for (i, frame) in samples.chunks_exact(output_channels).take(accepted).enumerate() {
                        if let Some(analysis) = analyzer.add_frame(frame) {
                            let time = analysis_start + (queued_total + i as u64 + 1) as f64 / output_rate * play_rate;
                            inner.spectrum.push(gen, time, analysis);
                        }
                    }
                }
                CaptureMode::Pcm => {
                    analyzer = None;
                    let timing = PcmTiming { start: analysis_start, playback_rate: play_rate, sample_rate: out.rate(), channels: output_channels };
                    inner.spectrum.push_pcm(gen, queued_total, timing, &samples[..accepted * output_channels]);
                }
                CaptureMode::Off => analyzer = None,
            }
            queued_total += accepted as u64;
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
