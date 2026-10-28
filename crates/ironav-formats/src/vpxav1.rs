//! What the Matroska and MP4 writers need to carry VP9 and AV1 as they are: which packets are key
//! frames, the `vpcC` record built from a VP9 key frame's header, and AV1 temporal units with the
//! OBUs neither container stores (temporal delimiters, padding, tile lists) taken out.

use ironav_core::Rational;

/// An MSB-first bit reader.
struct Bits<'a> {
    d: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.d.get(self.at / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(v)
    }
}

/// The colour description in a VP9 key frame's uncompressed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Vp9Key {
    pub profile: u8,
    pub bits: u8,
    /// `color_space`: 0 unknown, 1 BT.601, 2 BT.709, 3 SMPTE 170, 4 SMPTE 240, 5 BT.2020, 7 RGB.
    pub color_space: u8,
    pub full_range: bool,
    pub ssx: bool,
    pub ssy: bool,
}

/// The header of the first frame in a VP9 packet (a frame or a superframe), if it is a key frame.
pub(crate) fn vp9_key(d: &[u8]) -> Option<Vp9Key> {
    let mut b = Bits { d, at: 0 };
    if b.get(2)? != 2 {
        return None;
    }
    let lo = b.get(1)?;
    let profile = (lo | b.get(1)? << 1) as u8;
    if profile == 3 {
        b.get(1)?;
    }
    if b.get(1)? == 1 || b.get(1)? != 0 {
        return None; // show_existing_frame, or not a key frame
    }
    b.get(2)?; // show_frame, error_resilient_mode
    if b.get(24)? != 0x49_8342 {
        return None;
    }
    let bits = if profile >= 2 { if b.get(1)? == 1 { 12 } else { 10 } } else { 8 };
    let color_space = b.get(3)? as u8;
    let (full_range, ssx, ssy) = if color_space == 7 {
        (true, false, false) // RGB is 4:4:4
    } else {
        let full = b.get(1)? == 1;
        if profile & 1 == 1 {
            (full, b.get(1)? == 1, b.get(1)? == 1)
        } else {
            (full, true, true)
        }
    };
    Some(Vp9Key { profile, bits, color_space, full_range, ssx, ssy })
}

/// Whether a VP9 packet starts with a key frame.
pub(crate) fn vp9_is_key(d: &[u8]) -> bool {
    // frame_marker, profile (and a reserved bit for profile 3), show_existing_frame, frame_type
    let Some(&b) = d.first() else { return false };
    if b >> 6 != 2 {
        return false;
    }
    let profile = (b >> 5 & 1) | (b >> 4 & 1) << 1;
    let skip = if profile == 3 { 5 } else { 4 };
    b >> (7 - skip) & 1 == 0 && b >> (6 - skip) & 1 == 0
}

/// The level FFmpeg gives a VP9 stream of this picture size and frame rate (0 if none fits).
fn vp9_level(width: u32, height: u32, frame_rate: Rational) -> u8 {
    let size = width as i64 * height as i64;
    let rate = if frame_rate.num > 0 && frame_rate.den > 0 { size * frame_rate.num / frame_rate.den } else { 0 };
    const LEVELS: [(i64, i64, u8); 14] = [
        (829_440, 36_864, 10),
        (2_764_800, 73_728, 11),
        (4_608_000, 122_880, 20),
        (9_216_000, 245_760, 21),
        (20_736_000, 552_960, 30),
        (36_864_000, 983_040, 31),
        (83_558_400, 2_228_224, 40),
        (160_432_128, 2_228_224, 41),
        (311_951_360, 8_912_896, 50),
        (588_251_136, 8_912_896, 51),
        (1_176_502_272, 8_912_896, 52),
        (1_176_502_272, 35_651_584, 60),
        (2_353_004_544, 35_651_584, 61),
        (4_706_009_088, 35_651_584, 62),
    ];
    if size <= 0 {
        return 0;
    }
    LEVELS.iter().find(|&&(r, s, _)| rate <= r && size <= s).map_or(0, |l| l.2)
}

/// The body of a `vpcC` box (version 1) for a stream whose first key frame has this header, or
/// `None` for 4:4:0, which the record cannot describe.
pub(crate) fn vpcc_body(k: &Vp9Key, width: u32, height: u32, frame_rate: Rational) -> Option<Vec<u8>> {
    let subsampling = match (k.ssx, k.ssy) {
        (true, true) => 1, // 4:2:0 with chroma co-sited with luma, as FFmpeg writes when the siting is unknown
        (true, false) => 2,
        (false, false) => 3,
        (false, true) => return None,
    };
    let matrix = match k.color_space {
        1 | 3 => 6,
        2 => 1,
        4 => 7,
        5 => 9,
        7 => 0,
        _ => 2,
    };
    let mut v = vec![1, 0, 0, 0, k.profile, vp9_level(width, height, frame_rate), k.bits << 4 | subsampling << 1 | k.full_range as u8];
    v.extend([2, 2, matrix]); // primaries and transfer unspecified
    v.extend(0u16.to_be_bytes()); // no codec initialization data
    Some(v)
}

