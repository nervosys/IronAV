use super::*;
use ironav_core::{CodecId, Rational};

const STEREO16: &[u8] = include_bytes!("../../tests/data/wv_stereo16.wv");
const MONO24: &[u8] = include_bytes!("../../tests/data/wv_mono24.wv");
const MONO32: &[u8] = include_bytes!("../../tests/data/wv_mono32.wv");
const FLOAT: &[u8] = include_bytes!("../../tests/data/wv_stereo_float.wv");
const SURROUND: &[u8] = include_bytes!("../../tests/data/wv_51_16.wv");
const U8: &[u8] = include_bytes!("../../tests/data/wv_u8.wv");

/// The kind of raw sample FFmpeg's decode of each fixture is hashed as.
#[derive(Clone, Copy)]
enum Raw {
    S16,
    S32,
    F32,
}

fn stream(channels: u16) -> StreamInfo {
    StreamInfo {
        index: 0,
        codec: CodecId::WavPack,
        time_base: Rational::new(1, 8000),
        duration: None,
        bits: None,
        extradata: vec![0x10, 0x04],
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: 8000, channels },
    }
}

fn packet(data: Vec<u8>) -> Packet {
    Packet { stream: 0, pts: 0, duration: 0, data }
}

/// The frames of a file: the blocks from an initial one to a final one.
fn frames(file: &[u8]) -> Vec<Vec<u8>> {
    let (mut out, mut cur, mut p) = (Vec::new(), Vec::new(), 0usize);
    while p < file.len() {
        let h = BlockHeader::parse(&file[p..]).expect("a block header");
        let end = p + 8 + h.size as usize;
        cur.extend_from_slice(&file[p..end]);
        if h.is_final() {
            out.push(std::mem::take(&mut cur));
        }
        p = end;
    }
    out
}

fn decode_all(file: &[u8], channels: u16) -> Result<Vec<f64>> {
    let mut d = WavPackDecoder::new(&stream(channels))?;
    let mut out = Vec::new();
    for (i, data) in frames(file).into_iter().enumerate() {
        for f in d.decode(&Packet { stream: 0, pts: i as i64, duration: 0, data })? {
            let Frame::Audio(a) = f else { panic!("audio") };
            out.extend(a.samples);
        }
    }
    Ok(out)
}

