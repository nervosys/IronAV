//! HDR mastering metadata in SEI messages, shared by H.264 and HEVC: mastering display colour
//! volume (payload type 137, SMPTE ST 2086) and content light level info (type 144).

use ironav_core::{HdrMetadata, MasteringDisplay};

const MASTERING_DISPLAY: u32 = 137;
const CONTENT_LIGHT_LEVEL: u32 = 144;

/// Reads an SEI value of the form "0xFF bytes, then the last byte", summing them.
fn ff_value(d: &[u8], p: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    loop {
        let b = *d.get(*p)?;
        *p += 1;
        v = v.checked_add(b as u32)?;
        if b != 0xFF {
            return Some(v);
        }
    }
}

fn u16_at(d: &[u8], i: usize) -> u32 {
    u16::from_be_bytes([d[i], d[i + 1]]) as u32
}

fn u32_at(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

/// Reads the messages of an SEI RBSP (the NAL unit's bytes after its header, emulation prevention
/// removed) and records the HDR ones into `out`. A malformed or truncated message is ignored, and
/// so is whatever follows it: the stream still decodes.
pub fn parse_sei(rbsp: &[u8], out: &mut HdrMetadata) {
    let mut p = 0;
    // a message needs a type and a size, and the RBSP ends with the stop bit byte
    while p + 2 <= rbsp.len() {
        let Some(ty) = ff_value(rbsp, &mut p) else { return };
        let Some(size) = ff_value(rbsp, &mut p) else { return };
        let Some(payload) = rbsp.get(p..p.saturating_add(size as usize)) else { return };
        p += size as usize;
        match ty {
            MASTERING_DISPLAY if payload.len() >= 24 => {
                // green, blue, red primaries, in 0.00002; white point likewise; luminance in 0.0001 cd/m²
                let c = |i: usize| u16_at(payload, i) as f32 * 0.00002;
                let point = |i: usize| (c(i), c(i + 2));
                let (g, b, r) = (point(0), point(4), point(8));
                out.mastering = Some(MasteringDisplay {
                    primaries: [r, g, b],
                    white_point: point(12),
                    max_luminance: u32_at(payload, 16) as f32 * 0.0001,
                    min_luminance: u32_at(payload, 20) as f32 * 0.0001,
                });
            }
            CONTENT_LIGHT_LEVEL if payload.len() >= 4 => {
                out.max_cll = Some(u16_at(payload, 0)).filter(|&v| v > 0);
                out.max_fall = Some(u16_at(payload, 2)).filter(|&v| v > 0);
            }
            _ => {}
        }
    }
}

/// Builds an SEI RBSP of one message (with the 0xFF-extended type and size), for tests.
#[cfg(test)]
pub(crate) fn sei_message(ty: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    for mut n in [ty, payload.len() as u32] {
        while n >= 255 {
            v.push(0xFF);
            n -= 255;
        }
        v.push(n as u8);
    }
    v.extend_from_slice(payload);
    v
}

/// The payload of a mastering display colour volume message, in SEI order (G, B, R).
#[cfg(test)]
pub(crate) fn mdcv_payload(prim_gbr: [(u16, u16); 3], white: (u16, u16), max: u32, min: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for (x, y) in prim_gbr.into_iter().chain([white]) {
        v.extend_from_slice(&x.to_be_bytes());
        v.extend_from_slice(&y.to_be_bytes());
    }
    v.extend_from_slice(&max.to_be_bytes());
    v.extend_from_slice(&min.to_be_bytes());
    v
}

/// An Annex B NAL unit (start code, `header`, the RBSP plus its stop byte, with emulation
/// prevention) holding SEI messages, for splicing into a test stream.
#[cfg(test)]
pub(crate) fn sei_nal(header: &[u8], messages: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1];
    v.extend_from_slice(header);
    let mut zeros = 0;
    for &b in messages.concat().iter().chain([&0x80]) {
        if zeros >= 2 && b <= 3 {
            v.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        v.push(b);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BT.2020 primaries, D65, 1000 / 0.0001 nits: the usual HDR10 master.
    fn hdr10() -> Vec<u8> {
        mdcv_payload([(8500, 39850), (6550, 2300), (35400, 14600)], (15635, 16450), 10_000_000, 1)
    }

    #[test]
    fn mastering_display_and_light_level() {
        let mut rbsp = sei_message(137, &hdr10());
        rbsp.extend(sei_message(144, &[0x03, 0xE8, 0x01, 0x90]));
        rbsp.push(0x80);
        let mut m = HdrMetadata::default();
        parse_sei(&rbsp, &mut m);
        let d = m.mastering.unwrap();
        assert!((d.primaries[0].0 - 0.708).abs() < 1e-4 && (d.primaries[0].1 - 0.292).abs() < 1e-4, "red {:?}", d.primaries[0]);
        assert!((d.primaries[1].0 - 0.170).abs() < 1e-4 && (d.primaries[1].1 - 0.797).abs() < 1e-4);
        assert!((d.primaries[2].0 - 0.131).abs() < 1e-4 && (d.primaries[2].1 - 0.046).abs() < 1e-4);
        assert!((d.white_point.0 - 0.3127).abs() < 1e-4 && (d.white_point.1 - 0.3290).abs() < 1e-4);
        assert!((d.max_luminance - 1000.0).abs() < 1e-2 && (d.min_luminance - 0.0001).abs() < 1e-6);
        assert_eq!((m.max_cll, m.max_fall), (Some(1000), Some(400)));
    }

    #[test]
    fn extended_type_and_size() {
        // type 137 sent as 0xFF 0xFF ... is not 137; use an unknown 300-byte message before a real one
        let mut rbsp = sei_message(300, &[0u8; 300]);
        rbsp.extend(sei_message(144, &[0x10, 0x00, 0x00, 0x20]));
        let mut m = HdrMetadata::default();
        parse_sei(&rbsp, &mut m);
        assert_eq!((m.max_cll, m.max_fall), (Some(4096), Some(32)));
        // a message type above 255 is framed with 0xFF bytes
        assert_eq!(&sei_message(300, &[1])[..3], &[0xFF, 45, 1]);
    }

    #[test]
    fn zero_light_level_is_unknown() {
        let mut m = HdrMetadata::default();
        parse_sei(&sei_message(144, &[0, 0, 0, 0]), &mut m);
        assert!(m.is_empty());
    }

    #[test]
    fn malformed_messages_are_ignored() {
        let full = sei_message(137, &hdr10());
        // truncated payload
        let mut m = HdrMetadata::default();
        parse_sei(&full[..full.len() - 3], &mut m);
        assert!(m.is_empty());
        // a size that runs past the end, then a valid message that must not be read
        let mut bad = vec![144, 200, 1, 2];
        bad.extend(sei_message(144, &[0, 5, 0, 5]));
        parse_sei(&bad, &mut m);
        assert!(m.is_empty());
        // too short a payload for its type
        parse_sei(&sei_message(137, &[1, 2, 3]), &mut m);
        parse_sei(&sei_message(144, &[1, 2]), &mut m);
        assert!(m.is_empty());
        // only 0xFF bytes: no terminating byte
        parse_sei(&[0xFF, 0xFF, 0xFF], &mut m);
        parse_sei(&[], &mut m);
        parse_sei(&[0x80], &mut m);
        assert!(m.is_empty());
    }
}
