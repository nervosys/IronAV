//! DTS (DTS Coherent Acoustics, "DCA", ETSI TS 102 114) core audio decoder: the core substream of
//! DTS streams and of DTS-HD streams (whose core is backward compatible), up to 5 channels plus
//! the LFE channel (audio modes 0 to 9, mono to 3/2). Output is float, in the channel order of
//! FFmpeg's `dca` decoder (FL FR FC LFE BC SL SR, those the mode has), at the core's sample rate.
//!
//! Decoded: the core frame header, the side information (subband activity, prediction mode and VQ
//! address, bit allocation, transition modes, scale factors, joint intensity coding), ADPCM
//! prediction, high-frequency vector quantisation, block codes, Huffman codes and raw samples, the
//! 32-band synthesis filter bank in both its perfect and non-perfect reconstruction forms, LFE
//! interpolation (64 and 128 times), sum/difference channel decoding, and the lossless-flagged
//! quantiser step table. Streams may be 16-bit big- or little-endian or 14-bit big- or
//! little-endian; the last three are converted to the first as FFmpeg's `ff_dca_convert_bitstream`
//! does.
//!
//! Not decoded: the core extensions inside the core frame (XCH extra surround channel, X96 96 kHz
//! extension, XXCH extra channels), which are skipped, so such a stream decodes to its 5.1 (or
//! fewer) channel core at the core's sample rate; the extension substream of DTS-HD (sync word
//! 0x64582025, carrying XLL lossless, XBR high bit rate, LBR low bit rate, XXCH and X96), which is
//! skipped after the core frame, so DTS-HD Master Audio and High Resolution streams decode to their
//! core; a DTS-HD stream with no core in it (DTS Express, or an extension substream carrying the
//! core inside itself) is refused; the embedded stereo downmix, the dynamic range coefficients and
//! the auxiliary data, which are read past; the CRCs, which are not checked; and the "deficit
//! sample" frame types (the decoder refuses them, as FFmpeg does).
//!
//! The decoding follows FFmpeg's `libavcodec/dca_core.c`, `dcadsp.c`, `synth_filter.c`, `dca.c`
//! and `dcahuff.c`, and its tables in `dcadata.c` (see `tools/gen_dca_tables.py`), which are
//! licensed under the LGPL-2.1 or later (Copyright (C) 2016 foo86). The subband arithmetic is
//! FFmpeg's fixed point (23-bit subband samples); the synthesis is float.

mod bits;
mod core;
mod huff;
mod synth;
mod tables;

use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result};
use std::borrow::Cow;

/// The sync word of a core frame in each byte order.
const SYNC_BE16: u32 = 0x7FFE8001;
const SYNC_LE16: u32 = 0xFE7F0180;
const SYNC_BE14: u32 = 0x1FFFE800;
const SYNC_LE14: u32 = 0xFF1F00E8;
/// The sync word of an extension substream (DTS-HD).
const SYNC_EXSS: u32 = 0x64582025;

/// How the words of a stream are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// 16-bit words, big-endian: the form the decoder works on.
    Be16,
    Le16,
    /// 14 bits in each 16-bit word (DTS in WAV files and on CDs), big-endian.
    Be14,
    Le14,
}

/// What the header of the first core frame in some data says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Where the frame's sync word is in the data given.
    pub offset: usize,
    /// Bytes in the frame as stored, from the sync word: the core frame, and the extension substream
    /// after it if there is one.
    pub frame_size: usize,
    /// Bytes of the core frame alone, as stored.
    pub core_size: usize,
    pub sample_rate: u32,
    /// The channels the core decodes to, the LFE channel included.
    pub channels: u16,
    /// Samples per channel in the frame.
    pub samples: usize,
    /// The bit rate the core header says, in bits per second; 0 if it is open, variable or lossless.
    pub bit_rate: u32,
    pub byte_order: ByteOrder,
    /// Whether an extension substream (DTS-HD) follows the core frame.
    pub has_extension: bool,
}

fn be32(d: &[u8]) -> u32 {
    u32::from_be_bytes([d[0], d[1], d[2], d[3]])
}

fn order_of(word: u32) -> Option<ByteOrder> {
    match word {
        SYNC_BE16 => Some(ByteOrder::Be16),
        SYNC_LE16 => Some(ByteOrder::Le16),
        SYNC_BE14 => Some(ByteOrder::Be14),
        SYNC_LE14 => Some(ByteOrder::Le14),
        _ => None,
    }
}

