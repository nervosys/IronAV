//! Whether a packet holds a picture that decodes on its own, from the bitstream alone, so a
//! caller can decode keyframes only without help from the container.

use ironav_core::{CodecId, MediaType};

use crate::ts::nals;
use crate::vpxav1::{av1_sample, vp9_is_key};

/// Whether `data`, a packet of `codec` as the demuxers give it (H.264 and HEVC as Annex B), starts
/// a picture that needs no earlier one: an IDR or all-intra H.264 picture, an HEVC IRAP picture,
/// a VP8, VP9 or AV1 key frame, an MPEG-4 I-VOP, an MPEG-1 or MPEG-2 I picture. Intra-only codecs and audio are always keyframes.
/// `None` when the packet does not parse far enough to say.
pub fn is_keyframe(codec: CodecId, data: &[u8]) -> Option<bool> {
    match codec {
        CodecId::H264 => {
            let mut seen = false;
            for n in nals(data).filter(|n| !n.is_empty()) {
                match n[0] & 31 {
                    5 => return Some(true),
                    // the first slice's type: I (2, 7) or SI (4, 9). Emulation prevention cannot
                    // touch these first bits: first_mb_in_slice is short and starts the payload.
                    1 => {
                        let mut b = Ue { d: &n[1..], at: 0 };
                        b.ue()?;
                        return Some(matches!(b.ue()? % 5, 2 | 4));
                    }
                    _ => seen = true,
                }
            }
            seen.then_some(false)
        }
        CodecId::Hevc => {
            let mut seen = false;
            for n in nals(data).filter(|n| n.len() >= 2) {
                let t = n[0] >> 1 & 63;
                if t < 32 {
                    return Some((16..=23).contains(&t));
                }
                seen = true;
            }
            seen.then_some(false)
        }
        CodecId::Vp8 => data.first().map(|b| b & 1 == 0),
        CodecId::Vp9 => (!data.is_empty()).then(|| vp9_frames(data).any(vp9_is_key)),
        // a sequence header in the same unit is enough to read the frame header after it
        CodecId::Av1 => av1_sample(data, &mut false).map(|(_, key)| key),
        CodecId::Mpeg4 => data.windows(5).find(|w| w[..4] == [0, 0, 1, 0xB6]).map(|w| w[4] >> 6 == 0),
        // the first picture header: a 10-bit temporal reference, then the 3-bit picture coding type
        CodecId::Mpeg2Video | CodecId::Mpeg1Video => data.windows(6).find(|w| w[..4] == [0, 0, 1, 0]).map(|w| w[5] >> 3 & 7 == 1),
        c if c.media_type() == MediaType::Video && !matches!(c, CodecId::RawVideo | CodecId::ProRes | CodecId::Mjpeg) => None,
        _ => Some(true),
    }
}

/// The frames of a VP9 packet: those its superframe index lists, or the packet itself.
fn vp9_frames(d: &[u8]) -> impl Iterator<Item = &[u8]> {
    let last = d[d.len() - 1];
    let (n, size) = ((last & 7) as usize + 1, (last >> 3 & 3) as usize + 1);
    let index = 2 + n * size;
    let sizes: Option<Vec<usize>> = (last & 0xE0 == 0xC0 && d.len() >= index && d[d.len() - index] == last).then(|| {
        d[d.len() - index + 1..d.len() - 1].chunks(size).map(|c| c.iter().rev().fold(0, |v, &b| v << 8 | b as usize)).collect()
    });
    let sizes = sizes.unwrap_or_else(|| vec![d.len()]);
    let mut at = 0usize;
    sizes.into_iter().filter_map(move |s| {
        let f = d.get(at..at.checked_add(s)?)?;
        at += s;
        Some(f)
    })
}

/// Exp-Golomb codes from the start of a slice header.
struct Ue<'a> {
    d: &'a [u8],
    at: usize,
}

impl Ue<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.at / 8)?;
        self.at += 1;
        Some(u32::from(byte >> (7 - (self.at - 1) % 8) & 1))
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..zeros {
            v = v << 1 | self.bit()?;
        }
        Some((1u32 << zeros) - 1 + v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_keyframes_from_the_bitstream() {
        // H.264: SPS then IDR; a P slice (slice_type 0: first_mb 0 = '1', type 0 = '1'); an I slice
        // (type 7 = '0001000'); an access unit delimiter alone says nothing
        assert_eq!(is_keyframe(CodecId::H264, &[0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x88]), Some(true));
        assert_eq!(is_keyframe(CodecId::H264, &[0, 0, 1, 0x41, 0b1100_0000]), Some(false));
        assert_eq!(is_keyframe(CodecId::H264, &[0, 0, 1, 0x41, 0b1000_1000]), Some(true));
        assert_eq!(is_keyframe(CodecId::H264, &[0, 0, 1, 0x09, 0xF0]), Some(false));
        assert_eq!(is_keyframe(CodecId::H264, &[1, 2, 3]), None);
        // HEVC: VPS (32) before a CRA (21); a TRAIL_R (1)
        assert_eq!(is_keyframe(CodecId::Hevc, &[0, 0, 1, 0x40, 1, 0, 0, 1, 0x2A, 1]), Some(true));
        assert_eq!(is_keyframe(CodecId::Hevc, &[0, 0, 1, 0x02, 1]), Some(false));
        // VP8 frame tag bit 0, VP9 frame_type, MPEG-4 vop_coding_type
        assert_eq!(is_keyframe(CodecId::Vp8, &[0x10, 0, 0]), Some(true));
        assert_eq!(is_keyframe(CodecId::Vp8, &[0x11, 0, 0]), Some(false));
        assert_eq!(is_keyframe(CodecId::Vp9, &[0x82, 0x49, 0x83]), Some(true));
        assert_eq!(is_keyframe(CodecId::Vp9, &[0x86, 0, 0]), Some(false));
        // a superframe: a hidden inter frame, then a key frame; index of two one-byte sizes
        assert_eq!(is_keyframe(CodecId::Vp9, &[0x86, 0, 0x82, 0x49, 0x83, 0xC1, 2, 3, 0xC1]), Some(true));
        assert_eq!(is_keyframe(CodecId::Mpeg4, &[0, 0, 1, 0xB6, 0x10]), Some(true));
        assert_eq!(is_keyframe(CodecId::Mpeg4, &[0, 0, 1, 0xB6, 0x50]), Some(false));
        assert_eq!(is_keyframe(CodecId::Mjpeg, &[0xFF, 0xD8]), Some(true));
        assert_eq!(is_keyframe(CodecId::Aac, &[]), Some(true));
    }
}
