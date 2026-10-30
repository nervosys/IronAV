//! An Opus decoder (RFC 6716): the SILK, CELT and hybrid modes, mono and stereo.
//!
//! The band and linear-prediction decoding follow FFmpeg's native Opus decoder. SILK's 8, 12 and
//! 16 kHz output is brought to 48 kHz by a small polyphase windowed-sinc filter of our own, whose
//! delay equals the reference decoder's (4, 8 and 11 input samples).
//!
//! Channel-mapping family 0 only: one stream of one or two channels.

mod celt;
mod encoder;
pub(crate) mod mdct;
mod rc;
mod silk;
mod tables;

use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};
use rc::RangeDecoder;
pub use encoder::{opus_head, OpusEncoder};
use std::f64::consts::PI;
use tables::CELT_WINDOW2;

const RATE: u32 = 48_000;

/// Frame length in 48 kHz samples for each of the 32 TOC configurations.
const FRAME_DURATION: [usize; 32] = [
    480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960, 120,
    240, 480, 960,
];
const SILK_FRAME_MS: [usize; 16] = [10, 20, 40, 60, 10, 20, 40, 60, 10, 20, 40, 60, 10, 20, 10, 20];
/// Delay of the SILK upsampler, in input samples, per bandwidth (narrow, medium, wide).
const SILK_DELAY: [usize; 3] = [4, 8, 11];
/// The last CELT band coded for each bandwidth (narrow, medium, wide, super-wide, full).
const BAND_END: [usize; 5] = [13, 17, 17, 19, 21];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Silk,
    Hybrid,
    Celt,
}

struct Toc {
    mode: Mode,
    /// 0 narrow .. 4 full.
    bandwidth: usize,
    stereo: bool,
    config: usize,
    duration: usize,
}

fn toc(byte: u8) -> Toc {
    let config = (byte >> 3) as usize;
    let (mode, bandwidth) = match config {
        0..=11 => (Mode::Silk, config / 4),
        12..=15 => (Mode::Hybrid, 3 + (config - 12) / 2),
        _ => (
            Mode::Celt,
            match (config - 16) / 4 {
                0 => 0,
                1 => 2,
                2 => 3,
                _ => 4,
            },
        ),
    };
    Toc { mode, bandwidth, stereo: byte & 4 != 0, config, duration: FRAME_DURATION[config] }
}

/// The number of 48 kHz samples a packet decodes to, from its header alone.
pub fn packet_samples(data: &[u8]) -> Option<usize> {
    let t = toc(*data.first()?);
    let frames = match data[0] & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*data.get(1)? & 0x3F) as usize,
    };
    Some(frames * t.duration)
}

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("Opus: {msg}"))
}

fn lacing16(d: &[u8], p: &mut usize) -> Result<usize> {
    let mut v = *d.get(*p).ok_or_else(|| bad("truncated packet"))? as usize;
    *p += 1;
    if v >= 252 {
        v += 4 * *d.get(*p).ok_or_else(|| bad("truncated packet"))? as usize;
        *p += 1;
    }
    Ok(v)
}

/// A packet's frames: offset and length into it, at most 48.
struct Frames {
    at: [(usize, usize); 48],
    n: usize,
}

impl Frames {
    fn push(&mut self, f: (usize, usize)) {
        self.at[self.n] = f;
        self.n += 1;
    }

    fn iter(&self) -> impl Iterator<Item = &(usize, usize)> {
        self.at[..self.n].iter()
    }

    fn len(&self) -> usize {
        self.n
    }
}