/// FNV-1a over the samples as FFmpeg's raw output has them.
fn hash(samples: &[f64], raw: Raw) -> (usize, u64) {
    let mut bytes = Vec::new();
    for &v in samples {
        match raw {
            Raw::S16 => bytes.extend(((v * 32768.0) as i16).to_le_bytes()),
            Raw::S32 => bytes.extend(((v * 2147483648.0) as i32).to_le_bytes()),
            Raw::F32 => bytes.extend((v as f32).to_le_bytes()),
        }
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in &bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    (bytes.len(), h)
}

#[test]
fn fixtures_decode_as_ffmpeg_does() {
    // (name, file, channels, raw format, FFmpeg's output size and hash)
    let cases: [(&str, &[u8], u16, Raw, usize, u64); 6] = [
        ("stereo 16-bit", STEREO16, 2, Raw::S16, 3200, 0xb81b_3beb_b1d0_885f),
        ("mono 24-bit", MONO24, 1, Raw::S32, 3200, 0xa98b_967f_7382_085c),
        ("mono 32-bit", MONO32, 1, Raw::S32, 3200, 0xd017_399a_6935_038d),
        ("stereo float", FLOAT, 2, Raw::F32, 6400, 0xd2aa_acf6_5487_62bc),
        ("5.1 16-bit", SURROUND, 6, Raw::S16, 5760, 0xb874_ed32_df57_20d3),
        ("stereo 8-bit", U8, 2, Raw::S16, 3200, 0x0f7c_fcbc_57cd_c684),
    ];
    for (name, file, channels, raw, size, want) in cases {
        let got = decode_all(file, channels).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (n, h) = hash(&got, raw);
        assert_eq!((n, h), (size, want), "{name}: size and hash of the decode");
    }
}

#[test]
fn block_headers_and_hints() {
    let h = BlockHeader::parse(STEREO16).unwrap();
    assert_eq!((h.samples, h.total_samples, h.block_index), (800, 800, 0));
    assert!(h.initial() && h.is_final() && !h.mono() && h.bits() == 16 && h.rate() == 8000);
    assert!(BlockHeader::parse(&STEREO16[..31]).is_none(), "a header is 32 bytes");
    let mut b = STEREO16[..32].to_vec();
    b[8] = 0x01; // version 4.01
    assert!(BlockHeader::parse(&b).is_none());
    b[8] = 0x10;
    b[4..8].copy_from_slice(&23u32.to_le_bytes());
    assert!(BlockHeader::parse(&b).is_none(), "a block is 24 bytes or more");
    b[4..8].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
    assert!(BlockHeader::parse(&b).is_none(), "a block is at most 1 MiB");
    // the 5.1 file: blocks of one frame, the first initial and the last final
    assert_eq!(frames(SURROUND).len(), 1);
    let first = BlockHeader::parse(SURROUND).unwrap();
    assert!(first.initial() && !first.is_final());
    let hints = stream_hints(&SURROUND[32..8 + first.size as usize]);
    assert_eq!((hints.channels, hints.channel_mask, hints.sample_rate), (6, 0x3F, 0));
    // a custom rate: three bytes in a sub-block of two words with the odd flag
    let mut body = vec![ID_SAMPLE_RATE, 2, 0x44, 0xAC, 0x00, 0x00];
    assert_eq!(stream_hints(&body).sample_rate, 0, "three bytes of data need the odd flag");
    body[0] |= ID_IDF_ODD;
    assert_eq!(stream_hints(&body).sample_rate, 44100);
    // the long channel information: 275 channels (low byte 18, high nibble 1, plus one), mask 0xFF00
    let long = [ID_CHANINFO | ID_IDF_ODD, 4, 18, 0x00, 0x01, 0x00, 0xFF, 0x00, 0x00, 0x00];
    let h = stream_hints(&long);
    assert_eq!((h.channels, h.channel_mask), (275, 0xFF00));
    // a damaged sub-block ends the search without a panic
    assert_eq!(stream_hints(&[ID_CHANINFO, 200, 1, 2]), StreamHints::default());
    assert_eq!(stream_hints(&[ID_CHANINFO | ID_IDF_LONG, 1]), StreamHints::default());
    assert!(stream_hints(&[ID_DSD_DATA, 1, 0, 0, 0, 0]).dsd);
}

#[test]
fn bit_reader_reads_least_significant_first() {
    let mut b = Bits::new(&[0b1011_0110, 0xFF, 0x01]);
    assert_eq!(b.bit(), 0);
    assert_eq!(b.bits(3), 0b011);
    assert_eq!(b.left(), 20);
    assert_eq!(b.unary_0_33(), 2, "bits 4 and 5, then the zero of bit 6");
    assert_eq!(b.unary_0_33(), 10, "bit 7, the eight of the next byte, bit 0 of the last, then its zero");
    assert_eq!(b.left(), 6);
    // past the end it reads zeros and `left` goes negative
    let mut b = Bits::new(&[0xFF]);
    assert_eq!(b.bits(8), 0xFF);
    assert_eq!(b.bits(16), 0);
    assert_eq!(b.left(), -16);
    // 33 ones are read without the stop bit
    let mut b = Bits::new(&[0xFF; 8]);
    assert_eq!(b.unary_0_33(), 33);
    assert_eq!(b.left(), 64 - 33);
    let mut b = Bits::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    assert_eq!(b.bits(32), u32::MAX);
}

#[test]
fn log_and_exp_tables_are_inverse() {
    assert_eq!(wp_log2(0), 0);
    assert_eq!(wp_log2(1), 256);
    assert_eq!(wp_exp2(0), 0);
    assert_eq!(wp_exp2(i16::MIN), i32::MIN, "out of range");
    for v in [2u32, 3, 10, 100, 1000, 12345, 1 << 20, 1 << 30] {
        let back = wp_exp2(wp_log2(v) as i16);
        assert!((back as i64 - v as i64).abs() * 100 <= v as i64 + 100, "{v}: {back}");
    }
    assert_eq!(wp_exp2(-256 * 3), -wp_exp2(256 * 3));
}

/// A packet in Matroska's form: the sample count, then each block's flags, CRC, and (with several
/// blocks) its length, and its data without the header.
fn matroska_form(frame: &[u8]) -> Vec<u8> {
    let first = BlockHeader::parse(frame).unwrap();
    let mut out = first.samples.to_le_bytes().to_vec();
    let mut p = 0;
    while p < frame.len() {
        let h = BlockHeader::parse(&frame[p..]).unwrap();
        let end = p + 8 + h.size as usize;
        out.extend(h.flags.to_le_bytes());
        out.extend(h.crc.to_le_bytes());
        if !(h.initial() && h.is_final()) {
            out.extend((h.body_len() as u32).to_le_bytes());
        }
        out.extend(&frame[p + 32..end]);
        p = end;
    }
    out
}

#[test]
fn matroska_blocks_decode_like_raw_ones() {
    for (file, channels) in [(STEREO16, 2u16), (MONO24, 1), (SURROUND, 6), (FLOAT, 2)] {
        let mut d = WavPackDecoder::new(&stream(channels)).unwrap();
        for frame in frames(file) {
            let want = d.decode(&packet(frame.clone())).unwrap();
            let got = d.decode(&packet(matroska_form(&frame))).unwrap();
            let (Frame::Audio(w), Frame::Audio(g)) = (&want[0], &got[0]) else { panic!("audio") };
            assert_eq!(w.samples, g.samples);
        }
    }
    // damaged Matroska packets are refused
    let mut d = WavPackDecoder::new(&stream(2)).unwrap();
    let m = matroska_form(&frames(STEREO16)[0]);
    for n in [0, 5, 11, 20, m.len() / 2] {
        assert!(d.decode(&packet(m[..n].to_vec())).is_err(), "{n} bytes");
    }
}

#[test]
fn a_bad_crc_is_an_error_for_the_frame() {
    let mut d = WavPackDecoder::new(&stream(2)).unwrap();
    let f = frames(STEREO16).remove(0);
    assert!(d.decode(&packet(f.clone())).is_ok());
    let mut bad = f.clone();
    bad[28] ^= 0x40;
    let e = d.decode(&packet(bad)).unwrap_err();
    assert!(e.to_string().contains("CRC"), "{e}");
    // mono and float blocks check it too
    for (file, ch) in [(MONO24, 1u16), (FLOAT, 2)] {
        let mut d = WavPackDecoder::new(&stream(ch)).unwrap();
        let mut f = frames(file).remove(0);
        f[28] ^= 0x01;
        assert!(d.decode(&packet(f)).is_err());
    }
    // the decoder goes on with the next frame
    assert!(d.decode(&packet(frames(FLOAT).remove(0))).is_ok());
}

#[test]
fn cut_short_packets_are_errors_not_panics() {
    for (file, ch) in [(STEREO16, 2u16), (MONO24, 1), (MONO32, 1), (FLOAT, 2), (SURROUND, 6), (U8, 2)] {
        let mut d = WavPackDecoder::new(&stream(ch)).unwrap();
        let f = frames(file).remove(0);
        let ok = (0..f.len()).filter(|&n| d.decode(&packet(f[..n].to_vec())).is_ok()).count();
        // a block cut short is longer than its data, and a missing block is a missing channel
        assert_eq!(ok, 0, "{ok} of {} cut packets decoded", f.len());
        assert!(d.decode(&packet(f)).is_ok());
    }
}

#[test]
fn damaged_bytes_do_not_panic() {
    let mut seed = 0x1234_5678u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let mut errors = 0;
    for (file, ch) in [(STEREO16, 2u16), (MONO24, 1), (FLOAT, 2), (SURROUND, 6), (U8, 2), (MONO32, 1)] {
        let mut d = WavPackDecoder::new(&stream(ch)).unwrap();
        let f = frames(file).remove(0);
        for _ in 0..1500 {
            let mut b = f.clone();
            for _ in 0..1 + next() % 3 {
                let at = (next() as usize) % b.len();
                b[at] = next() as u8;
            }
            errors += d.decode(&packet(b)).is_err() as usize; // any result but a panic
        }
    }
    assert!(errors > 5000, "most damage is noticed ({errors} of 9000)");
}

#[test]
fn unsupported_and_invalid_frames_are_refused() {
    let mut d = WavPackDecoder::new(&stream(2)).unwrap();
    let f = frames(STEREO16).remove(0);
    // DSD
    let mut b = f.clone();
    let flags = le32(&b, 24) | WV_DSD_DATA;
    b[24..28].copy_from_slice(&flags.to_le_bytes());
    assert_eq!(d.decode(&packet(b)).unwrap_err().code, ErrorCode::UnsupportedFormat);
    // no samples, or too many
    for n in [0u32, 150_001, u32::MAX] {
        let mut b = f.clone();
        b[20..24].copy_from_slice(&n.to_le_bytes());
        assert!(d.decode(&packet(b)).is_err(), "{n} samples");
    }
    // a block that is longer than the packet
    let mut b = f.clone();
    b[4..8].copy_from_slice(&(f.len() as u32 + 10).to_le_bytes());
    assert!(d.decode(&packet(b)).is_err());
    // a stream of other channels than the blocks hold
    assert!(WavPackDecoder::new(&stream(1)).unwrap().decode(&packet(f.clone())).is_err(), "too many channels coded");
    assert!(WavPackDecoder::new(&stream(6)).unwrap().decode(&packet(f.clone())).is_err(), "not enough channels coded");
    assert!(WavPackDecoder::new(&stream(0)).is_err());
    let mut s = stream(2);
    s.params = StreamParams::Subtitle;
    assert!(WavPackDecoder::new(&s).is_err());
    // an empty packet and one of header size
    assert!(d.decode(&packet(Vec::new())).is_err());
    assert!(d.decode(&packet(f[..32].to_vec())).is_err());
    assert!(d.reset());
}

/// The frame with each block marked hybrid and given a HYBRID sub-block in front of its data: for
/// each channel a rate accumulator of `acc` (and a zero slow level in bitrate mode), and a zero
/// delta when `delta` is set.
fn marked_hybrid(frame: &[u8], bitrate_mode: bool, acc: u16, delta: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < frame.len() {
        let h = BlockHeader::parse(&frame[p..]).unwrap();
        let end = p + 8 + h.size as usize;
        let channels = 1 + (!h.mono() && h.flags & WV_FALSE_STEREO == 0) as usize;
        let mut sub = Vec::new();
        if bitrate_mode {
            sub.extend(std::iter::repeat_n(0u8, 2 * channels)); // slow levels
        }
        for _ in 0..channels {
            sub.extend(acc.to_le_bytes());
        }
        if delta {
            sub.extend(std::iter::repeat_n(0u8, 2 * channels));
        }
        let mut block = frame[p..p + 32].to_vec();
        let flags = h.flags | WV_HYBRID_MODE | if bitrate_mode { WV_HYBRID_BITRATE } else { 0 };
        block[24..28].copy_from_slice(&flags.to_le_bytes());
        block.extend([ID_HYBRID, (sub.len() / 2) as u8]);
        block.extend(&sub);
        block.extend(&frame[p + 32..end]);
        let size = block.len() as u32 - 8;
        block[4..8].copy_from_slice(&size.to_le_bytes());
        out.extend(block);
        p = end;
    }
    out
}

#[test]
fn hybrid_blocks_with_no_error_are_decoded() {
    // A lossless block marked hybrid, with rates that give every error limit zero, decodes exactly
    // and its CRC holds: the hybrid path (the error limit update and the clip) gives the lossless
    // samples. (In bitrate mode the accumulator, 0x7000, is a rate far above the slow level, which follows the signal.)
    for (file, ch) in [(STEREO16, 2u16), (MONO24, 1), (MONO32, 1), (SURROUND, 6)] {
        let want = decode_all(file, ch).unwrap();
        for (bitrate_mode, acc, delta) in [(false, 0, false), (false, 0, true), (true, 0x7000, false), (true, 0x7000, true)] {
            let mut d = WavPackDecoder::new(&stream(ch)).unwrap();
            let mut got = Vec::new();
            for f in frames(file) {
                let Frame::Audio(a) = d.decode(&packet(marked_hybrid(&f, bitrate_mode, acc, delta))).unwrap().remove(0) else { panic!("audio") };
                got.extend(a.samples);
            }
            assert_eq!(got, want, "channels {ch}, bitrate mode {bitrate_mode}, delta {delta}");
        }
    }
    // a block marked hybrid without its parameters is refused
    let mut f = frames(STEREO16).remove(0);
    let flags = le32(&f, 24) | WV_HYBRID_MODE;
    f[24..28].copy_from_slice(&flags.to_le_bytes());
    let e = WavPackDecoder::new(&stream(2)).unwrap().decode(&packet(f)).unwrap_err();
    assert!(e.to_string().contains("hybrid"), "{e}");
}

#[test]
fn lossy_hybrid_blocks_are_decoded_without_the_crc() {
    // a rate that allows error: the samples are not the lossless ones, and the CRC is not enforced
    let f = frames(STEREO16).remove(0);
    let want = decode_all(STEREO16, 2).unwrap();
    let b = marked_hybrid(&f, false, 0x0800, true);
    let Frame::Audio(a) = WavPackDecoder::new(&stream(2)).unwrap().decode(&packet(b)).expect("a lossy block is delivered").remove(0) else { panic!("audio") };
    assert_eq!(a.samples.len(), want.len());
    assert_ne!(a.samples, want, "the lossy decode differs from the lossless one");
    // a lossless block with a wrong CRC is still an error
    let mut b = marked_hybrid(&f, false, 0, true);
    b[28] ^= 1;
    assert!(WavPackDecoder::new(&stream(2)).unwrap().decode(&packet(b)).is_err());
}