/// The data in the 16-bit big-endian form (`ff_dca_convert_bitstream`): the 16-bit little-endian
/// form has its bytes swapped, and the 14-bit forms have their 14-bit words packed together.
fn to_be16(data: &[u8], order: ByteOrder) -> Cow<'_, [u8]> {
    match order {
        ByteOrder::Be16 => Cow::Borrowed(data),
        ByteOrder::Le16 => {
            let mut out = Vec::with_capacity(data.len() + 1);
            for c in data.chunks(2) {
                out.push(*c.get(1).unwrap_or(&0));
                out.push(c[0]);
            }
            Cow::Owned(out)
        }
        ByteOrder::Be14 | ByteOrder::Le14 => {
            let mut out = Vec::with_capacity(data.len() * 7 / 8 + 2);
            let (mut acc, mut n) = (0u32, 0u32);
            for c in data.chunks(2) {
                let w = if order == ByteOrder::Be14 { u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) } else { u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]) };
                acc = (acc << 14) | (w & 0x3FFF) as u32;
                n += 14;
                while n >= 8 {
                    out.push((acc >> (n - 8)) as u8);
                    n -= 8;
                }
                acc &= (1 << n) - 1;
            }
            if n > 0 {
                out.push((acc << (8 - n)) as u8);
            }
            Cow::Owned(out)
        }
    }
}

/// Bytes a core frame of `size` bytes (of the converted form) takes as stored.
fn stored_size(size: usize, order: ByteOrder) -> usize {
    match order {
        ByteOrder::Be16 | ByteOrder::Le16 => size,
        // 14 bits of every 16 carry data; a frame that does not fill its last word is rounded up
        ByteOrder::Be14 | ByteOrder::Le14 => (size * 8).div_ceil(14) * 2,
    }
}

/// The size of the extension substream at the start of `data` (in the 16-bit big-endian form), if
/// it starts with a sound header.
fn exss_size(data: &[u8]) -> Option<usize> {
    if data.len() < 8 || be32(data) != SYNC_EXSS {
        return None;
    }
    let mut r = bits::Bits::new(data);
    r.skip(32 + 8 + 2);
    let wide = r.bit() as u32;
    let header = r.bits(8 + 4 * wide) as usize + 1;
    let size = r.bits(16 + 4 * wide) as usize + 1;
    (header >= 8 && size >= header).then_some(size)
}

/// A core frame found in some data.
struct Located {
    offset: usize,
    order: ByteOrder,
    header: core::Header,
    core_size: usize,
    frame_size: usize,
}

/// Finds the first core frame in `data`, its sync word at most `max_offset` bytes in: a sync word
/// (in any byte order) with a header that parses.
fn locate(data: &[u8], max_offset: usize) -> Result<Located> {
    let mut saw_exss = false;
    for off in 0..data.len().saturating_sub(3).min(max_offset.saturating_add(1)) {
        let word = be32(&data[off..]);
        if word == SYNC_EXSS {
            saw_exss = true;
        }
        let Some(order) = order_of(word) else { continue };
        let window = &data[off..data.len().min(off + core::HEADER_BYTES * 2)];
        let Ok(header) = core::parse_header(&to_be16(window, order)) else { continue };
        let core_size = stored_size(header.frame_size, order);
        let mut frame_size = core_size;
        if matches!(order, ByteOrder::Be16 | ByteOrder::Le16) {
            // the extension substream starts on a 4-byte boundary after the core
            let start = off + ((header.frame_size + 3) & !3);
            if let Some(rest) = data.get(start..data.len().min(start + 16)) {
                let rest = to_be16(rest, order);
                if let Some(n) = exss_size(&rest) {
                    if start + n <= data.len() {
                        frame_size = start + n - off;
                    }
                }
            }
        }
        return Ok(Located { offset: off, order, header, core_size, frame_size });
    }
    Err(if saw_exss {
        Error::new(ErrorCode::UnsupportedCodec, "DTS: an extension substream without a core frame is not decoded")
    } else {
        Error::invalid_input("DTS: no core frame sync word in the data")
    })
}

/// The header of the first core frame in `data`, in any of the four byte orders.
pub fn frame_info(data: &[u8]) -> Result<FrameInfo> {
    info_of(locate(data, usize::MAX)?)
}

/// The header of the core frame that starts at the first byte of `data`: what a reader that walks
/// a file frame by frame wants, as it costs no search.
pub fn frame_info_at_start(data: &[u8]) -> Result<FrameInfo> {
    info_of(locate(data, 0)?)
}

