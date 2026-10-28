//! Bit writing for the encoder: RBSP bits, Exp-Golomb codes and NAL unit framing (ITU-T H.264 7.2, 7.4.1).

#[derive(Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    cur: u64,
    n: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter { buf: Vec::with_capacity(4096), cur: 0, n: 0 }
    }

    /// Writes the low `n` bits of `v` (n at most 32).
    pub fn put(&mut self, n: u32, v: u32) {
        debug_assert!(n <= 32 && (n == 32 || v >> n == 0));
        self.cur = (self.cur << n) | v as u64;
        self.n += n;
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.cur >> self.n) as u8);
        }
        self.cur &= (1u64 << self.n) - 1;
    }

    pub fn flag(&mut self, b: bool) {
        self.put(1, b as u32);
    }

    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        if len > 16 {
            self.put(len - 1, 0);
            self.put(len, x as u32);
        } else {
            self.put(2 * len - 1, x as u32);
        }
    }

    pub fn se(&mut self, v: i32) {
        self.ue(if v > 0 { 2 * v as u32 - 1 } else { (-2 * v as i64) as u32 });
    }

    /// Bits written so far.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.len() * 8 + self.n as usize
    }

    /// cabac_alignment_one_bit: ones to the byte boundary.
    pub fn align_ones(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(pad, (1 << pad) - 1);
        }
    }

    /// The bytes written, the last one padded with zeros.
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(pad, 0);
        }
        self.buf
    }

    /// rbsp_trailing_bits(): a one, then zeros to the byte boundary.
    pub fn finish(mut self) -> Vec<u8> {
        self.put(1, 1);
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(pad, 0);
        }
        self.buf
    }
}

/// Bits taken by ue(v) of `v`.
pub fn ue_bits(v: u32) -> u32 {
    2 * (32 - (v + 1).leading_zeros()) - 1
}

/// Bits taken by se(v) of `v`.
pub fn se_bits(v: i32) -> u32 {
    ue_bits(if v > 0 { 2 * v as u32 - 1 } else { (-2 * v as i64) as u32 })
}

/// Appends a NAL unit with a four-byte start code, inserting emulation prevention bytes.
pub fn put_nal(out: &mut Vec<u8>, ref_idc: u8, kind: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, (ref_idc << 5) | kind]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::bits::BitReader;

    #[test]
    fn exp_golomb_codes_read_back() {
        let mut w = BitWriter::new();
        let vals = [0u32, 1, 2, 3, 7, 254, 255, 65535, 1 << 20];
        let svals = [0i32, 1, -1, 2, -2, 1000, -1000];
        for &v in &vals {
            w.ue(v);
        }
        for &v in &svals {
            w.se(v);
        }
        w.put(5, 0b10110);
        let bits = w.len();
        let data = w.finish();
        let mut r = BitReader::new(&data);
        for &v in &vals {
            assert_eq!(r.ue().unwrap(), v);
        }
        for &v in &svals {
            assert_eq!(r.se().unwrap(), v);
        }
        assert_eq!(r.bits(5).unwrap(), 0b10110);
        assert_eq!(r.pos(), bits);
        let expect: u32 = vals.iter().map(|&v| ue_bits(v)).sum::<u32>() + svals.iter().map(|&v| se_bits(v)).sum::<u32>() + 5;
        assert_eq!(bits as u32, expect);
    }

    #[test]
    fn emulation_prevention_is_inserted() {
        let mut out = Vec::new();
        put_nal(&mut out, 3, 5, &[0, 0, 0, 0, 0, 1, 0, 0, 3, 7]);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 3, 7]);
        assert_eq!(crate::h264::bits::unescape(&out[5..]), [0, 0, 0, 0, 0, 1, 0, 0, 3, 7]);
    }
}
