use super::*;
use ironav_core::{Decoder, Frame, Packet};

/// A bit writer for building streams.
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
    fn pad16(&mut self) {
        while !self.n.is_multiple_of(16) {
            self.put(0, 1);
        }
    }
}

/// A major sync of 28 bytes (after the access unit header) with its checksum. TrueHD: the 6-channel
/// presentation is `arr5`, the 8-channel one `arr13` (channel maps); MLP: stereo, 16 bits.
fn major_sync(truehd: bool, substreams: u32, arr5: u32, arr13: u32) -> Vec<u8> {
    let mut w = W::new();
    w.put(0xF8726F, 24);
    if truehd {
        w.put(SYNC_TRUEHD as u32, 8);
        w.put(0, 4); // 48 kHz
        w.put(0, 4);
        w.put(0, 2);
        w.put(0, 2);
        w.put(arr5, 5);
        w.put(0, 2);
        w.put(arr13, 13);
    } else {
        w.put(SYNC_MLP as u32, 8);
        w.put(0, 4); // 16 bits
        w.put(0, 4);
        w.put(0, 4); // 48 kHz
        w.put(0xF, 4); // no second rate
        w.put(0, 11);
        w.put(1, 5); // stereo
    }
    for _ in 0..3 {
        w.put(0, 16);
    }
    w.put(1, 1); // variable bit rate
    w.put(0x1234, 15);
    w.put(substreams, 4);
    w.put(0, 2);
    w.put(0, 2);
    w.put(0x14, 8);
    let mut v = w.out;
    assert_eq!(v.len(), 18);
    v.resize(28, 0);
    let c = crc16(&v[..24]) ^ u16::from_be_bytes([v[24], v[25]]);
    v[26..28].copy_from_slice(&c.to_be_bytes());
    v
}

/// An access unit with a major sync, one substream of stereo raw 24-bit samples (no matrices, no
/// filters), `lossless_check` in its restart header, and the substream's parity and checksum if
/// `with_check`.
fn access_unit(truehd: bool, samples: &[[i32; 2]], lossless_check: u8, with_check: bool) -> Vec<u8> {
    let mut w = W::new();
    w.put(1, 1); // parameters follow
    w.put(1, 1); // with a restart header
    w.put(0x31EA >> 1, 13);
    w.put(truehd as u32, 1); // the noise type: 0x31EB is TrueHD's
    w.put(0, 16);
    w.put(0, 4); // channels 0 to 1
    w.put(1, 4);
    w.put(1, 4); // max matrix channel
    w.put(3, 4); // noise shift
    w.put(0x1ABCD, 23); // noise seed
    w.put(0, 19);
    w.put(0, 1); // no data check
    w.put(lossless_check as u32, 8);
    w.put(0, 16);
    w.put(0, 6); // matrix channel 0 to output 0, 1 to 1 (TrueHD numbers FL, FR the same)
    w.put(1, 6);
    let sum = restart_checksum(&w.out, w.n - 2);
    w.put(sum as u32, 8);
    w.put(0, 1); // no new presence flags
    w.put(1, 1); // a block size
    w.put(samples.len() as u32, 9);
    w.put(0, 1); // no matrices
    w.put(0, 1); // no output shift
    w.put(0, 1); // no quantiser step
    w.put(0, 1); // channel 0: nothing new
    w.put(0, 1);
    for s in samples {
        for &v in s {
            w.put((v + (1 << 23)) as u32 & 0xFF_FFFF, 24);
        }
    }
    w.put(1, 1); // the last block
    w.pad16();
    let mut data = w.out;
    if with_check {
        data.extend([0, 0]);
        let len = data.len();
        let p = parity(&data[..len - 2]);
        data[len - 2] = p ^ 0xA9;
        data[len - 1] = checksum8(&data, len - 2);
    }
    let mut sub_header = W::new();
    sub_header.put(0, 1);
    sub_header.put(0, 1); // a restart: not a non-restart substream
    sub_header.put(with_check as u32, 1);
    sub_header.put(0, 1);
    sub_header.put(data.len() as u32 / 2, 12);
    let sync = major_sync(truehd, 1, 1, 1);
    let words = (4 + sync.len() + 2 + data.len()) / 2;
    for nibble in 0..16u16 {
        let head = ((nibble << 12) | words as u16).to_be_bytes();
        let p = head[0] ^ head[1] ^ sub_header.out[0] ^ sub_header.out[1];
        if ((p >> 4) ^ p) & 0xF == 0xF {
            let mut au = vec![head[0], head[1], 0, 0];
            au.extend(&sync);
            au.extend(&sub_header.out);
            au.extend(&data);
            return au;
        }
    }
    unreachable!()
}

