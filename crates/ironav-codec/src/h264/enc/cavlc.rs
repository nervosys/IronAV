//! CAVLC residual block writing (ITU-T H.264 7.3.5.3.2, 9.2), the inverse of the decoder's parser.

use super::bitw::BitWriter;
use crate::h264::tables::*;

fn put_code(w: &mut BitWriter, len: u8, code: u8) {
    debug_assert!(len > 0, "a CAVLC symbol without a code");
    w.put(len as u32, code as u32);
}

/// Writes one level_prefix / level_suffix pair for `code` (levelCode) at `suffix_len`.
fn put_level(w: &mut BitWriter, code: u32, suffix_len: u32) {
    // the escape: prefix 15 with a 12-bit suffix, or longer prefixes with longer suffixes
    let escape = |w: &mut BitWriter, rem: u32| {
        if rem < 4096 {
            w.put(16, 1); // prefix 15
            w.put(12, rem);
        } else {
            let mut p = 16;
            while rem + 4096 >= 1 << (p - 2) {
                p += 1;
            }
            w.put(p, 0);
            w.put(1, 1);
            w.put(p - 3, rem + 4096 - (1 << (p - 3)));
        }
    };
    if suffix_len == 0 {
        if code < 14 {
            w.put(code + 1, 1);
        } else if code < 30 {
            w.put(15, 1); // prefix 14
            w.put(4, code - 14);
        } else {
            escape(w, code - 30);
        }
    } else if code >> suffix_len < 15 {
        w.put((code >> suffix_len) + 1, 1);
        w.put(suffix_len, code & ((1 << suffix_len) - 1));
    } else {
        escape(w, code - (15 << suffix_len));
    }
}

/// Writes one residual block. `levels` are in scan order (`levels.len()` is maxNumCoeff: 16, 15 or
/// 4); `nc` is the neighbour context (-1 for 4:2:0 chroma DC). Returns the number of non-zero levels.
pub fn write_block(w: &mut BitWriter, nc: i32, levels: &[i32]) -> usize {
    let max = levels.len();
    // non-zero levels from the highest frequency down, with the zeros before each
    let mut nz = [0i32; 16];
    let mut runs = [0usize; 16];
    let mut total = 0;
    let mut last = None;
    for (i, &l) in levels.iter().enumerate().rev() {
        if l != 0 {
            if let Some(prev) = last {
                runs[total - 1] = prev - i - 1;
            }
            nz[total] = l;
            total += 1;
            last = Some(i);
        }
    }
    let mut ones = 0;
    while ones < total.min(3) && nz[ones].abs() == 1 {
        ones += 1;
    }
    let token = total * 4 + ones;
    if nc == -1 {
        put_code(w, CHROMA_DC_COEFF_TOKEN_LEN[token], CHROMA_DC_COEFF_TOKEN_BITS[token]);
    } else {
        let class = match nc {
            0..=1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            _ => 3,
        };
        put_code(w, COEFF_TOKEN_LEN[class][token], COEFF_TOKEN_BITS[class][token]);
    }
    if total == 0 {
        return 0;
    }
    for &l in &nz[..ones] {
        w.flag(l < 0);
    }
    let mut suffix_len = if total > 10 && ones < 3 { 1 } else { 0 };
    for (i, &l) in nz.iter().enumerate().take(total).skip(ones) {
        let mut code = if l > 0 { 2 * l as u32 - 2 } else { (-2 * l) as u32 - 1 };
        if i == ones && ones < 3 {
            code -= 2;
        }
        put_level(w, code, suffix_len);
        if suffix_len == 0 {
            suffix_len = 1;
        }
        if l.unsigned_abs() > (3 << (suffix_len - 1)) && suffix_len < 6 {
            suffix_len += 1;
        }
    }
    let highest = levels.iter().rposition(|&l| l != 0).unwrap();
    let mut zeros_left = highest + 1 - total;
    if total < max {
        let tz = zeros_left;
        if nc == -1 {
            put_code(w, CHROMA_DC_TOTAL_ZEROS_LEN[total - 1][tz], CHROMA_DC_TOTAL_ZEROS_BITS[total - 1][tz]);
        } else {
            put_code(w, TOTAL_ZEROS_LEN[total - 1][tz], TOTAL_ZEROS_BITS[total - 1][tz]);
        }
    }
    // the gap between the last nonzero and the next lower, for all but the lowest
    for &run in runs.iter().take(total - 1) {
        if zeros_left == 0 {
            break;
        }
        put_code(w, RUN_LEN[zeros_left.min(7) - 1][run], RUN_BITS[zeros_left.min(7) - 1][run]);
        zeros_left -= run;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::bits::BitReader;
    use crate::h264::cavlc::residual_block;

    fn round_trip(nc: i32, levels: &[i32]) {
        let mut w = BitWriter::new();
        let n = write_block(&mut w, nc, levels);
        w.put(8, 0xA5);
        let data = w.finish();
        let mut r = BitReader::new(&data);
        let mut out = [0i32; 16];
        let got = residual_block(&mut r, nc, levels.len(), &mut out).unwrap();
        assert_eq!(got, n, "{levels:?}");
        assert_eq!(&out[..levels.len()], levels, "nc {nc}");
        assert_eq!(r.bits(8).unwrap(), 0xA5, "{levels:?} leaves the reader where the writer stopped");
    }

    #[test]
    fn blocks_read_back_through_the_decoder() {
        let mut seed = 12345u32;
        let mut rnd = |m: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % m
        };
        for nc in [0, 1, 2, 3, 5, 7, 8, 16] {
            for len in [16, 15] {
                round_trip(nc, &vec![0; len]);
                for _ in 0..300 {
                    let density = 1 + rnd(16);
                    let big = rnd(4);
                    let levels: Vec<i32> = (0..len)
                        .map(|_| {
                            if rnd(16) >= density {
                                return 0;
                            }
                            let m = match big {
                                0 => 2,
                                1 => 8,
                                2 => 100,
                                _ => 9000,
                            };
                            let v = 1 + rnd(m) as i32;
                            if rnd(2) == 0 { v } else { -v }
                        })
                        .collect();
                    round_trip(nc, &levels);
                }
            }
        }
        for _ in 0..500 {
            let levels: Vec<i32> = (0..4).map(|_| rnd(7) as i32 - 3).collect();
            round_trip(-1, &levels);
        }
        round_trip(0, &[1, -1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        round_trip(0, &[30000; 16]);
    }
}
