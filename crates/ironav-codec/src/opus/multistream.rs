//! Multistream Opus (RFC 7845 channel-mapping families 1 and 255): several Opus streams, mono or
//! coupled stereo, in one packet, and the table that maps their channels to the output channels.
//!
//! A multistream packet is the packets of its streams in order. All but the last are
//! self-delimited (RFC 6716 Appendix B): each keeps its TOC byte, and carries the lengths of its
//! frames after it, so that the packet's end can be found; the last takes the rest, in the usual
//! form. The first `coupled` streams are stereo; coded channel `c` is the left
//! (even) or right (odd) channel of stream `c / 2` when `c < 2 * coupled`, and otherwise the mono
//! channel of stream `c - coupled`.
//!
//! Family 1 (3 to 8 channels, RFC 7845 section 5.1.1.2) lists its table in Vorbis channel order.
//! The output is in the order IronAV uses for multichannel audio (the WAV and FFmpeg order, as
//! the Vorbis decoder outputs it), so the table is read through the Vorbis permutation. Family
//! 255 (up to 255 unrelated channels) is output in the table's order.

use super::{frames_of, lacing16, OpusDecoder, OpusEncoder, MAX_PACKET_SAMPLES};
use crate::channels::ORDER;
use ironav_core::{AudioFrame, Decoder, Encoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};

const RATE: u32 = 48_000;
/// The mapping table entry of a channel that carries no stream: silence.
const SILENT: u8 = 255;

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("Opus: {msg}"))
}

/// The streams, the coupled streams and the mapping table, in Vorbis order, of a family 1 layout
/// of `channels` (3 to 8) from RFC 7845 section 5.1.1.2, as libopus's surround encoder lays them out.
fn layout(channels: usize) -> Option<(usize, usize, &'static [u8])> {
    Some(match channels {
        3 => (2, 1, &[0, 2, 1]),
        4 => (2, 2, &[0, 1, 2, 3]),
        5 => (3, 2, &[0, 4, 1, 2, 3]),
        6 => (4, 2, &[0, 4, 1, 2, 3, 5]),
        7 => (5, 2, &[0, 4, 1, 2, 3, 5, 6]),
        8 => (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]),
        _ => return None,
    })
}

/// The coded channel that output channel `i` takes, in output order, from the mapping table. For
/// family 1 the table is in Vorbis order and is read through the Vorbis permutation.
fn output_map(family: u8, table: &[u8]) -> Vec<u8> {
    let n = table.len();
    if family == 1 {
        (0..n).map(|i| table[ORDER[n - 1][i]]).collect()
    } else {
        table.to_vec()
    }
}

/// The stream and the channel within it (0 left or mono, 1 right) that coded channel `c` is.
fn coded_route(c: usize, coupled: usize) -> (usize, usize) {
    if c < 2 * coupled {
        (c / 2, c % 2)
    } else {
        (c - coupled, 0)
    }
}

/// Appends the self-delimiting length of a packet of `len` bytes (RFC 6716 Appendix B).
fn push_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    if len > 1275 {
        return Err(bad("a stream's packet is longer than 1275 bytes"));
    }
    if len < 252 {
        out.push(len as u8);
    } else {
        let first = 252 + (len & 3);
        out.push(first as u8);
        out.push(((len - first) / 4) as u8);
    }
    Ok(())
}

/// The OpusHead of a family 1 stream of `channels` channels, with the pre-skip given.
pub(super) fn family1_head(channels: usize, pre_skip: usize) -> Vec<u8> {
    let (streams, coupled, table) = layout(channels).expect("family 1 is 3 to 8 channels here");
    let mut v = b"OpusHead".to_vec();
    v.push(1);
    v.push(channels as u8);
    v.extend((pre_skip as u16).to_le_bytes());
    v.extend(RATE.to_le_bytes());
    v.extend(0i16.to_le_bytes());
    v.push(1);
    v.push(streams as u8);
    v.push(coupled as u8);
    v.extend_from_slice(table);
    v
}

/// The `n` bytes of `d` from `*pos`, which move past them.
fn take<'a>(d: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos.checked_add(n).filter(|&e| e <= d.len()).ok_or_else(|| bad("a stream's packet is longer than the packet"))?;
    let s = &d[*pos..end];
    *pos = end;
    Ok(s)
}

