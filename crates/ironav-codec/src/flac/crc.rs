//! CRC-8 (poly 0x07) over frame headers and CRC-16 (poly 0x8005) over whole frames,
//! both MSB-first with zero initial value, as specified for FLAC.

const fn table8() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn table16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static T8: [u8; 256] = table8();
static T16: [u16; 256] = table16();

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| T8[(c ^ b) as usize])
}

#[inline]
pub fn crc16_update(crc: u16, byte: u8) -> u16 {
    (crc << 8) ^ T16[((crc >> 8) as u8 ^ byte) as usize]
}

pub fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |c, &b| crc16_update(c, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_the_crc_yields_zero() {
        let mut d = b"hello flac frame".to_vec();
        let c = crc16(&d);
        d.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc16(&d), 0);
        let mut h = vec![0xFF, 0xF8, 0xC9, 0x18, 0x00];
        h.push(crc8(&h));
        assert_eq!(crc8(&h), 0);
    }
}