/// Split a packet into its frames (offset and length into `d`).
fn frames_of(d: &[u8]) -> Result<Frames> {
    if d.is_empty() {
        return Err(bad("empty packet"));
    }
    let code = d[0] & 3;
    let mut p = 1;
    let mut end = d.len();
    let mut out = Frames { at: [(0, 0); 48], n: 0 };
    match code {
        0 => out.push((p, end - p)),
        1 => {
            let n = end - p;
            if n % 2 != 0 {
                return Err(bad("odd size for two equal frames"));
            }
            out.push((p, n / 2));
            out.push((p + n / 2, n / 2));
        }
        2 => {
            let a = lacing16(d, &mut p)?;
            if a > end - p {
                return Err(bad("frame longer than packet"));
            }
            out.push((p, a));
            out.push((p + a, end - p - a));
        }
        _ => {
            let b = *d.get(p).ok_or_else(|| bad("truncated packet"))?;
            p += 1;
            let count = (b & 0x3F) as usize;
            let vbr = b & 0x80 != 0;
            let mut padding = 0usize;
            if b & 0x40 != 0 {
                loop {
                    let n = *d.get(p).ok_or_else(|| bad("truncated padding"))? as usize;
                    p += 1;
                    if n < 255 {
                        padding += n;
                        break;
                    }
                    padding += 254;
                }
            }
            if count == 0 || count > 48 || padding > end - p {
                return Err(bad("invalid frame count"));
            }
            end -= padding;
            if vbr {
                // the sizes first, then the offsets once the sizes' end is known
                let mut total = 0;
                for _ in 0..count - 1 {
                    let s = lacing16(d, &mut p)?;
                    total += s;
                    out.push((0, s));
                }
                if p > end || total > end - p {
                    return Err(bad("frame sizes exceed the packet"));
                }
                out.push((0, end - p - total));
                for f in out.at[..count].iter_mut() {
                    f.0 = p;
                    p += f.1;
                }
            } else {
                let n = end - p;
                if n % count != 0 {
                    return Err(bad("frames of unequal size in a constant-bitrate packet"));
                }
                for i in 0..count {
                    out.push((p + i * (n / count), n / count));
                }
            }
        }
    }
    if out.iter().any(|&(_, s)| s > 1275) {
        return Err(bad("frame longer than 1275 bytes"));
    }
    Ok(out)
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

/// Polyphase windowed-sinc interpolation from 8, 12 or 16 kHz to 48 kHz with a fixed delay of
/// `SILK_DELAY` input samples.
struct Upsampler {
    ratio: usize,
    taps: Vec<f32>,
    per_phase: usize,
    hist: Vec<f32>,
    /// The history and the input, together.
    buf: Vec<f32>,
}

impl Upsampler {
    fn new(bandwidth: usize) -> Self {
        let ratio = [6, 4, 3][bandwidth];
        let delay = SILK_DELAY[bandwidth];
        let per_phase = 2 * delay;
        let len = per_phase * ratio;
        let centre = (delay * ratio) as f64;
        let cutoff = 0.94 / ratio as f64;
        let beta = 6.0;
        let h: Vec<f64> = (0..len)
            .map(|n| {
                let t = n as f64 - centre;
                let sinc = if t == 0.0 { 1.0 } else { (PI * cutoff * t).sin() / (PI * cutoff * t) };
                let r = (t / centre).min(1.0);
                sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
            })
            .collect();
        // taps[phase][k] = h[phase + ratio * k], each phase normalised to unit gain at DC
        let mut taps = vec![0f32; ratio * per_phase];
        for r in 0..ratio {
            let sum: f64 = (0..per_phase).map(|k| h[r + ratio * k]).sum();
            for k in 0..per_phase {
                taps[r * per_phase + k] = (h[r + ratio * k] / sum) as f32;
            }
        }
        Self { ratio, taps, per_phase, hist: vec![0.0; per_phase - 1], buf: Vec::with_capacity(per_phase + 960) }
    }

    fn process(&mut self, x: &[f32], out: &mut [f32]) {
        let keep = self.per_phase - 1;
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        buf.extend_from_slice(&self.hist);
        buf.extend_from_slice(x);
        for i in 0..x.len() {
            // x[i] is buf[keep + i]; the newest tap multiplies it
            for r in 0..self.ratio {
                let t = &self.taps[r * self.per_phase..(r + 1) * self.per_phase];
                let mut acc = 0f32;
                for (k, &c) in t.iter().enumerate() {
                    acc += c * buf[keep + i - k];
                }
                out[i * self.ratio + r] = acc;
            }
        }
        self.hist.copy_from_slice(&buf[buf.len() - keep..]);
        self.buf = buf;
    }
}

pub struct OpusDecoder {
    channels: usize,
    pre_skip: usize,
    gain: f32,
    skip_left: usize,
    silk: silk::Silk,
    celt: celt::Celt,
    up: [Option<Upsampler>; 2],
    up_bandwidth: usize,
    /// Scratch planes, sized once: a frame's output, SILK's, CELT's, and a redundancy frame's.
    planes: [Vec<f32>; 2],
    silk_out: [Vec<f32>; 2],
    celt_out: [Vec<f32>; 2],
    red_out: [Vec<f32>; 2],
    /// The last frame decoded: its length and whether it was CELT only, for concealment.
    last: Option<(usize, bool)>,
}

/// Samples per channel in the longest packet (120 ms).
pub const MAX_PACKET_SAMPLES: usize = 5760;

fn parse_head(extradata: &[u8], channels: u16) -> Result<(usize, usize, f32)> {
    if extradata.is_empty() {
        return Ok((channels.clamp(1, 2) as usize, 0, 1.0));
    }
    if extradata.len() < 19 || &extradata[..8] != b"OpusHead" {
        return Err(bad("the codec private data is not an OpusHead"));
    }
    let ch = extradata[9] as usize;
    let pre_skip = u16::from_le_bytes([extradata[10], extradata[11]]) as usize;
    let gain_q8 = i16::from_le_bytes([extradata[16], extradata[17]]);
    if extradata[18] != 0 || !(1..=2).contains(&ch) {
        return Err(Error::new(ErrorCode::UnsupportedCodec, "Opus: only mono and stereo streams (channel-mapping family 0) are decoded")
            .hint("surround Opus needs the multistream layout, which is not implemented"));
    }
    Ok((ch, pre_skip, 10f32.powf(gain_q8 as f32 / (20.0 * 256.0))))
}

impl OpusDecoder {
    pub fn new(extradata: &[u8], params: &StreamParams) -> Result<Self> {
        let StreamParams::Audio { channels, .. } = params else {
            return Err(Error::invalid_arg("Opus decoder needs an audio stream"));
        };
        let (ch, pre_skip, gain) = parse_head(extradata, *channels)?;
        Ok(Self {
            channels: ch,
            pre_skip,
            gain,
            skip_left: pre_skip,
            silk: silk::Silk::new(ch),
            celt: celt::Celt::new(ch, true),
            up: [None, None],
            up_bandwidth: 9,
            planes: [vec![0.0; 2880], vec![0.0; 2880]],
            silk_out: [vec![0.0; 960], vec![0.0; 960]],
            celt_out: [vec![0.0; 960], vec![0.0; 960]],
            red_out: [vec![0.0; 240], vec![0.0; 240]],
            last: None,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decodes a packet into `out` as interleaved samples at 48 kHz, after the stream's gain and
    /// pre-skip, and returns the samples per channel written. `out` must hold the packet's
    /// duration (at most [`MAX_PACKET_SAMPLES`] per channel). An empty packet means a lost one:
    /// it is concealed as [`conceal_into`](Self::conceal_into) does. Allocates nothing once the
    /// stream's bandwidth has been seen. A damaged frame comes out as silence.
    pub fn decode_into(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        if packet.is_empty() {
            return self.conceal_into(out);
        }
        let frames = frames_of(packet)?;
        let t = toc(packet[0]);
        let ch = self.channels;
        let total = frames.len() * t.duration;
        if out.len() < total * ch {
            return Err(Error::invalid_arg(format!("the packet decodes to {total} samples per channel; the buffer holds {}", out.len() / ch)));
        }
        let mut w = 0;
        for &(off, len) in frames.iter() {
            if self.decode_frame(&t, &packet[off..off + len]).is_err() {
                // a damaged or empty frame is replaced by silence rather than ending the stream
                for p in self.planes.iter_mut() {
                    p[..t.duration].fill(0.0);
                }
            }
            self.last = Some((t.duration, t.mode == Mode::Celt));
            w = self.put(t.duration, out, w);
        }
        Ok(w / ch)
    }

    /// A frame standing in for a lost packet, of the last frame's length: for CELT, noise shaped
    /// like the last frame's bands and fading (9 dB, then 3 dB a frame), joined to the frames on
    /// either side without a click; after SILK or hybrid frames, or before any, silence. Written
    /// to `out` as [`decode_into`](Self::decode_into) writes; returns the samples per channel.
    pub fn conceal_into(&mut self, out: &mut [f32]) -> Result<usize> {
        let ch = self.channels;
        let (n, celt) = self.last.unwrap_or((960, false));
        if out.len() < n * ch {
            return Err(Error::invalid_arg(format!("a concealed frame is {n} samples per channel; the buffer holds {}", out.len() / ch)));
        }
        let mut planes = std::mem::take(&mut self.planes);
        if celt && n <= 960 {
            self.celt.conceal(&mut planes, n)?;
        } else {
            for p in planes.iter_mut() {
                p[..n].fill(0.0);
            }
            self.silk.flush();
            self.celt.flush();
        }
        self.planes = planes;
        Ok(self.put(n, out, 0) / ch)
    }

    /// Interleaves `n` samples of the planes into `out` from `w`, after the gain and pre-skip;
    /// returns the new end.
    fn put(&mut self, n: usize, out: &mut [f32], mut w: usize) -> usize {
        let skip = self.skip_left.min(n);
        self.skip_left -= skip;
        for i in skip..n {
            for c in 0..self.channels {
                out[w] = (self.planes[c][i] * self.gain).clamp(-1.0, 1.0);
                w += 1;
            }
        }
        w
    }

    fn decode_redundancy(&mut self, t: &Toc, data: &[u8], out: &mut [Vec<f32>; 2]) -> Result<()> {
        let mut rc = RangeDecoder::new(data);
        rc.set_raw(data.len(), data.len());
        self.celt.decode_frame(&mut rc, out, t.stereo as usize + 1, 240, 0, BAND_END[t.bandwidth])
    }

    /// Decodes one frame into the scratch planes.
    fn decode_frame(&mut self, t: &Toc, data: &[u8]) -> Result<()> {
        let mut out = std::mem::take(&mut self.planes);
        let mut silk = std::mem::take(&mut self.silk_out);
        let mut celt = std::mem::take(&mut self.celt_out);
        let mut red = std::mem::take(&mut self.red_out);
        let r = self.decode_frame_to(t, data, &mut out, &mut silk, &mut celt, &mut red);
        (self.planes, self.silk_out, self.celt_out, self.red_out) = (out, silk, celt, red);
        r
    }

    fn decode_frame_to(&mut self, t: &Toc, data: &[u8], out: &mut [Vec<f32>; 2], silk_out: &mut [Vec<f32>; 2], celt_out: &mut [Vec<f32>; 2], red_out: &mut [Vec<f32>; 2]) -> Result<()> {
        let n = t.duration;
        for o in out.iter_mut() {
            o[..n].fill(0.0);
        }
        let mut size = data.len();
        if size < 2 {
            return Err(bad("frame too short"));
        }
        let mut rc = RangeDecoder::new(data);

        if t.mode != Mode::Celt {
            let sbw = t.bandwidth.min(2);
            if self.up_bandwidth != sbw {
                self.up = [Some(Upsampler::new(sbw)), Some(Upsampler::new(sbw))];
                self.up_bandwidth = sbw;
            }
            let got = self.silk.decode_superframe(&mut rc, silk_out, sbw, t.stereo as usize + 1, SILK_FRAME_MS[t.config]);
            let ratio = [6, 4, 3][sbw];
            if got * ratio != n {
                return Err(bad("SILK frame length does not match the TOC"));
            }
            for c in 0..self.channels {
                self.up[c].as_mut().unwrap().process(&silk_out[c][..got], &mut out[c][..n]);
            }
        } else {
            self.silk.flush();
            self.up = [None, None];
            self.up_bandwidth = 9;
        }

        // in-band redundancy: a short CELT frame to smooth a mode transition
        let consumed = rc.tell();
        let mut redundancy = false;
        if t.mode == Mode::Hybrid && consumed + 37 <= size as i32 * 8 {
            redundancy = rc.log(12) != 0;
        } else if t.mode == Mode::Silk && consumed + 17 <= size as i32 * 8 {
            redundancy = true;
        }
        let (mut red_pos, mut red_size) = (false, 0usize);
        if redundancy {
            red_pos = rc.log(1) != 0;
            red_size = if t.mode == Mode::Hybrid { rc.uint(256) as usize + 2 } else { size.saturating_sub(((consumed + 7) / 8) as usize) };
            if red_size > size {
                return Err(bad("invalid redundancy frame size"));
            }
            size -= red_size;
            if red_pos {
                self.decode_redundancy(t, &data[size..size + red_size], red_out)?;
                self.celt.flush();
            }
        }

        if t.mode != Mode::Silk {
            rc.set_raw(size, size);
            let start = if t.mode == Mode::Hybrid { 17 } else { 0 };
            self.celt.decode_frame(&mut rc, celt_out, t.stereo as usize + 1, n, start, BAND_END[t.bandwidth])?;
            for c in 0..self.channels {
                for (o, v) in out[c][..n].iter_mut().zip(&celt_out[c][..n]) {
                    *o += *v;
                }
            }
        } else {
            self.celt.flush();
        }

        if redundancy {
            if !red_pos {
                self.celt.flush();
                self.decode_redundancy(t, &data[size..size + red_size], red_out)?;
                for c in 0..self.channels {
                    for i in 0..120 {
                        let w = CELT_WINDOW2[i];
                        let o = &mut out[c][n - 120 + i];
                        *o = red_out[c][120 + i] * w + *o * (1.0 - w);
                    }
                }
            } else {
                for c in 0..self.channels {
                    out[c][..120].copy_from_slice(&red_out[c][..120]);
                    for i in 0..120 {
                        let w = CELT_WINDOW2[i];
                        let o = &mut out[c][120 + i];
                        *o = *o * w + red_out[c][120 + i] * (1.0 - w);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Decoder for OpusDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let d = &packet.data;
        let frames = frames_of(d)?;
        let t = toc(d[0]);
        let ch = self.channels;
        let before = self.skip_left;
        let mut pcm = vec![0f32; frames.len() * t.duration * ch];
        let n = self.decode_into(d, &mut pcm)?;
        let skip = before - self.skip_left;
        let samples: Vec<f64> = pcm[..n * ch].iter().map(|&v| v as f64).collect();
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        // times run from the start of the first kept sample
        let pts = packet.pts + skip as i64 - self.pre_skip as i64;
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: RATE, channels: ch as u16, pts, samples })])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_decodes_mode_bandwidth_and_length() {
        let t = toc(8 << 3 | 4); // config 8: SILK wideband 10 ms, stereo
        assert_eq!((t.mode, t.bandwidth, t.stereo, t.duration), (Mode::Silk, 2, true, 480));
        let t = toc(15 << 3); // hybrid full band 20 ms
        assert_eq!((t.mode, t.bandwidth, t.duration), (Mode::Hybrid, 4, 960));
        let t = toc(19 << 3); // CELT narrow 20 ms
        assert_eq!((t.mode, t.bandwidth, t.duration), (Mode::Celt, 0, 960));
        let t = toc(31 << 3); // CELT full band 20 ms
        assert_eq!((t.mode, t.bandwidth, t.duration), (Mode::Celt, 4, 960));
    }

    #[test]
    fn framing_codes() {
        assert_eq!(frames_of(&[0x00, 1, 2, 3]).unwrap().iter().copied().collect::<Vec<_>>(), vec![(1, 3)]);
        assert_eq!(frames_of(&[0x01, 1, 2, 3, 4]).unwrap().iter().copied().collect::<Vec<_>>(), vec![(1, 2), (3, 2)]);
        assert_eq!(frames_of(&[0x02, 1, 9, 8, 7]).unwrap().iter().copied().collect::<Vec<_>>(), vec![(2, 1), (3, 2)]);
        // code 3, CBR, three frames of two bytes
        assert_eq!(frames_of(&[0x03, 3, 1, 2, 3, 4, 5, 6]).unwrap().iter().copied().collect::<Vec<_>>(), vec![(2, 2), (4, 2), (6, 2)]);
        assert_eq!(packet_samples(&[19 << 3 | 3, 3, 0, 0, 0, 0, 0, 0]), Some(3 * 960));
    }

    #[test]
    fn upsampler_passes_dc_and_keeps_its_delay() {
        for bw in 0..3 {
            let mut up = Upsampler::new(bw);
            let ratio = up.ratio;
            let x = vec![0.5f32; 200];
            let mut y = vec![0f32; 200 * ratio];
            up.process(&x, &mut y);
            assert!((y[y.len() - 1] - 0.5).abs() < 1e-4, "bandwidth {bw}: {}", y[y.len() - 1]);
            // an impulse comes out centred `delay` input samples later
            let mut up = Upsampler::new(bw);
            let mut x = vec![0f32; 100];
            x[10] = 1.0;
            let mut y = vec![0f32; 100 * ratio];
            up.process(&x, &mut y);
            let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
            let want = (10 + SILK_DELAY[bw]) * ratio;
            assert!(peak.abs_diff(want) <= 1, "bandwidth {bw}: peak at {peak}, want {want}");
        }
    }
}
