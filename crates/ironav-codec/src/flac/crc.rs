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
/// `T[k][b]` is the CRC-16 of byte `b` followed by `k` zero bytes, which lets four bytes be
/// folded in with four independent lookups instead of one dependent chain.
const fn tables16x4() -> [[u16; 256]; 4] {
    let t0 = table16();
    let mut t = [[0u16; 256]; 4];
    let mut b = 0;
    while b < 256 {
        let mut c = t0[b];
        t[0][b] = c;
        let mut k = 1;
        while k < 4 {
            c = (c << 8) ^ t0[(c >> 8) as usize];
            t[k][b] = c;
            k += 1;
        }
        b += 1;
    }
    t
}

static T16: [[u16; 256]; 4] = tables16x4();

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| T8[(c ^ b) as usize])
}

#[inline]
pub fn crc16_update(crc: u16, byte: u8) -> u16 {
    (crc << 8) ^ T16[0][((crc >> 8) as u8 ^ byte) as usize]
}

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    let (chunks, rem) = data.as_chunks::<4>();
    for c in chunks {
        let x = crc ^ u16::from_be_bytes([c[0], c[1]]);
        crc = T16[3][(x >> 8) as usize] ^ T16[2][(x & 0xFF) as usize] ^ T16[1][c[2] as usize] ^ T16[0][c[3] as usize];
    }
    rem.iter().fold(crc, |c, &b| crc16_update(c, b))
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
        // The four-byte fast path must agree with the plain bytewise definition for every length.
        let data: Vec<u8> = (0..300u32).map(|i| (i * 37 + 11) as u8).collect();
        for n in 0..data.len() {
            assert_eq!(crc16(&data[..n]), data[..n].iter().fold(0u16, |c, &b| crc16_update(c, b)), "length {n}");
        }
        let mut h = vec![0xFF, 0xF8, 0xC9, 0x18, 0x00];
        h.push(crc8(&h));
        assert_eq!(crc8(&h), 0);
    }
}