/// The self-delimited form of a packet (RFC 6716 Appendix B): the TOC byte, the lengths its
/// frames need (all but the last frame of a VBR code 3 packet, which is measured last), then the
/// frames. Padding is not kept.
fn to_self_delimited(p: &[u8]) -> Result<Vec<u8>> {
    let frames = frames_of(p)?;
    let mut out = vec![p[0]];
    match p[0] & 3 {
        // one frame, or two of equal length (the length is of one of them)
        0 | 1 => push_len(&mut out, frames.at[0].1)?,
        2 => {
            push_len(&mut out, frames.at[0].1)?;
            push_len(&mut out, frames.at[1].1)?;
        }
        _ => {
            let m = frames.len();
            out.push((p[1] & 0x80) | m as u8);
            if p[1] & 0x80 != 0 {
                for f in frames.iter().take(m - 1) {
                    push_len(&mut out, f.1)?;
                }
                push_len(&mut out, frames.at[m - 1].1)?;
            } else {
                push_len(&mut out, frames.at[0].1)?;
            }
        }
    }
    for &(off, len) in frames.iter() {
        out.extend_from_slice(&p[off..off + len]);
    }
    Ok(out)
}

/// The usual form of the self-delimited packet at `d[*pos..]` (the inverse of
/// [`to_self_delimited`], also taking padding); `pos` moves past it.
fn from_self_delimited(d: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let toc = *d.get(*pos).ok_or_else(|| bad("truncated packet"))?;
    *pos += 1;
    let mut out = vec![toc];
    match toc & 3 {
        0 | 1 => {
            let len = lacing16(d, pos)?;
            let total = if toc & 3 == 1 { 2 * len } else { len };
            out.extend_from_slice(take(d, pos, total)?);
        }
        2 => {
            let (s1, s2) = (lacing16(d, pos)?, lacing16(d, pos)?);
            push_len(&mut out, s1)?;
            out.extend_from_slice(take(d, pos, s1 + s2)?);
        }
        _ => {
            let c = *d.get(*pos).ok_or_else(|| bad("truncated packet"))?;
            *pos += 1;
            let (m, vbr, padding) = ((c & 0x3F) as usize, c & 0x80 != 0, c & 0x40 != 0);
            if m == 0 || m > 48 {
                return Err(bad("invalid frame count"));
            }
            let mut pad = 0usize;
            if padding {
                loop {
                    let n = *d.get(*pos).ok_or_else(|| bad("truncated padding"))? as usize;
                    *pos += 1;
                    pad += n;
                    if n < 255 {
                        break;
                    }
                }
            }
            if vbr {
                let mut sizes = Vec::with_capacity(m);
                for _ in 0..m - 1 {
                    sizes.push(lacing16(d, pos)?);
                }
                sizes.push(lacing16(d, pos)?);
                out.push(0x80 | m as u8);
                for &s in &sizes[..m - 1] {
                    push_len(&mut out, s)?;
                }
                out.extend_from_slice(take(d, pos, sizes.iter().sum())?);
            } else {
                let f = lacing16(d, pos)?;
                out.push(m as u8);
                out.extend_from_slice(take(d, pos, f * m)?);
            }
            take(d, pos, pad)?;
        }
    }
    Ok(out)
}

/// Splits a multistream packet into the usual-form packets of its `streams` streams.
fn split(packet: &[u8], streams: usize) -> Result<Vec<Vec<u8>>> {
    let mut parts = Vec::with_capacity(streams);
    let mut pos = 0;
    for _ in 1..streams {
        parts.push(from_self_delimited(packet, &mut pos)?);
    }
    parts.push(packet[pos..].to_vec());
    Ok(parts)
}

/// A decoder for the packets of a multistream Opus stream (families 1 and 255).
pub struct MultistreamDecoder {
    channels: usize,
    pre_skip: usize,
    gain: f32,
    skip_left: usize,
    streams: Vec<OpusDecoder>,
    /// Per output channel: the stream and the channel within it, or `None` for silence.
    route: Vec<Option<(usize, usize)>>,
    /// Per stream: its channel count, and its decoded samples (interleaved).
    widths: Vec<usize>,
    bufs: Vec<Vec<f32>>,
}

