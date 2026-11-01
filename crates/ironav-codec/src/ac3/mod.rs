//! AC-3 (Dolby Digital, ATSC A/52) audio decoder: the full-bandwidth channels of every channel
//! mode (1/0 to 3/2) with the LFE channel, coupling, rematrixing, delta bit allocation, block
//! switching and dynamic range control at FFmpeg's default settings (`drc_scale` 1). Output is
//! float, in the channel order of the mode (FL FR FC LFE BL BR for 3/2 with LFE), as FFmpeg's
//! `ac3` decoder gives it. Not decoded: E-AC-3 (refused with an error), and the downmix.
//!
//! The decoding follows FFmpeg's `libavcodec/ac3dec.c`, `ac3dec_float.c`, `ac3.c` and `ac3dsp.c`,
//! which are licensed under the LGPL-2.1 or later (Copyright (c) 2000-2008 the FFmpeg developers,
//! including Fabrice Bellard, Kartikey Mahendra Bhatt, Bartlomiej Wolowiec and Justin Ruggles).

mod alloc;
mod bits;
mod block;
mod header;
mod synth;
mod tables;

use bits::BitReader;
use std::borrow::Cow;
use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result};

/// Samples per channel in one audio block, and blocks per AC-3 frame.
const BLOCK: usize = 256;
const BLOCKS: usize = 6;

/// For each channel mode (acmod) and LFE flag: the decoded channel that each output channel
/// takes, in the output order (FL FR FC LFE BL BR for 3/2 with LFE).
const OUTPUT_MAP: [[&[usize]; 2]; 8] = [
    [&[0, 1], &[0, 1, 2]],
    [&[0], &[0, 1]],
    [&[0, 1], &[0, 1, 2]],
    [&[0, 2, 1], &[0, 2, 1, 3]],
    [&[0, 1, 2], &[0, 1, 3, 2]],
    [&[0, 2, 1, 3], &[0, 2, 1, 4, 3]],
    [&[0, 1, 2, 3], &[0, 1, 4, 2, 3]],
    [&[0, 2, 1, 3, 4], &[0, 2, 1, 5, 3, 4]],
];

/// The AC-3 decoder. Each packet is one or more AC-3 frames; the decoder carries its overlap
/// state from frame to frame.
pub struct Ac3Decoder {
    st: Box<block::State>,
}

impl Default for Ac3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Ac3Decoder {
    pub fn new() -> Self {
        Ac3Decoder { st: Box::new(block::State::new()) }
    }

    /// Decodes the frame at the start of `data` (after any bytes before its sync word). Returns the
    /// interleaved samples, the frame's header and the number of bytes the frame used.
    fn decode_frame(&mut self, data: &[u8]) -> Result<(Vec<f64>, header::Header, usize)> {
        let off = sync_offset(data)?;
        let frame = unswapped(&data[off..]);
        let mut r = BitReader::new(&frame);
        let hdr = parse_header(&mut r)?;
        if hdr.frame_size > frame.len() {
            return Err(Error::invalid_input("AC-3: the frame is cut short by the end of the data"));
        }

        let st = &mut self.st;
        st.sr_code = hdr.sr_code;
        st.ba.sr_code = hdr.sr_code;
        st.ba.sr_shift = hdr.sr_shift;
        st.channel_mode = hdr.acmod;
        st.lfe_on = hdr.lfe_on;
        st.channels = hdr.channels;
        st.fbw = hdr.channels - hdr.lfe_on as usize;
        st.lfe_ch = st.fbw + 1;
        if st.lfe_on {
            // the LFE channel has a fixed bandwidth: seven bins in two exponent groups
            st.start_freq[st.lfe_ch] = 0;
            st.end_freq[st.lfe_ch] = 7;
            st.num_exp_groups[st.lfe_ch] = 2;
            st.channel_in_cpl[st.lfe_ch] = false;
        }
        st.num_blocks = BLOCKS;
        for blk in 0..BLOCKS {
            st.decode_block(&mut r, blk).map_err(|msg| Error::invalid_input(format!("AC-3: {msg}")))?;
        }

        let map = OUTPUT_MAP[hdr.acmod][hdr.lfe_on as usize];
        let n = BLOCKS * BLOCK;
        let nch = map.len();
        let mut samples = vec![0f64; n * nch];
        for (j, &c) in map.iter().enumerate() {
            for (i, &s) in st.output[c][..n].iter().enumerate() {
                samples[i * nch + j] = s as f64;
            }
        }
        Ok((samples, hdr, off + hdr.frame_size))
    }
}

/// What the header at the start of a frame says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Where the frame's sync word is, in the bytes given (up to 10 bytes of junk may come first).
    pub offset: usize,
    /// The frame's length in bytes, from the sync word.
    pub frame_size: usize,
    pub sample_rate: u32,
    /// The channels the frame carries, the LFE channel included.
    pub channels: u16,
}

/// The header of the first AC-3 frame in `data`. An E-AC-3 frame is an error.
pub fn frame_info(data: &[u8]) -> Result<FrameInfo> {
    let offset = sync_offset(data)?;
    let frame = unswapped(&data[offset..]);
    let hdr = parse_header(&mut BitReader::new(&frame))?;
    Ok(FrameInfo { offset, frame_size: hdr.frame_size, sample_rate: hdr.sample_rate, channels: hdr.channels as u16 })
}

/// Where the sync word of the frame at the start of `data` is: at most 10 bytes in.
fn sync_offset(data: &[u8]) -> Result<usize> {
    header::find_syncword(data).filter(|&i| i <= 10).ok_or_else(|| Error::invalid_input("AC-3: no sync word at the start of the data"))
}

/// The frame's bytes with the byte order of the sync word restored: a stream may be byte-swapped
/// (sync word 0x770B), which FFmpeg also reads.
fn unswapped(data: &[u8]) -> Cow<'_, [u8]> {
    if data.starts_with(&[0x77, 0x0B]) {
        Cow::Owned(data.chunks(2).flat_map(|c| c.iter().rev().copied()).collect())
    } else {
        Cow::Borrowed(data)
    }
}

fn parse_header(r: &mut BitReader) -> Result<header::Header> {
    header::parse(r).map_err(|msg| {
        let code = if msg.contains("E-AC-3") { ErrorCode::UnsupportedCodec } else { ErrorCode::InvalidInput };
        Error::new(code, format!("AC-3: {msg}"))
    })
}

impl Decoder for Ac3Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        let mut pos = 0;
        let mut pts = packet.pts;
        loop {
            let (samples, hdr, used) = self.decode_frame(&packet.data[pos..])?;
            frames.push(Frame::Audio(AudioFrame { sample_rate: hdr.sample_rate, channels: hdr.channels as u16, pts, samples }));
            pts += (BLOCKS * BLOCK) as i64;
            pos += used;
            // whatever follows a frame is decoded only if it holds another frame
            if pos >= packet.data.len() || header::find_syncword(&packet.data[pos..]).is_none() {
                break;
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_map_is_a_permutation_of_the_decoded_channels() {
        for (mode, row) in OUTPUT_MAP.iter().enumerate() {
            for (lfe, map) in row.iter().enumerate() {
                let channels = [2usize, 1, 2, 3, 3, 4, 4, 5][mode] + lfe;
                assert_eq!(map.len(), channels, "mode {mode} lfe {lfe}");
                let mut sorted = map.to_vec();
                sorted.sort();
                assert_eq!(sorted, (0..channels).collect::<Vec<_>>(), "mode {mode} lfe {lfe}");
            }
        }
    }
}
