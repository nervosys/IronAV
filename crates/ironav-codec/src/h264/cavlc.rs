//! CAVLC residual block parsing (ITU-T H.264 9.2).

use super::bits::BitReader;
use super::tables::*;
use ironav_core::{Error, Result};
use std::sync::OnceLock;

/// A prefix-code lookup table indexed by the next `bits` bits of the stream.
struct Vlc {
    bits: u32,
    t: Vec<u16>,
}

impl Vlc {
    /// `lens[v]` and `codes[v]` give the length and value of the code for symbol `v`; a length of 0
    /// means the symbol has no code.
    fn build(lens: &[u8], codes: &[u8]) -> Vlc {
        let bits = lens.iter().copied().max().unwrap_or(1) as u32;
        let mut t = vec![0u16; 1 << bits];
        for (v, (&l, &c)) in lens.iter().zip(codes).enumerate() {
            if l == 0 {
                continue;
            }
            let l = l as u32;
            let base = (c as usize) << (bits - l);
            for k in 0..1usize << (bits - l) {
                t[base + k] = ((l as u16) << 8) | v as u16;
            }
        }
        Vlc { bits, t }
    }

    fn read(&self, r: &mut BitReader) -> Result<usize> {
        let e = self.t[r.peek(self.bits) as usize];
        if e == 0 {
            return Err(Error::invalid_input("H.264 CAVLC data holds an invalid code"));
        }
        r.skip((e >> 8) as usize);
        Ok((e & 255) as usize)
    }
}

struct Tables {
    coeff_token: [Vlc; 4],
    chroma_dc_token: Vlc,
    total_zeros: Vec<Vlc>,
    chroma_dc_total_zeros: Vec<Vlc>,
    run_before: Vec<Vlc>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| Tables {
        coeff_token: [0, 1, 2, 3].map(|i| Vlc::build(&COEFF_TOKEN_LEN[i], &COEFF_TOKEN_BITS[i])),
        chroma_dc_token: Vlc::build(&CHROMA_DC_COEFF_TOKEN_LEN, &CHROMA_DC_COEFF_TOKEN_BITS),
        total_zeros: (0..15).map(|i| Vlc::build(&TOTAL_ZEROS_LEN[i], &TOTAL_ZEROS_BITS[i])).collect(),
        chroma_dc_total_zeros: (0..3).map(|i| Vlc::build(&CHROMA_DC_TOTAL_ZEROS_LEN[i], &CHROMA_DC_TOTAL_ZEROS_BITS[i])).collect(),
        run_before: (0..7).map(|i| Vlc::build(&RUN_LEN[i], &RUN_BITS[i])).collect(),
    })
}

/// Parses one residual block. `nc` is the neighbour-derived context (-1 for chroma DC) and
/// `max_coef` the number of coefficients the block can hold (16, 15 or 4). The levels are written
/// to `out` in scan order; the return value is the number of non-zero coefficients.
pub fn residual_block(r: &mut BitReader, nc: i32, max_coef: usize, out: &mut [i32; 16]) -> Result<usize> {
    let t = tables();
    out.fill(0);
    let token = if nc == -1 {
        t.chroma_dc_token.read(r)?
    } else {
        let class = match nc {
            0..=1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            _ => 3,
        };
        t.coeff_token[class].read(r)?
    };
    let (total, ones) = (token >> 2, token & 3);
    if total == 0 {
        return Ok(0);
    }
    if total > max_coef {
        return Err(Error::invalid_input("H.264 block holds more coefficients than it can"));
    }
    let mut levels = [0i32; 16];
    for l in levels.iter_mut().take(ones) {
        *l = 1 - 2 * r.bit()? as i32;
    }
    let mut suffix_len = if total > 10 && ones < 3 { 1 } else { 0 };
    for i in ones..total {
        let mut prefix = 0u32;
        while r.bit()? == 0 {
            prefix += 1;
            if prefix > 25 {
                return Err(Error::invalid_input("H.264 level prefix is too long"));
            }
        }
        let mut code = (prefix.min(15) << suffix_len) as i32;
        if suffix_len > 0 || prefix >= 14 {
            let size = if prefix == 14 && suffix_len == 0 {
                4
            } else if prefix >= 15 {
                prefix - 3
            } else {
                suffix_len
            };
            if size > 0 {
                code += r.bits(size)? as i32;
            }
        }
        if prefix >= 15 && suffix_len == 0 {
            code += 15;
        }
        if prefix >= 16 {
            code += (1 << (prefix - 3)) - 4096;
        }
        if i == ones && ones < 3 {
            code += 2;
        }
        let level = if code % 2 == 0 { (code + 2) >> 1 } else { (-code - 1) >> 1 };
        levels[i] = level;
        if suffix_len == 0 {
            suffix_len = 1;
        }
        if level.abs() > (3 << (suffix_len - 1)) && suffix_len < 6 {
            suffix_len += 1;
        }
    }
    let mut zeros_left = if total < max_coef {
        if nc == -1 {
            t.chroma_dc_total_zeros[total - 1].read(r)?
        } else {
            t.total_zeros[total - 1].read(r)?
        }
    } else {
        0
    };
    if total + zeros_left > max_coef {
        return Err(Error::invalid_input("H.264 block has more zeros than fit"));
    }
    let mut pos = total + zeros_left - 1;
    for i in 0..total {
        out[pos] = levels[i];
        if i + 1 < total {
            let run = if zeros_left > 0 { t.run_before[zeros_left.min(7) - 1].read(r)? } else { 0 };
            if run > zeros_left {
                return Err(Error::invalid_input("H.264 run_before exceeds the zeros left"));
            }
            zeros_left -= run;
            pos -= 1 + run;
        }
    }
    Ok(total)
}