fn tone(n: usize) -> Vec<[i32; 2]> {
    (0..n).map(|i| [(i as i32 * 256 * 7) - 3000 * 256, 100_000 - (i as i32 * 256 * 11)]).collect()
}

fn packet(data: Vec<u8>) -> Packet {
    Packet { stream: 0, pts: 0, duration: 0, data }
}

#[test]
fn sample_rates_come_from_the_rate_code() {
    assert_eq!([0, 1, 2, 8, 9, 10].map(samplerate), [48000, 96000, 192000, 44100, 88200, 176400]);
    assert_eq!(samplerate(0xF), 0);
}

#[test]
fn major_sync_of_mlp_and_truehd_parses() {
    let mh = parse_major_sync(&major_sync(false, 1, 0, 0)).unwrap();
    assert!(!mh.truehd);
    assert_eq!((mh.sample_rate, mh.group1_bits, mh.channels(), mh.access_unit_size, mh.access_unit_size_pow2), (48000, 16, 2, 40, 64));
    assert_eq!((mh.header_size, mh.num_substreams, mh.is_vbr, mh.substream_info), (28, 1, true, 0x14));
    assert_eq!(mh.peak_bitrate, (0x1234 * 48000 + 8) >> 4);
    let mh = parse_major_sync(&major_sync(true, 1, 1, 1)).unwrap();
    assert!(mh.truehd);
    assert_eq!((mh.group1_bits, mh.channels()), (24, 2));
    // 3 substreams: the last one's channels are decoded (7.1 here)
    let mh = parse_major_sync(&major_sync(true, 3, 0xF, 0x4F)).unwrap();
    assert_eq!(mh.channels(), 8);
    // 4 substreams (Atmos): still the third's
    let mh = parse_major_sync(&major_sync(true, 4, 0xF, 0x4F)).unwrap();
    assert_eq!(mh.channels(), 8);
    // 2 substreams: the 8-channel word is the second's layout
    let mh = parse_major_sync(&major_sync(true, 2, 0xF, 0xF)).unwrap();
    assert_eq!(mh.channels(), 6);
}

#[test]
fn a_major_sync_with_a_wrong_checksum_or_shape_is_refused() {
    for truehd in [false, true] {
        let good = major_sync(truehd, 1, 1, 1);
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x10;
            assert!(parse_major_sync(&bad).is_err(), "byte {i}");
        }
        assert!(parse_major_sync(&good[..27]).is_err());
        assert!(parse_major_sync(&[]).is_err());
    }
    // an unknown stream type with a correct checksum
    let mut v = major_sync(false, 1, 0, 0);
    v[3] = 0xBC;
    let c = crc16(&v[..24]) ^ u16::from_be_bytes([v[24], v[25]]);
    v[26..28].copy_from_slice(&c.to_be_bytes());
    assert!(parse_major_sync(&v).is_err());
}