impl MultistreamDecoder {
    /// A decoder for the OpusHead `extradata` of a stream with channel-mapping family 1 or 255.
    pub fn new(extradata: &[u8], params: &StreamParams) -> Result<Self> {
        if !matches!(params, StreamParams::Audio { .. }) {
            return Err(Error::invalid_arg("Opus decoder needs an audio stream"));
        }
        if extradata.len() < 21 || &extradata[..8] != b"OpusHead" {
            return Err(bad("the codec private data is not an OpusHead with a channel mapping table"));
        }
        let channels = extradata[9] as usize;
        let pre_skip = u16::from_le_bytes([extradata[10], extradata[11]]) as usize;
        let gain_q8 = i16::from_le_bytes([extradata[16], extradata[17]]);
        let family = extradata[18];
        if family != 1 && family != 255 {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("Opus: channel-mapping family {family} is not decoded (0, 1 and 255 are)")));
        }
        if channels == 0 || (family == 1 && channels > 8) {
            return Err(bad("the channel count is out of range for the mapping family"));
        }
        let (streams, coupled) = (extradata[19] as usize, extradata[20] as usize);
        let table = extradata.get(21..21 + channels).ok_or_else(|| bad("the channel mapping table is cut short"))?;
        if streams == 0 || coupled > streams {
            return Err(bad("the stream counts are inconsistent"));
        }
        let map = output_map(family, table);
        let mut route = Vec::with_capacity(channels);
        for &idx in &map {
            route.push(match idx {
                SILENT => None,
                i if (i as usize) < streams + coupled => Some(coded_route(i as usize, coupled)),
                _ => return Err(bad("a channel mapping entry names no stream")),
            });
        }
        let widths: Vec<usize> = (0..streams).map(|s| if s < coupled { 2 } else { 1 }).collect();
        Ok(Self {
            channels,
            pre_skip,
            gain: 10f32.powf(gain_q8 as f32 / (20.0 * 256.0)),
            skip_left: pre_skip,
            streams: widths.iter().map(|&w| OpusDecoder::stream(w)).collect(),
            route,
            bufs: widths.iter().map(|&w| vec![0.0; MAX_PACKET_SAMPLES * w]).collect(),
            widths,
        })
    }

    /// The output channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decodes a multistream packet into `out` as interleaved samples at 48 kHz, in output order,
    /// after the stream's gain and pre-skip, and returns the samples per channel written. As
    /// [`OpusDecoder::decode_into`](super::OpusDecoder::decode_into), with an empty packet concealed.
    pub fn decode_into(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        let n = if packet.is_empty() {
            self.conceal_streams()?
        } else {
            let parts = split(packet, self.streams.len())?;
            let mut n = None;
            for (s, part) in parts.into_iter().enumerate() {
                let got = self.streams[s].decode_into(&part, &mut self.bufs[s])?;
                if n.is_some_and(|m| m != got) {
                    return Err(bad("the streams of a packet differ in length"));
                }
                n = Some(got);
            }
            n.unwrap_or(0)
        };
        self.put(n, out)
    }

    /// Concealment of a lost packet, as [`OpusDecoder::conceal_into`](super::OpusDecoder::conceal_into).
    pub fn conceal_into(&mut self, out: &mut [f32]) -> Result<usize> {
        let n = self.conceal_streams()?;
        self.put(n, out)
    }

    fn conceal_streams(&mut self) -> Result<usize> {
        let mut n = 0;
        for s in 0..self.streams.len() {
            n = self.streams[s].conceal_into(&mut self.bufs[s])?;
        }
        Ok(n)
    }

    /// Writes `n` samples per channel of the streams' output to `out`, in output order, after the
    /// pre-skip and the gain; returns the samples per channel written.
    fn put(&mut self, n: usize, out: &mut [f32]) -> Result<usize> {
        let skip = self.skip_left.min(n);
        self.skip_left -= skip;
        let keep = n - skip;
        let ch = self.channels;
        if out.len() < keep * ch {
            return Err(Error::invalid_arg(format!("the packet decodes to {keep} samples per channel; the buffer holds {}", out.len() / ch)));
        }
        let mut w = 0;
        for i in skip..n {
            for c in 0..ch {
                let v = match self.route[c] {
                    Some((s, k)) => self.bufs[s][i * self.widths[s] + k],
                    None => 0.0,
                };
                out[w] = (v * self.gain).clamp(-1.0, 1.0);
                w += 1;
            }
        }
        Ok(keep)
    }
}

