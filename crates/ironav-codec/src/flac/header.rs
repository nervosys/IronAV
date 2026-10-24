//! FLAC frame header parsing (RFC 9639 §9.1). Shared by the decoder and by the container
//! demuxer, which uses it to validate candidate frame boundaries.

use super::crc::crc8;

/// Longest possible frame header, in bytes.
pub const MAX_HEADER_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    /// 1..=8 channels coded separately.
    Independent(u8),
    LeftSide,
    SideRight,
    MidSide,
}

impl Assignment {
    pub fn channels(self) -> usize {
        match self {
            Self::Independent(n) => n as usize,
            _ => 2,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub block_size: u32,
    /// `None` means "take it from STREAMINFO".
    pub sample_rate: Option<u32>,
    pub assignment: Assignment,
    /// `None` means "take it from STREAMINFO".
    pub bits_per_sample: Option<u8>,
    pub variable_blocksize: bool,
    /// Frame number (fixed block size) or first-sample number (variable block size).
    pub number: u64,
    /// Header length in bytes, including its CRC-8.
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// More bytes are needed to decide.
    Incomplete,
    Invalid(&'static str),
}

pub fn parse_header(d: &[u8]) -> Result<FrameHeader, HeaderError> {
    use HeaderError::*;
    let get = |i: usize| d.get(i).copied().ok_or(Incomplete);
    if get(0)? != 0xFF {
        return Err(Invalid("bad sync code"));
    }
    let b1 = get(1)?;
    if b1 & 0xFE != 0xF8 {
        return Err(Invalid("bad sync code"));
    }
    let variable = b1 & 1 == 1;
    let (b2, b3) = (get(2)?, get(3)?);
    let (bs_code, sr_code) = (b2 >> 4, b2 & 15);
    let (ch_code, size_code) = (b3 >> 4, (b3 >> 1) & 7);
    if b3 & 1 != 0 {
        return Err(Invalid("reserved bit set"));
    }
    let assignment = match ch_code {
        0..=7 => Assignment::Independent(ch_code + 1),
        8 => Assignment::LeftSide,
        9 => Assignment::SideRight,
        10 => Assignment::MidSide,
        _ => return Err(Invalid("reserved channel assignment")),
    };
    let bits_per_sample = match size_code {
        0 => None,
        1 => Some(8),
        2 => Some(12),
        4 => Some(16),
        5 => Some(20),
        6 => Some(24),
        7 => Some(32),
        _ => return Err(Invalid("reserved sample size")),
    };

    let mut p = 4;
    let b0 = get(p)?;
    let n = b0.leading_ones() as usize;
    let number = match n {
        0 => {
            p += 1;
            b0 as u64
        }
        2..=7 => {
            let mut v = (b0 as u64) & (0x7F >> n);
            for i in 1..n {
                let c = get(p + i)?;
                if c & 0xC0 != 0x80 {
                    return Err(Invalid("bad UTF-8 frame number"));
                }
                v = (v << 6) | (c & 0x3F) as u64;
            }
            p += n;
            v
        }
        _ => return Err(Invalid("bad UTF-8 frame number")),
    };

    let block_size = match bs_code {
        0 => return Err(Invalid("reserved block size")),
        1 => 192,
        2..=5 => 576u32 << (bs_code - 2),
        6 => {
            let v = get(p)? as u32 + 1;
            p += 1;
            v
        }
        7 => {
            let v = u16::from_be_bytes([get(p)?, get(p + 1)?]) as u32 + 1;
            p += 2;
            v
        }
        _ => 256u32 << (bs_code - 8),
    };

    let sample_rate = match sr_code {
        0 => None,
        1 => Some(88_200),
        2 => Some(176_400),
        3 => Some(192_000),
        4 => Some(8_000),
        5 => Some(16_000),
        6 => Some(22_050),
        7 => Some(24_000),
        8 => Some(32_000),
        9 => Some(44_100),
        10 => Some(48_000),
        11 => Some(96_000),
        12 => {
            let v = get(p)? as u32 * 1000;
            p += 1;
            Some(v)
        }
        13 => {
            let v = u16::from_be_bytes([get(p)?, get(p + 1)?]) as u32;
            p += 2;
            Some(v)
        }
        14 => {
            let v = u16::from_be_bytes([get(p)?, get(p + 1)?]) as u32 * 10;
            p += 2;
            Some(v)
        }
        _ => return Err(Invalid("reserved sample rate")),
    };

    if get(p)? != crc8(&d[..p]) {
        return Err(Invalid("header CRC mismatch"));
    }
    Ok(FrameHeader { block_size, sample_rate, assignment, bits_per_sample, variable_blocksize: variable, number, len: p + 1 })
}

/// UTF-8-style coding of a frame number, as used in FLAC frame headers.
pub fn write_utf8_number(out: &mut Vec<u8>, v: u64) {
    if v < 0x80 {
        out.push(v as u8);
        return;
    }
    // An n-byte sequence carries (7 - n) + 6*(n - 1) payload bits: 11, 16, 21, 26, 31, 36.
    let mut n = 2usize;
    while n < 7 && v >= 1u64 << ((7 - n) + 6 * (n - 1)) {
        n += 1;
    }
    out.push(!(0xFFu8 >> n) | (v >> (6 * (n - 1))) as u8);
    for i in (0..n - 1).rev() {
        out.push(0x80 | ((v >> (6 * i)) & 0x3F) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_numbers_roundtrip() {
        for v in [0u64, 1, 127, 128, 2047, 2048, 65535, 65536, 0x1F_FFFF, 0x20_0000, 0x3FF_FFFF, 0x400_0000, 0xF_FFFF_FFFF] {
            let mut d = vec![0xFF, 0xF8, 0xC9, 0x18];
            write_utf8_number(&mut d, v);
            let c = crc8(&d);
            d.push(c);
            let h = parse_header(&d).unwrap_or_else(|e| panic!("{v}: {e:?}"));
            assert_eq!(h.number, v);
            assert_eq!(h.len, d.len());
        }
    }

    #[test]
    fn rejects_garbage_and_asks_for_more_data() {
        assert_eq!(parse_header(&[0x12, 0x34, 0, 0, 0]).unwrap_err(), HeaderError::Invalid("bad sync code"));
        assert_eq!(parse_header(&[0xFF, 0xF8]).unwrap_err(), HeaderError::Incomplete);
    }
}