#[test]
fn truehd_channel_numbers_map_to_the_native_order() {
    // 7.1: FL FR FC LFE BL BR SL SR in the native order; TrueHD numbers the side pair before the
    // back pair, so its channels 4 and 5 (side) are places 6 and 7, and 6 and 7 (back) places 4 and 5
    let mask = FL | FR | FC | LFE | SL | SR | BL | BR;
    let places: Vec<usize> = (0..8).map(|i| decode::thd_native_index(mask, i).unwrap()).collect();
    assert_eq!(places, [0, 1, 2, 3, 6, 7, 4, 5]);
    assert_eq!(decode::thd_native_index(mask, 8), None);
    assert_eq!(decode::thd_native_index(0, 0), None);
    assert_eq!(decode::thd_native_index(FL | FR, 1), Some(1));
}

#[test]
fn decodes_raw_samples_of_an_mlp_access_unit() {
    let samples = tone(40);
    let au = access_unit(false, &samples, 0, true);
    assert_eq!(access_unit_length(&au), Some(au.len()));
    assert_eq!(major_sync_kind(&au), Some(false));
    assert!(major_sync_at_start(&au).is_some());
    let mut dec = MlpDecoder::new(false);
    let frames = dec.decode(&packet(au)).unwrap();
    assert_eq!(frames.len(), 1);
    let Frame::Audio(a) = &frames[0] else { panic!() };
    assert_eq!((a.sample_rate, a.channels, a.frames()), (48000, 2, 40));
    // a 16-bit stream: the 24-bit samples, 8 bits down
    for (i, s) in samples.iter().enumerate() {
        for c in 0..2 {
            assert_eq!(a.samples[i * 2 + c], (s[c] >> 8) as f64 / 32768.0, "sample {i} channel {c}");
        }
    }
    assert_eq!(dec.check_failures(), 0);
}

#[test]
fn decodes_raw_samples_of_a_truehd_access_unit() {
    let samples = tone(40);
    let au = access_unit(true, &samples, 0, false);
    assert_eq!(major_sync_kind(&au), Some(true));
    let mut dec = MlpDecoder::new(true);
    let frames = dec.decode(&packet(au)).unwrap();
    let Frame::Audio(a) = &frames[0] else { panic!() };
    assert_eq!((a.sample_rate, a.channels, a.frames()), (48000, 2, 40));
    for (i, s) in samples.iter().enumerate() {
        for c in 0..2 {
            assert_eq!(a.samples[i * 2 + c], s[c] as f64 / 8388608.0, "sample {i} channel {c}");
        }
    }
    assert_eq!(dec.check_failures(), 0);
    // an MLP decoder refuses TrueHD, and the other way round
    assert!(MlpDecoder::new(false).decode(&packet(access_unit(true, &samples, 0, false))).is_err());
    assert!(MlpDecoder::new(true).decode(&packet(access_unit(false, &samples, 0, false))).is_err());
}

/// The lossless check value the decoder keeps: the xor of the samples, each shifted by its channel.
fn lossless_check_of(samples: &[[i32; 2]]) -> u8 {
    let mut c = 0u32;
    for s in samples {
        for (ch, &v) in s.iter().enumerate() {
            c ^= ((v & 0xFF_FFFF) as u32) << ch;
        }
    }
    (c ^ (c >> 16) ^ ((c ^ (c >> 16)) >> 8)) as u8
}

#[test]
fn the_lossless_check_of_the_previous_access_unit_is_compared() {
    let samples = tone(40);
    let right = lossless_check_of(&samples);
    // the second access unit's restart header carries the check of what was decoded before it
    let mut dec = MlpDecoder::new(true);
    dec.decode(&packet(access_unit(true, &samples, 0, false))).unwrap();
    dec.decode(&packet(access_unit(true, &samples, right, false))).unwrap();
    assert_eq!(dec.check_failures(), 0);
    let mut dec = MlpDecoder::new(true);
    dec.decode(&packet(access_unit(true, &samples, 0, false))).unwrap();
    dec.decode(&packet(access_unit(true, &samples, right ^ 0x40, false))).unwrap();
    assert_eq!(dec.check_failures(), 1);
}