fn info_of(l: Located) -> Result<FrameInfo> {
    Ok(FrameInfo {
        offset: l.offset,
        frame_size: l.frame_size,
        core_size: l.core_size,
        sample_rate: l.header.sample_rate,
        channels: l.header.channels() as u16,
        samples: l.header.samples(),
        bit_rate: l.header.bit_rate(),
        byte_order: l.order,
        has_extension: l.frame_size > l.core_size,
    })
}

/// The DTS core decoder. A packet is one or more frames, each a core frame, with or without a
/// DTS-HD extension substream after it (which is skipped).
pub struct DtsDecoder {
    core: core::Core,
}

impl Default for DtsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DtsDecoder {
    pub fn new() -> Self {
        DtsDecoder { core: core::Core::new() }
    }
}

impl Decoder for DtsDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        let mut pos = 0;
        let mut pts = packet.pts;
        loop {
            let l = locate(&packet.data[pos..], usize::MAX)?;
            let start = pos + l.offset;
            let stored = &packet.data[start..packet.data.len().min(start + l.core_size)];
            let (h, samples) = self.core.decode(&to_be16(stored, l.order))?;
            frames.push(Frame::Audio(AudioFrame { sample_rate: h.sample_rate, channels: h.channels() as u16, pts, samples: samples.iter().map(|&x| x as f64).collect() }));
            pts += h.samples() as i64;
            pos = start + l.frame_size;
            // whatever follows a frame is decoded only if it holds another frame
            if pos >= packet.data.len() || locate(&packet.data[pos..], 0).is_err() {
                break;
            }
        }
        Ok(frames)
    }

    fn reset(&mut self) -> bool {
        self.core.reset();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit writer for building headers.
    struct W {
        out: Vec<u8>,
        n: usize,
    }

    impl W {
        fn new() -> W {
            W { out: Vec::new(), n: 0 }
        }
        fn put(&mut self, v: u32, bits: usize) {
            for i in (0..bits).rev() {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                let b = ((v >> i) & 1) as u8;
                *self.out.last_mut().unwrap() |= b << (7 - self.n % 8);
                self.n += 1;
            }
        }
    }

    /// A core frame header (and zeros for the rest of a frame of `frame_size` bytes): 16 blocks,
    /// 48 kHz, audio mode `mode`, no LFE, 192 kbit/s.
    fn header_bytes(mode: u32, frame_size: usize) -> Vec<u8> {
        let mut w = W::new();
        w.put(SYNC_BE16, 32);
        w.put(1, 1); // normal frame
        w.put(31, 5); // deficit samples 32
        w.put(0, 1); // no CRC
        w.put(15, 7); // 16 blocks
        w.put(frame_size as u32 - 1, 14);
        w.put(mode, 6);
        w.put(13, 4); // 48 kHz
        w.put(6, 5); // 192 kbit/s
        w.put(0, 1); // reserved
        w.put(0, 3); // drc, time stamp, aux
        w.put(0, 1); // hdcd
        w.put(0, 3); // extension type
        w.put(0, 1); // extension present
        w.put(0, 1); // sync flag
        w.put(0, 2); // lfe
        w.put(0, 1); // predictor history
        w.put(1, 1); // perfect filter
        w.put(0, 4 + 2); // revision, copy history
        w.put(0, 3); // 16 bit source
        w.put(0, 2); // sum/difference
        w.put(0, 4);
        let mut v = w.out;
        v.resize(frame_size, 0);
        v
    }

    #[test]
    fn parses_the_header_in_all_four_byte_orders() {
        let be = header_bytes(9, 1000);
        // 16-bit little endian: each pair of bytes swapped
        let le: Vec<u8> = be.chunks(2).flat_map(|c| [c[1], c[0]]).collect();
        // 14-bit: the 16-bit stream cut into 14-bit words, each in a 16-bit word
        let mut bits14 = Vec::new();
        let mut acc = 0u64;
        let mut n = 0;
        for &b in &be {
            acc = (acc << 8) | b as u64;
            n += 8;
            while n >= 14 {
                bits14.push(((acc >> (n - 14)) & 0x3FFF) as u16);
                n -= 14;
            }
        }
        // each 14-bit word is sign-extended to 16 bits
        let ext = |w: &u16| ((*w << 2) as i16 >> 2) as u16;
        let be14: Vec<u8> = bits14.iter().flat_map(|w| ext(w).to_be_bytes()).collect();
        let le14: Vec<u8> = bits14.iter().flat_map(|w| ext(w).to_le_bytes()).collect();
        for (data, order) in [(&be, ByteOrder::Be16), (&le, ByteOrder::Le16), (&be14, ByteOrder::Be14), (&le14, ByteOrder::Le14)] {
            let mut padded = vec![0xAAu8, 0x55];
            padded.extend(data);
            let info = frame_info(&padded).unwrap_or_else(|e| panic!("{order:?}: {e}"));
            assert_eq!(info.byte_order, order);
            assert_eq!(info.offset, 2);
            assert_eq!((info.sample_rate, info.channels, info.samples, info.bit_rate), (48000, 5, 512, 192000), "{order:?}");
            assert_eq!(info.frame_size, info.core_size);
            assert!(!info.has_extension);
        }
        // the sizes: the converted frame is 1000 bytes
        assert_eq!(frame_info(&be).unwrap().core_size, 1000);
        assert_eq!(frame_info(&be14).unwrap().core_size, (1000usize * 8).div_ceil(14) * 2);
    }

    #[test]
    fn audio_modes_give_the_channel_counts() {
        for (mode, channels) in [(0u32, 1u16), (1, 2), (2, 2), (3, 2), (4, 2), (5, 3), (6, 3), (7, 4), (8, 4), (9, 5)] {
            assert_eq!(frame_info(&header_bytes(mode, 200)).unwrap().channels, channels, "mode {mode}");
        }
        // modes above 9 are other layouts and are refused by name
        let e = frame_info(&header_bytes(10, 200)).unwrap_err();
        assert!(e.to_string().contains("no core frame"), "{e}");
    }

    #[test]
    fn finds_an_extension_substream_after_the_core() {
        let mut data = header_bytes(9, 1000);
        // an extension substream: sync, 8 user bits, index 0, narrow header, header size 16, size 40
        let mut w = W::new();
        w.put(SYNC_EXSS, 32);
        w.put(0, 8);
        w.put(0, 2);
        w.put(0, 1);
        w.put(15, 8);
        w.put(39, 16);
        let mut exss = w.out;
        exss.resize(40, 0);
        data.extend(&exss);
        let info = frame_info(&data).unwrap();
        assert_eq!((info.core_size, info.frame_size, info.has_extension), (1000, 1040, true));
        // an extension substream cut short is left out
        let info = frame_info(&data[..1030]).unwrap();
        assert_eq!((info.frame_size, info.has_extension), (1000, false));
    }

    #[test]
    fn bad_input_is_an_error_and_never_a_panic() {
        let mut dec = DtsDecoder::new();
        let packet = |data: Vec<u8>| Packet { stream: 0, pts: 0, duration: 512, data };
        assert!(dec.decode(&packet(Vec::new())).is_err());
        assert!(dec.decode(&packet(vec![0; 100])).is_err());
        assert!(dec.decode(&packet(vec![0x7F, 0xFE, 0x80, 0x01])).is_err());
        assert!(dec.decode(&packet(vec![0x7F, 0xFE, 0x80])).is_err());
        // a header with nothing behind it, and with garbage behind it
        assert!(dec.decode(&packet(header_bytes(9, 1000)[..40].to_vec())).is_err());
        assert!(dec.decode(&packet(header_bytes(9, 1000))).is_err());
        let mut junk = header_bytes(9, 1000);
        let mut x = 12345u32;
        for b in junk[18..].iter_mut() {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            *b = (x >> 16) as u8;
        }
        assert!(dec.decode(&packet(junk)).is_err());
        // absurd sizes: a frame size of 16384 on a few bytes, and a frame that is too small
        let big = header_bytes(9, 16384)[..200].to_vec();
        assert!(dec.decode(&packet(big)).is_err());
        let mut tiny = header_bytes(9, 200);
        tiny[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        assert!(dec.decode(&packet(tiny)).is_err());
        // a zero frame size field: the smallest size is 96 bytes
        let small = header_bytes(9, 96);
        assert!(dec.decode(&packet(small)).is_err());
    }

    #[test]
    fn deficit_samples_and_odd_block_counts_are_refused() {
        let mut d = header_bytes(9, 200);
        d[4] = 0x78; // normal frame bit 0, deficit samples field 11110
        assert!(core::parse_header(&d).is_err());
        let mut d = header_bytes(9, 200);
        d[5] = (14 << 2) | (d[5] & 3); // 15 blocks: not a multiple of 8
        assert!(core::parse_header(&d).is_err());
    }
}