/// An AV1 temporal unit without its temporal delimiter, padding and tile list OBUs, and whether it
/// starts with a shown key frame; `None` if the OBUs do not parse. `reduced_still` tracks the
/// sequence header's `reduced_still_picture_header`, which changes how a frame header starts.
pub(crate) fn av1_sample(d: &[u8], reduced_still: &mut bool) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::with_capacity(d.len());
    let mut key = None;
    let mut at = 0;
    while at < d.len() {
        let h = d[at];
        if h & 0x80 != 0 {
            return None;
        }
        let ty = h >> 3 & 15;
        let mut p = at + 1 + (h >> 2 & 1) as usize;
        let end = if h & 2 != 0 {
            let mut size = 0u64;
            for i in 0..8 {
                let byte = *d.get(p)?;
                p += 1;
                size |= u64::from(byte & 0x7F) << (7 * i);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            p.checked_add(usize::try_from(size).ok()?).filter(|&e| e <= d.len())?
        } else {
            d.len()
        };
        if p > end {
            return None;
        }
        let payload = &d[p..end];
        match ty {
            1 => {
                // seq_profile (3), still_picture (1), reduced_still_picture_header (1)
                *reduced_still = payload.first().is_some_and(|b| b & 0x08 != 0);
            }
            3 | 6 if key.is_none() => {
                key = Some(*reduced_still || {
                    let mut b = Bits { d: payload, at: 0 };
                    // show_existing_frame, frame_type (KEY_FRAME is 0), show_frame
                    b.get(1)? == 0 && b.get(2)? == 0 && b.get(1)? == 1
                });
            }
            _ => {}
        }
        if !matches!(ty, 2 | 8 | 15) {
            out.extend_from_slice(&d[at..end]);
        }
        at = end;
    }
    Some((out, key.unwrap_or(false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp9_key_frames_are_told_from_inter_frames_in_every_profile() {
        // profile 0 key frame: marker 10, profile 00, show_existing 0, frame_type 0, show 1, error_res 0, sync code, cs 2 (BT.709), range 0
        let key0 = [0x82, 0x49, 0x83, 0x42, 0x40];
        assert!(vp9_is_key(&key0));
        assert_eq!(vp9_key(&key0), Some(Vp9Key { profile: 0, bits: 8, color_space: 2, full_range: false, ssx: true, ssy: true }));
        assert!(!vp9_is_key(&[0x86, 0, 0, 0])); // frame_type 1
        assert!(!vp9_is_key(&[0x88, 0, 0, 0])); // show_existing_frame
        // profile 1 key frame (profile_low 1): 4:4:4, BT.601, full range
        let key1 = [0xA2, 0x49, 0x83, 0x42, 0x30];
        assert_eq!(vp9_key(&key1), Some(Vp9Key { profile: 1, bits: 8, color_space: 1, full_range: true, ssx: false, ssy: false }));
        // profile 3 has a reserved bit before show_existing_frame
        assert!(vp9_is_key(&[0xB0, 0x92, 0x60]));
        assert!(!vp9_is_key(&[0xB2, 0, 0]));
        assert!(!vp9_is_key(&[0x00]));
    }

    #[test]
    fn vpcc_carries_profile_level_depth_and_subsampling() {
        let k = Vp9Key { profile: 0, bits: 8, color_space: 2, full_range: false, ssx: true, ssy: true };
        let v = vpcc_body(&k, 1920, 1080, Rational::new(30, 1)).unwrap();
        assert_eq!(v, [1, 0, 0, 0, 0, 40, 0x82, 2, 2, 1, 0, 0]);
        assert_eq!(vpcc_body(&Vp9Key { ssx: false, ..k }, 64, 64, Rational::new(30, 1)), None);
    }

    #[test]
    fn av1_temporal_units_lose_delimiters_and_padding_and_report_key_frames() {
        let td = [0x12, 0x00];
        let seq = [0x0A, 0x02, 0x00, 0x00];
        let key_frame = [0x32, 0x02, 0x10, 0xAA]; // OBU_FRAME: show_existing 0, KEY_FRAME, show_frame 1
        let inter = [0x32, 0x02, 0x30, 0xAA]; // frame_type 1
        let pad = [0x7A, 0x01, 0x00];
        let mut still = false;
        let (s, key) = av1_sample(&[&td[..], &seq, &key_frame, &pad].concat(), &mut still).unwrap();
        assert_eq!(s, [&seq[..], &key_frame].concat());
        assert!(key);
        let (s, key) = av1_sample(&[&td[..], &inter].concat(), &mut still).unwrap();
        assert_eq!(s, inter);
        assert!(!key);
        // the last OBU may leave out its size
        let (s, key) = av1_sample(&[0x30, 0x10, 0xAA], &mut still).unwrap();
        assert_eq!((s.len(), key), (3, true));
        assert!(av1_sample(&[0x32, 0x05, 0x10], &mut still).is_none(), "a size past the end is refused");
        assert!(av1_sample(&[0x80], &mut still).is_none(), "the forbidden bit is refused");
    }
}
