//! The colour boxes of a visual sample entry: `colr` (nclx or nclc), `mdcv` (SMPTE ST 2086 mastering
//! display) and `clli` (content light level). Damaged boxes are skipped, never an error.

use super::boxes;
use ironav_core::{ColorInfo, HdrMetadata, MasteringDisplay};

/// What a video track's container says about its colour.
#[derive(Clone, Copy, Default)]
pub(crate) struct VideoMeta {
    pub color: Option<ColorInfo>,
    pub hdr: Option<HdrMetadata>,
}

fn u16_at(d: &[u8], i: usize) -> Option<u32> {
    Some(u16::from_be_bytes(d.get(i..i + 2)?.try_into().ok()?) as u32)
}

fn u32_at(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
}

/// `colr`: `nclx` has the code points and a full range flag, `nclc` (QuickTime) only the code points.
fn colr(b: &[u8]) -> Option<ColorInfo> {
    let full_range = match b.get(..4)? {
        b"nclx" => b.get(10)? & 0x80 != 0,
        b"nclc" => false,
        _ => return None,
    };
    Some(ColorInfo { primaries: u16_at(b, 4)? as u8, transfer: u16_at(b, 6)? as u8, matrix: u16_at(b, 8)? as u8, full_range })
}

/// `mdcv`: green, blue, red primaries and the white point in 0.00002 units, then the maximum and
/// minimum luminance in 0.0001 cd/m².
fn mdcv(b: &[u8]) -> Option<MasteringDisplay> {
    let c = |i: usize| u16_at(b, i).map(|v| v as f32 * 0.00002);
    let point = |i: usize| Some((c(i)?, c(i + 2)?));
    let (g, bl, r) = (point(0)?, point(4)?, point(8)?);
    Some(MasteringDisplay { primaries: [r, g, bl], white_point: point(12)?, max_luminance: u32_at(b, 16)? as f32 * 0.0001, min_luminance: u32_at(b, 20)? as f32 * 0.0001 })
}

/// The colour description and HDR metadata in the child boxes of a visual sample entry.
pub(crate) fn visual_meta(kids: &[u8]) -> VideoMeta {
    let mut m = VideoMeta::default();
    let mut hdr = HdrMetadata::default();
    for (kind, body) in boxes(kids).map_while(Result::ok) {
        match &kind {
            b"colr" => m.color = colr(body).or(m.color),
            b"mdcv" => hdr.mastering = mdcv(body).or(hdr.mastering),
            b"clli" => {
                hdr.max_cll = u16_at(body, 0).filter(|&v| v > 0).or(hdr.max_cll);
                hdr.max_fall = u16_at(body, 2).filter(|&v| v > 0).or(hdr.max_fall);
            }
            _ => {}
        }
    }
    m.hdr = Some(hdr).filter(|h| !h.is_empty());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn nclx(p: u16, t: u16, m: u16, full: bool) -> Vec<u8> {
        let mut b = b"nclx".to_vec();
        for v in [p, t, m] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.push(if full { 0x80 } else { 0 });
        bx(b"colr", &b)
    }

    fn mdcv_box() -> Vec<u8> {
        let mut b = Vec::new();
        for v in [8500u16, 39850, 6550, 2300, 35400, 14600, 15635, 16450] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&40_000_000u32.to_be_bytes());
        b.extend_from_slice(&50u32.to_be_bytes());
        bx(b"mdcv", &b)
    }

    #[test]
    fn colr_mdcv_and_clli_are_read() {
        let kids = [bx(b"avcC", &[1, 2, 3]), nclx(9, 16, 9, false), mdcv_box(), bx(b"clli", &[0x0F, 0xA0, 0x03, 0xE8])].concat();
        let m = visual_meta(&kids);
        assert_eq!(m.color, Some(ColorInfo { primaries: 9, transfer: 16, matrix: 9, full_range: false }));
        let h = m.hdr.unwrap();
        assert_eq!((h.max_cll, h.max_fall), (Some(4000), Some(1000)));
        let d = h.mastering.unwrap();
        assert_eq!((d.max_luminance, d.min_luminance), (4000.0, 0.005));
        // red is the third primary in the box
        assert!((d.primaries[0].0 - 0.708).abs() < 1e-4 && (d.primaries[1].1 - 0.797).abs() < 1e-4 && (d.primaries[2].0 - 0.131).abs() < 1e-4);
        assert!((d.white_point.1 - 0.329).abs() < 1e-4);
    }

    #[test]
    fn full_range_nclx_and_nclc() {
        assert_eq!(visual_meta(&nclx(1, 1, 1, true)).color, Some(ColorInfo { primaries: 1, transfer: 1, matrix: 1, full_range: true }));
        let mut c = b"nclc".to_vec();
        for v in [9u16, 18, 9] {
            c.extend_from_slice(&v.to_be_bytes());
        }
        let m = visual_meta(&bx(b"colr", &c));
        assert_eq!(m.color, Some(ColorInfo { primaries: 9, transfer: 18, matrix: 9, full_range: false }));
        assert!(m.hdr.is_none());
    }

    #[test]
    fn damaged_boxes_are_skipped() {
        let kids = [bx(b"colr", b"nclx\0\x09"), bx(b"mdcv", &[0; 10]), bx(b"clli", &[0, 0, 0, 0]), bx(b"colr", b"prof")].concat();
        let m = visual_meta(&kids);
        assert!(m.color.is_none() && m.hdr.is_none());
        // a box that claims to run past the end stops the walk
        assert!(visual_meta(&[0, 0, 1, 0, b'c', b'o', b'l', b'r']).color.is_none());
        assert!(visual_meta(&[]).color.is_none());
    }
}