impl Decoder for MultistreamDecoder {
    fn decode(&mut self, packet: &ironav_core::Packet) -> Result<Vec<Frame>> {
        let before = self.skip_left;
        let mut pcm = vec![0f32; MAX_PACKET_SAMPLES * self.channels];
        let n = self.decode_into(&packet.data, &mut pcm)?;
        let skip = before - self.skip_left;
        if n == 0 {
            return Ok(Vec::new());
        }
        let samples: Vec<f64> = pcm[..n * self.channels].iter().map(|&v| v as f64).collect();
        let pts = packet.pts + skip as i64 - self.pre_skip as i64;
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: RATE, channels: self.channels as u16, pts, samples })])
    }
}

/// An encoder for multistream Opus, family 1: 3 to 8 channels, each stream coded by an
/// [`OpusEncoder`] (mono or stereo) and the streams' packets joined with self-delimiting lengths.
pub struct MultistreamEncoder {
    channels: usize,
    streams: Vec<OpusEncoder>,
    /// Per stream: the input channels it takes, in its order.
    members: Vec<Vec<usize>>,
}

impl MultistreamEncoder {
    /// An encoder for `channels` (3 to 8) at about `bitrate` bits per second, shared among the
    /// streams in proportion to their channels (a stereo stream takes twice a mono one's share).
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if sample_rate != RATE {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Opus encoder takes 48000 Hz audio, not {sample_rate} Hz")).hint("resample the audio to 48000 Hz first"));
        }
        let Some((count, coupled, table)) = layout(channels) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the multistream Opus encoder takes 3 to 8 channels, not {channels}")));
        };
        let coded = output_map(1, table);
        // per stream, the input channel for each of its channels
        let mut slots = vec![[usize::MAX; 2]; count];
        for (i, &c) in coded.iter().enumerate() {
            let (s, k) = coded_route(c as usize, coupled);
            slots[s][k] = i;
        }
        let members: Vec<Vec<usize>> = slots.iter().enumerate().map(|(s, m)| m[..if s < coupled { 2 } else { 1 }].to_vec()).collect();
        let total: u64 = members.iter().map(|m| m.len() as u64).sum();
        let streams = members
            .iter()
            .map(|m| {
                let share = (bitrate as u64 * m.len() as u64 / total) as u32;
                OpusEncoder::new(RATE, m.len(), share)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { channels, streams, members })
    }

    /// Applies `f` to each stream's encoder: the settings of a mode, frame length, FEC or DTX. Only
    /// before any audio, as those settings are.
    pub fn map_streams(mut self, f: impl Fn(OpusEncoder) -> Result<OpusEncoder>) -> Result<Self> {
        self.streams = std::mem::take(&mut self.streams).into_iter().map(f).collect::<Result<Vec<_>>>()?;
        Ok(self)
    }

    /// The OpusHead that describes this stream to a container.
    pub fn head(&self) -> Vec<u8> {
        family1_head(self.channels, self.streams[0].pre_skip())
    }

    /// Joins the packets of the streams, one set per frame; all streams give the same count.
    fn combine(per: Vec<Vec<Packet>>) -> Result<Vec<Packet>> {
        let count = per[0].len();
        if per.iter().any(|p| p.len() != count) {
            return Err(Error::invalid_input("the streams of a multistream Opus packet are out of step"));
        }
        let last = per.len() - 1;
        let mut out = Vec::with_capacity(count);
        for k in 0..count {
            let mut data = Vec::new();
            for (s, p) in per.iter().enumerate() {
                if s < last {
                    data.extend(to_self_delimited(&p[k].data)?);
                } else {
                    data.extend_from_slice(&p[k].data);
                }
            }
            out.push(Packet { stream: 0, pts: per[0][k].pts, duration: per[0][k].duration, data });
        }
        Ok(out)
    }
}