#[test]
fn a_bad_substream_parity_or_checksum_is_counted_not_fatal() {
    let samples = tone(40);
    let good = access_unit(false, &samples, 0, true);
    let mut dec = MlpDecoder::new(false);
    dec.decode(&packet(good.clone())).unwrap();
    assert_eq!(dec.check_failures(), 0);
    for tail in [1, 2] {
        let mut bad = good.clone();
        let n = bad.len();
        bad[n - tail] ^= 0x55;
        let mut dec = MlpDecoder::new(false);
        assert_eq!(dec.decode(&packet(bad)).unwrap().len(), 1);
        assert_eq!(dec.check_failures(), 1, "byte {tail} from the end");
    }
}

#[test]
fn an_access_unit_with_a_bad_parity_nibble_is_refused() {
    let au = access_unit(false, &tone(40), 0, false);
    for nibble in 0..16u8 {
        let mut bad = au.clone();
        bad[0] = (bad[0] & 0x0F) | (nibble << 4);
        let r = MlpDecoder::new(false).decode(&packet(bad.clone()));
        assert_eq!(r.is_ok(), bad[0] == au[0], "nibble {nibble}");
    }
}

#[test]
fn bad_input_is_an_error_or_nothing_and_never_a_panic() {
    for truehd in [false, true] {
        let au = access_unit(truehd, &tone(40), 0x55, true);
        // truncated at every length
        for n in 0..au.len() {
            let _ = MlpDecoder::new(truehd).decode(&packet(au[..n].to_vec()));
        }
        // every bit flipped, in a decoder that has seen the good unit first and one that has not
        for i in 0..au.len() * 8 {
            let mut bad = au.clone();
            bad[i / 8] ^= 1 << (i % 8);
            let _ = MlpDecoder::new(truehd).decode(&packet(bad.clone()));
            let mut dec = MlpDecoder::new(truehd);
            dec.decode(&packet(au.clone())).unwrap();
            let _ = dec.decode(&packet(bad));
        }
        // noise
        let mut x = 12345u32;
        for len in [4usize, 8, 40, 100, 400] {
            for _ in 0..50 {
                let junk: Vec<u8> = (0..len)
                    .map(|_| {
                        x = x.wrapping_mul(1103515245).wrapping_add(12345);
                        (x >> 16) as u8
                    })
                    .collect();
                let _ = MlpDecoder::new(truehd).decode(&packet(junk));
            }
        }
        // noise behind a good major sync and substream header
        for _ in 0..200 {
            let mut bad = au.clone();
            for b in bad[34..].iter_mut() {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                *b = (x >> 16) as u8;
            }
            let _ = MlpDecoder::new(truehd).decode(&packet(bad));
        }
    }
    // empty, short, a length of zero, and lengths past the data
    let mut dec = MlpDecoder::new(false);
    assert!(dec.decode(&packet(Vec::new())).is_err());
    assert!(dec.decode(&packet(vec![0; 3])).is_err());
    assert!(dec.decode(&packet(vec![0x00, 0x01, 0, 0])).is_err());
    assert!(dec.decode(&packet(vec![0x0F, 0xFF, 0, 0, 0xF8, 0x72, 0x6F, 0xBB])).is_err());
    assert!(MlpDecoder::new(true).decode(&packet(vec![0x00, 0x20, 0, 0, 0xF8, 0x72, 0x6F, 0xBA])).is_err());
}

#[test]
fn an_access_unit_before_any_major_sync_is_skipped() {
    let mut au = access_unit(false, &tone(40), 0, false);
    // no major sync word: the decoder has no parameters, so it gives no frame and no error
    au[4..8].copy_from_slice(&[0, 0, 0, 0]);
    assert!(MlpDecoder::new(false).decode(&packet(au)).unwrap().is_empty());
}