impl Encoder for MultistreamEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the Opus encoder received a video frame"));
        };
        if a.sample_rate != RATE || a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed sample rate or channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        let mut per = Vec::with_capacity(self.streams.len());
        for (s, enc) in self.streams.iter_mut().enumerate() {
            let m = &self.members[s];
            let mut samples = Vec::with_capacity(n * m.len());
            for i in 0..n {
                for &c in m {
                    samples.push(a.samples[i * self.channels + c]);
                }
            }
            per.push(enc.encode(&Frame::Audio(AudioFrame { sample_rate: RATE, channels: m.len() as u16, pts: a.pts, samples }))?);
        }
        Self::combine(per)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let per = self.streams.iter_mut().map(|e| e.flush()).collect::<Result<Vec<_>>>()?;
        if per[0].is_empty() {
            return Ok(Vec::new());
        }
        Self::combine(per)
    }
}

/// The decoder for an Opus stream: family 0 (mono or stereo) or the multistream decoder.
pub fn new_decoder(extradata: &[u8], params: &StreamParams) -> Result<Box<dyn Decoder>> {
    if extradata.len() >= 19 && extradata[18] != 0 {
        Ok(Box::new(MultistreamDecoder::new(extradata, params)?))
    } else {
        Ok(Box::new(OpusDecoder::new(extradata, params)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::packet_samples;

    #[test]
    fn family1_tables_route_every_channel_once() {
        for n in 3..=8 {
            let (streams, coupled, table) = layout(n).unwrap();
            let mut seen = vec![false; streams + coupled];
            // the table in Vorbis order, each entry a distinct coded channel
            for &c in table {
                assert!(!seen[c as usize], "{n} channels: coded channel {c} twice");
                seen[c as usize] = true;
            }
            assert!(seen.iter().all(|&s| s), "{n} channels: a coded channel unused");
            assert_eq!(table.len(), n);
        }
    }

    #[test]
    fn self_delimited_lengths_round_trip() {
        for len in [0usize, 1, 100, 251, 252, 253, 300, 1000, 1275] {
            let mut v = Vec::new();
            push_len(&mut v, len).unwrap();
            let mut p = 0;
            assert_eq!(lacing16(&v, &mut p).unwrap(), len, "length {len}");
            assert_eq!(p, v.len());
        }
        assert!(push_len(&mut Vec::new(), 1276).is_err());
    }

    #[test]
    fn head_names_the_streams_and_the_table() {
        let h = family1_head(6, 312);
        assert_eq!(&h[..8], b"OpusHead");
        assert_eq!((h[9], h[18], h[19], h[20]), (6, 1, 4, 2));
        assert_eq!(&h[21..], &[0, 4, 1, 2, 3, 5]);
    }

    #[test]
    fn self_delimited_form_round_trips_every_code() {
        let frame = |n: usize, v: u8| vec![v; n];
        let cat = |parts: &[Vec<u8>]| parts.concat();
        // code 0; code 1 (two frames of 7); code 2 (5 and 300 bytes); code 3 CBR (2 frames of 6);
        // code 3 VBR (4, 300 and 2 bytes), and code 3 with padding
        let mut cases: Vec<Vec<u8>> = vec![
            cat(&[vec![31 << 3], frame(10, 1)]),
            cat(&[vec![31 << 3 | 1], frame(14, 2)]),
            cat(&[vec![31 << 3 | 2, 5], frame(5, 3), frame(300, 4)]),
            cat(&[vec![31 << 3 | 3, 2], frame(12, 5)]),
            cat(&[vec![31 << 3 | 3, 0x80 | 3, 4, 252, 12], frame(4, 6), frame(300, 7), frame(2, 8)]),
            cat(&[vec![31 << 3 | 3, 0x80 | 0x40 | 2, 3, 8], frame(8, 9), frame(40, 1), vec![0, 0, 0]]),
        ];
        // a TOC-only packet (discontinuous transmission) is a frame of no bytes
        cases.push(vec![31 << 3]);
        for case in &cases {
            let sd = to_self_delimited(case).unwrap();
            let mut pos = 0;
            let back = from_self_delimited(&sd, &mut pos).unwrap();
            assert_eq!(pos, sd.len());
            // the same frames: the padding of the last case is dropped
            let want = to_self_delimited(&back).unwrap();
            assert_eq!(want, sd);
            assert_eq!(back[0], case[0]);
            assert_eq!(frames_of(&back).unwrap().iter().map(|f| f.1).sum::<usize>(), frames_of(case).unwrap().iter().map(|f| f.1).sum::<usize>());
        }
    }

    /// Encodes `seconds` of a distinct tone on each input channel, and decodes the packets.
    /// Returns the decoded channels (output order), the packets and the head.
    fn round_trip(channels: usize, seconds: f64) -> (Vec<Vec<f64>>, Vec<Packet>, Vec<u8>) {
        let n = (RATE as f64 * seconds) as usize;
        let mut input = vec![0f64; n * channels];
        for i in 0..n {
            for c in 0..channels {
                let f = 300.0 + 170.0 * c as f64;
                input[i * channels + c] = 0.25 * (2.0 * std::f64::consts::PI * f * i as f64 / RATE as f64).sin();
            }
        }
        let mut enc = MultistreamEncoder::new(RATE, channels, 64_000 * channels as u32).unwrap();
        let head = enc.head();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: RATE, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let params = StreamParams::Audio { sample_rate: RATE, channels: channels as u16 };
        let mut dec = new_decoder(&head, &params).unwrap();
        let mut out = vec![Vec::new(); channels];
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                assert_eq!(a.channels as usize, channels);
                for (i, v) in a.samples.iter().enumerate() {
                    out[i % channels].push(*v);
                }
            }
        }
        // the input, de-interleaved
        let input = (0..channels).map(|c| (0..n).map(|i| input[i * channels + c]).collect()).collect::<Vec<Vec<f64>>>();
        (out.into_iter().zip(input).map(|(o, i)| o[..i.len().min(o.len())].to_vec()).collect(), packets, head)
    }

    #[test]
    fn surround_round_trip_keeps_each_channel_in_its_place() {
        for channels in 3..=8 {
            let (out, packets, _) = round_trip(channels, 1.0);
            assert_eq!(packet_samples(&packets[0].data), Some(960), "{channels} channels");
            for c in 0..channels {
                let f = 300.0 + 170.0 * c as f64;
                let want: Vec<f64> = (0..out[c].len()).map(|i| 0.25 * (2.0 * std::f64::consts::PI * f * i as f64 / RATE as f64).sin()).collect();
                let (mut sig, mut err) = (0.0, 0.0);
                // skip the first 20 ms, while the streams' filters settle
                for i in 960..want.len() {
                    sig += want[i] * want[i];
                    err += (want[i] - out[c][i]).powi(2);
                }
                let snr = 10.0 * (sig / err.max(1e-30)).log10();
                assert!(snr > 12.0, "{channels} channels, output channel {c}: SNR {snr:.1} dB");
            }
        }
    }

    #[test]
    fn family_255_with_the_same_routing_decodes_the_same() {
        let (out, packets, head) = round_trip(3, 0.5);
        // family 1 for three channels routes the output as coded channels 0, 1 and 2; a family
        // 255 table saying so decodes to the same samples
        let mut h255 = head.clone();
        h255[18] = 255;
        h255[21..24].copy_from_slice(&[0, 1, 2]);
        let params = StreamParams::Audio { sample_rate: RATE, channels: 3 };
        let mut dec = new_decoder(&h255, &params).unwrap();
        let mut got = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                got.extend(a.samples);
            }
        }
        // `out` is cut to the input's length; the decoder's output runs to the end of the packets
        let want: Vec<f64> = (0..out[0].len()).flat_map(|i| (0..3).map(|c| out[c][i]).collect::<Vec<_>>()).collect();
        assert!(got.len() >= want.len());
        assert!(got.iter().zip(&want).all(|(a, b)| (a - b).abs() < 1e-9));
    }
}
