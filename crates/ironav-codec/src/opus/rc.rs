//! The Opus range decoder (RFC 6716 section 4.1).
//!
//! Range-coded symbols are read from the front of a frame, raw bits from its back; the two meet in
//! the middle. Bytes past the end of the data read as zero.

pub struct RangeDecoder<'a> {
    data: &'a [u8],
    /// Next bit to read from the front.
    bit_pos: usize,
    range: u32,
    value: u32,
    total_bits: u32,
    /// Raw bits are taken from the end of the frame, a byte at a time, backwards.
    raw_end: usize,
    raw_bytes: u32,
    raw_cache_len: u32,
    raw_cache: u32,
}

const TOP: u32 = 1 << 31;
const BOT: u32 = TOP >> 8;

fn log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// Number of bits needed to write `x` (0 for 0).
pub fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

pub fn isqrt(x: u32) -> u32 {
    let mut r = (x as f64).sqrt() as u32;
    while r * r > x {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= x {
        r += 1;
    }
    r
}

impl<'a> RangeDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut rc = Self {
            data,
            bit_pos: 0,
            range: 128,
            value: 0,
            total_bits: 9,
            raw_end: data.len(),
            raw_bytes: data.len() as u32,
            raw_cache_len: 0,
            raw_cache: 0,
        };
        rc.value = 127 - rc.bits(7);
        rc.normalize();
        rc
    }

    /// Start taking raw bits from `bytes` bytes ending at `end` (an index into the frame).
    pub fn set_raw(&mut self, end: usize, bytes: usize) {
        self.raw_end = end;
        self.raw_bytes = bytes as u32;
        self.raw_cache_len = 0;
        self.raw_cache = 0;
    }

    /// The frame's size in bits as the raw-bits side sees it.
    pub fn frame_bits(&self) -> i32 {
        (self.raw_bytes * 8) as i32
    }

    /// Account for `n` bits as consumed without reading them (a silent frame).
    pub fn skip_bits(&mut self, n: u32) {
        self.total_bits += n;
    }

    pub fn range(&self) -> u32 {
        self.range
    }

    fn bits(&mut self, n: u32) -> u32 {
        let byte = self.bit_pos >> 3;
        let off = (self.bit_pos & 7) as u32;
        let at = |i: usize| self.data.get(i).copied().unwrap_or(0) as u32;
        let w = at(byte) << 8 | at(byte + 1);
        self.bit_pos += n as usize;
        (w >> (16 - off - n)) & ((1 << n) - 1)
    }

    fn normalize(&mut self) {
        while self.range <= BOT {
            self.value = ((self.value << 8) | (self.bits(8) ^ 0xFF)) & (TOP - 1);
            self.range <<= 8;
            self.total_bits += 8;
        }
    }

    fn update(&mut self, scale: u32, low: u32, high: u32, total: u32) {
        self.value -= scale * (total - high);
        self.range = if low != 0 { scale * (high - low) } else { self.range - scale * (total - high) };
        self.normalize();
    }

    /// Whole bits used so far.
    pub fn tell(&self) -> i32 {
        (self.total_bits - log2(self.range) - 1) as i32
    }

    /// Bits used so far in eighths.
    pub fn tell_frac(&self) -> i32 {
        tell_frac_of(self.total_bits, self.range)
    }

    /// Decode a symbol with the cumulative distribution `cdf` (`cdf[0]` is the total).
    pub fn cdf(&mut self, cdf: &[u16]) -> usize {
        let total = cdf[0] as u32;
        let scale = self.range / total;
        let symbol = self.value / scale + 1;
        let symbol = total - symbol.min(total);
        let cdf = &cdf[1..];
        let mut k = 0;
        while (cdf[k] as u32) <= symbol {
            k += 1;
        }
        let high = cdf[k] as u32;
        let low = if k > 0 { cdf[k - 1] as u32 } else { 0 };
        self.update(scale, low, high, total);
        k
    }

    /// A bit that is set with probability `2^-bits`.
    pub fn log(&mut self, bits: u32) -> u32 {
        let scale = self.range >> bits;
        let k = if self.value >= scale {
            self.value -= scale;
            self.range -= scale;
            0
        } else {
            self.range = scale;
            1
        };
        self.normalize();
        k
    }

    /// 0 to 25 raw bits from the end of the frame.
    pub fn raw(&mut self, count: u32) -> u32 {
        while self.raw_bytes > 0 && self.raw_cache_len < count {
            self.raw_end -= 1;
            let b = self.data.get(self.raw_end).copied().unwrap_or(0) as u32;
            self.raw_cache |= b << self.raw_cache_len;
            self.raw_cache_len += 8;
            self.raw_bytes -= 1;
        }
        let value = if count >= 32 { self.raw_cache } else { self.raw_cache & ((1u32 << count) - 1) };
        self.raw_cache = if count >= 32 { 0 } else { self.raw_cache >> count };
        self.raw_cache_len = self.raw_cache_len.wrapping_sub(count);
        self.total_bits += count;
        value
    }

    /// A uniformly distributed integer in `0..size`.
    pub fn uint(&mut self, size: u32) -> u32 {
        let bits = ilog(size - 1);
        let total = if bits > 8 { ((size - 1) >> (bits - 8)) + 1 } else { size };
        let scale = self.range / total;
        let k = self.value / scale + 1;
        let k = total - k.min(total);
        self.update(scale, k, k + 1, total);
        if bits > 8 {
            let k = k << (bits - 8) | self.raw(bits - 8);
            k.min(size - 1)
        } else {
            k
        }
    }

    /// CELT's stereo angle: probability 3 up to `k0`, then 1.
    pub fn uint_step(&mut self, k0: u32) -> u32 {
        let total = (k0 + 1) * 3 + k0;
        let scale = self.range / total;
        let symbol = self.value / scale + 1;
        let symbol = total - symbol.min(total);
        let k = if symbol < (k0 + 1) * 3 { symbol / 3 } else { symbol - (k0 + 1) * 2 };
        let (low, high) = if k <= k0 { (3 * k, 3 * (k + 1)) } else { (k - 1 - k0 + 3 * (k0 + 1), k - k0 + 3 * (k0 + 1)) };
        self.update(scale, low, high, total);
        k
    }

    /// CELT's angle with a triangular distribution.
    pub fn uint_tri(&mut self, qn: u32) -> u32 {
        let total = ((qn >> 1) + 1) * ((qn >> 1) + 1);
        let scale = self.range / total;
        let center = self.value / scale + 1;
        let center = total - center.min(total);
        let (k, low, symbol);
        if center < total >> 1 {
            k = (isqrt(8 * center + 1) - 1) >> 1;
            low = k * (k + 1) >> 1;
            symbol = k + 1;
        } else {
            k = (2 * (qn + 1) - isqrt(8 * (total - center - 1) + 1)) >> 1;
            low = total - ((qn + 1 - k) * (qn + 2 - k) >> 1);
            symbol = qn + 1 - k;
        }
        self.update(scale, low, low + symbol, total);
        k
    }

    /// A Laplace-distributed integer (CELT's coarse band energy).
    pub fn laplace(&mut self, symbol: u32, decay: u32) -> i32 {
        let mut symbol = symbol;
        let mut value = 0i32;
        let mut low = 0u32;
        let scale = self.range >> 15;
        let center = self.value / scale + 1;
        let center = (1 << 15) - center.min(1 << 15);
        if center >= symbol {
            value += 1;
            low = symbol;
            symbol = 1 + (((32768 - 32 - symbol) * (16384 - decay)) >> 15);
            while symbol > 1 && center >= low + 2 * symbol {
                value += 1;
                symbol *= 2;
                low += symbol;
                symbol = (((symbol - 2) * decay) >> 15) + 1;
            }
            if symbol <= 1 {
                let distance = (center - low) >> 1;
                value += distance as i32;
                low += 2 * distance;
            }
            if center < low + symbol {
                value = -value;
            } else {
                low += symbol;
            }
        }
        self.update(scale, low, (low + symbol).min(32768), 32768);
        value
    }
}

/// What the CELT layer needs from a range coder, so that one body of code decodes a frame and
/// encodes it. Every method takes the value to write; a decoder ignores it and returns what it read,
/// an encoder writes it and returns it (possibly clamped).
pub trait Coder {
    /// Whether this coder writes.
    const ENC: bool;
    fn tell(&self) -> i32;
    fn tell_frac(&self) -> i32;
    fn cdf(&mut self, cdf: &[u16], want: usize) -> usize;
    fn log(&mut self, bits: u32, want: u32) -> u32;
    fn raw(&mut self, count: u32, want: u32) -> u32;
    fn uint(&mut self, size: u32, want: u32) -> u32;
    fn uint_step(&mut self, k0: u32, want: u32) -> u32;
    fn uint_tri(&mut self, qn: u32, want: u32) -> u32;
}

impl Coder for RangeDecoder<'_> {
    const ENC: bool = false;
    fn tell(&self) -> i32 {
        RangeDecoder::tell(self)
    }
    fn tell_frac(&self) -> i32 {
        RangeDecoder::tell_frac(self)
    }
    fn cdf(&mut self, cdf: &[u16], _: usize) -> usize {
        RangeDecoder::cdf(self, cdf)
    }
    fn log(&mut self, bits: u32, _: u32) -> u32 {
        RangeDecoder::log(self, bits)
    }
    fn raw(&mut self, count: u32, _: u32) -> u32 {
        RangeDecoder::raw(self, count)
    }
    fn uint(&mut self, size: u32, _: u32) -> u32 {
        RangeDecoder::uint(self, size)
    }
    fn uint_step(&mut self, k0: u32, _: u32) -> u32 {
        RangeDecoder::uint_step(self, k0)
    }
    fn uint_tri(&mut self, qn: u32, _: u32) -> u32 {
        RangeDecoder::uint_tri(self, qn)
    }
}

/// The Opus range encoder (RFC 6716 section 5.1): range-coded symbols go to the front of a
/// fixed-size frame, raw bits to its back.
pub struct RangeEncoder {
    buf: Vec<u8>,
    offs: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: u32,
    total_bits: u32,
    range: u32,
    value: u32,
    /// The last byte not yet written, which a carry may still change (-1 for none).
    rem: i32,
    /// Bytes of 0xFF waiting behind `rem`.
    ext: u32,
    error: bool,
}

impl RangeEncoder {
    /// An encoder for a frame of exactly `size` bytes.
    pub fn new(size: usize) -> Self {
        Self { buf: vec![0; size], offs: 0, end_offs: 0, end_window: 0, nend_bits: 0, total_bits: 33, range: TOP, value: 0, rem: -1, ext: 0, error: false }
    }

    /// The frame's size in bits.
    pub fn frame_bits(&self) -> i32 {
        (self.buf.len() * 8) as i32
    }

    fn write_byte(&mut self, b: u32) {
        if self.offs + self.end_offs >= self.buf.len() {
            self.error = true;
            return;
        }
        self.buf[self.offs] = b as u8;
        self.offs += 1;
    }

    fn write_byte_at_end(&mut self, b: u32) {
        if self.offs + self.end_offs >= self.buf.len() {
            self.error = true;
            return;
        }
        self.end_offs += 1;
        let at = self.buf.len() - self.end_offs;
        self.buf[at] = b as u8;
    }

    fn carry_out(&mut self, c: i32) {
        if c != 0xFF {
            let carry = c >> 8;
            if self.rem >= 0 {
                self.write_byte((self.rem + carry) as u32);
            }
            if self.ext > 0 {
                let sym = ((0xFF + carry) & 0xFF) as u32;
                for _ in 0..self.ext {
                    self.write_byte(sym);
                }
                self.ext = 0;
            }
            self.rem = c & 0xFF;
        } else {
            self.ext += 1;
        }
    }

    fn normalize(&mut self) {
        while self.range <= BOT {
            self.carry_out((self.value >> 23) as i32);
            self.value = (self.value << 8) & (TOP - 1);
            self.range <<= 8;
            self.total_bits += 8;
        }
    }

    /// Code the symbol occupying `low..high` of `total`.
    pub fn encode(&mut self, low: u32, high: u32, total: u32) {
        let r = self.range / total;
        if low > 0 {
            self.value = self.value.wrapping_add(self.range - r * (total - low));
            self.range = r * (high - low);
        } else {
            self.range -= r * (total - high);
        }
        self.normalize();
    }

    fn encode_bin(&mut self, low: u32, high: u32, bits: u32) {
        let r = self.range >> bits;
        if low > 0 {
            self.value = self.value.wrapping_add(self.range - r * ((1 << bits) - low));
            self.range = r * (high - low);
        } else {
            self.range -= r * ((1 << bits) - high);
        }
        self.normalize();
    }

    /// A Laplace-distributed integer (see `RangeDecoder::laplace`); returns the value as coded,
    /// which is clamped when it is too large to represent.
    pub fn laplace(&mut self, value: i32, fs: u32, decay: u32) -> i32 {
        let mut fs = fs as i32;
        let mut low = 0i32;
        let mut out = value;
        if value != 0 {
            let s: i32 = if value < 0 { -1 } else { 0 };
            let val = value.abs();
            low = fs;
            fs = (((32768 - 32 - fs) as i64 * (16384 - decay as i64)) >> 15) as i32;
            let mut i = 1;
            while fs > 0 && i < val {
                fs *= 2;
                low += fs + 2;
                fs = (fs * decay as i32) >> 15;
                i += 1;
            }
            if fs == 0 {
                // out of range: the value is too large to code exactly
                let ndi_max = ((32768 - low) - s) >> 1;
                let di = (val - i).min(ndi_max - 1);
                low += 2 * di + 1 + s;
                fs = 1.min(32768 - low);
                out = (i + di + s) ^ s;
            } else {
                fs += 1;
                low += fs & !s;
            }
        }
        self.encode_bin(low as u32, (low + fs) as u32, 15);
        out
    }

    /// Starts another frame of exactly `size` bytes in the same buffer, which allocates only when
    /// it is larger than any before.
    pub fn reset(&mut self, size: usize) {
        self.buf.clear();
        self.buf.resize(size, 0);
        (self.offs, self.end_offs, self.end_window, self.nend_bits, self.total_bits) = (0, 0, 0, 0, 33);
        (self.range, self.value, self.rem, self.ext, self.error) = (TOP, 0, -1, 0, false);
    }

    /// Makes the frame `size` bytes long, once its size is known: only before any raw bits, and
    /// never shorter than what is written.
    pub fn shrink(&mut self, size: usize) {
        debug_assert!(self.end_offs == 0 && self.nend_bits == 0 && self.offs <= size);
        self.buf.resize(size, 0);
    }

    /// Finish the frame: flush the range coder and merge in the raw bits. Fails if the frame
    /// overflowed.
    #[cfg(test)]
    pub fn finish(mut self) -> Result<Vec<u8>, ()> {
        self.finish_in_place()?;
        Ok(self.buf)
    }

    /// [`finish`](Self::finish), leaving the frame in [`bytes`](Self::bytes).
    pub fn finish_in_place(&mut self) -> Result<(), ()> {
        let mut l = 32 - ilog(self.range) as i32;
        let mut msk = (TOP - 1) >> l;
        let mut end = self.value.wrapping_add(msk) & !msk;
        if (end | msk) >= self.value.wrapping_add(self.range) {
            l += 1;
            msk >>= 1;
            end = self.value.wrapping_add(msk) & !msk;
        }
        while l > 0 {
            self.carry_out((end >> 23) as i32);
            end = (end << 8) & (TOP - 1);
            l -= 8;
        }
        if self.rem >= 0 || self.ext > 0 {
            self.carry_out(0);
        }
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        while used >= 8 {
            self.write_byte_at_end(window & 0xFF);
            window >>= 8;
            used -= 8;
        }
        if !self.error && used > 0 {
            if self.end_offs >= self.buf.len() {
                self.error = true;
            } else {
                let spare = (-l) as u32;
                if self.offs + self.end_offs >= self.buf.len() && spare < used {
                    window &= (1 << spare) - 1;
                    self.error = true;
                }
                let at = self.buf.len() - self.end_offs - 1;
                self.buf[at] |= window as u8;
            }
        }
        if self.error {
            Err(())
        } else {
            Ok(())
        }
    }

    /// The frame, once finished.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

impl Coder for RangeEncoder {
    const ENC: bool = true;
    fn tell(&self) -> i32 {
        (self.total_bits - ilog(self.range)) as i32
    }
    fn tell_frac(&self) -> i32 {
        tell_frac_of(self.total_bits, self.range)
    }
    fn cdf(&mut self, cdf: &[u16], want: usize) -> usize {
        let low = if want > 0 { cdf[want] as u32 } else { 0 };
        self.encode(low, cdf[want + 1] as u32, cdf[0] as u32);
        want
    }
    fn log(&mut self, bits: u32, want: u32) -> u32 {
        let s = self.range >> bits;
        let r = self.range - s;
        if want != 0 {
            self.value = self.value.wrapping_add(r);
            self.range = s;
        } else {
            self.range = r;
        }
        self.normalize();
        (want != 0) as u32
    }
    fn raw(&mut self, count: u32, want: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        if used + count > 32 {
            while used >= 8 {
                self.write_byte_at_end(window & 0xFF);
                window >>= 8;
                used -= 8;
            }
        }
        window |= want << used;
        used += count;
        self.end_window = window;
        self.nend_bits = used;
        self.total_bits += count;
        want
    }
    fn uint(&mut self, size: u32, want: u32) -> u32 {
        let ft = size - 1;
        let bits = ilog(ft);
        if bits > 8 {
            let shift = bits - 8;
            let ft1 = (ft >> shift) + 1;
            let top = want >> shift;
            self.encode(top, top + 1, ft1);
            Coder::raw(self, shift, want & ((1 << shift) - 1));
        } else {
            self.encode(want, want + 1, ft + 1);
        }
        want
    }
    fn uint_step(&mut self, k0: u32, k: u32) -> u32 {
        let total = (k0 + 1) * 3 + k0;
        let (low, high) = if k <= k0 { (3 * k, 3 * (k + 1)) } else { (k - 1 - k0 + 3 * (k0 + 1), k - k0 + 3 * (k0 + 1)) };
        self.encode(low, high, total);
        k
    }
    fn uint_tri(&mut self, qn: u32, k: u32) -> u32 {
        let total = ((qn >> 1) + 1) * ((qn >> 1) + 1);
        let (low, symbol) = if k <= qn >> 1 { (k * (k + 1) >> 1, k + 1) } else { (total - ((qn + 1 - k) * (qn + 2 - k) >> 1), qn + 1 - k) };
        self.encode(low, low + symbol, total);
        k
    }
}

fn tell_frac_of(total_bits: u32, range: u32) -> i32 {
    let total_bits = total_bits << 3;
    let mut rcbuffer = log2(range) + 1;
    let mut range = range >> (rcbuffer - 16);
    for _ in 0..3 {
        range = range * range >> 15;
        let bit = range >> 16;
        rcbuffer = rcbuffer << 1 | bit;
        range >>= bit;
    }
    (total_bits - rcbuffer) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator for the round-trip tests.
    fn lcg(s: &mut u64) -> u32 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 33) as u32
    }

    #[test]
    fn every_symbol_kind_round_trips_through_the_encoder_and_decoder() {
        for seed in 0..40u64 {
            let mut s = seed + 1;
            let mut ops = Vec::new();
            for _ in 0..60 {
                let kind = lcg(&mut s) % 7;
                let v = lcg(&mut s);
                ops.push((kind, v, lcg(&mut s)));
            }
            let mut enc = RangeEncoder::new(400);
            let mut wrote = Vec::new();
            for &(kind, v, w) in &ops {
                wrote.push(match kind {
                    0 => Coder::log(&mut enc, 1 + v % 5, w & 1) as i64,
                    1 => Coder::raw(&mut enc, 1 + v % 20, w & ((1 << (1 + v % 20)) - 1)) as i64,
                    2 => {
                        let size = 2 + v % 5000;
                        Coder::uint(&mut enc, size, w % size) as i64
                    }
                    3 => {
                        let k0 = 1 + v % 20;
                        Coder::uint_step(&mut enc, k0, w % (2 * k0 + 1)) as i64
                    }
                    4 => {
                        let qn = 2 * (1 + v % 20);
                        Coder::uint_tri(&mut enc, qn, w % (qn + 1)) as i64
                    }
                    5 => Coder::cdf(&mut enc, &[32, 7, 9, 30, 32], (w % 4) as usize) as i64,
                    _ => {
                        let value = (w % 41) as i32 - 20;
                        enc.laplace(value, 72 << 7, 64 << 6) as i64
                    }
                });
            }
            let bytes = enc.finish().expect("fits");
            let mut dec = RangeDecoder::new(&bytes);
            for (i, &(kind, v, w)) in ops.iter().enumerate() {
                let got = match kind {
                    0 => dec.log(1 + v % 5) as i64,
                    1 => dec.raw(1 + v % 20) as i64,
                    2 => dec.uint(2 + v % 5000) as i64,
                    3 => dec.uint_step(1 + v % 20) as i64,
                    4 => dec.uint_tri(2 * (1 + v % 20)) as i64,
                    5 => dec.cdf(&[32, 7, 9, 30, 32]) as i64,
                    _ => dec.laplace(72 << 7, 64 << 6) as i64,
                };
                let _ = w;
                assert_eq!(got, wrote[i], "seed {seed} op {i} kind {kind}");
            }
        }
    }

    #[test]
    fn the_encoder_and_decoder_agree_on_the_bits_used() {
        let mut enc = RangeEncoder::new(100);
        let mut dec_ops = Vec::new();
        for i in 0..30u32 {
            Coder::uint(&mut enc, 1000, i * 31 % 1000);
            Coder::log(&mut enc, 3, i & 1);
            dec_ops.push(i);
        }
        let (t, tf) = (Coder::tell(&enc), Coder::tell_frac(&enc));
        let bytes = enc.finish().unwrap();
        let mut dec = RangeDecoder::new(&bytes);
        for &i in &dec_ops {
            assert_eq!(dec.uint(1000), i * 31 % 1000);
            assert_eq!(dec.log(3), i & 1);
        }
        assert_eq!((dec.tell(), dec.tell_frac()), (t, tf));
    }
}

#[cfg(test)]
mod laplace_tests {
    use super::*;

    #[test]
    fn laplace_values_round_trip() {
        for fs in [72u32 << 7, 127 << 7, 10 << 7] {
            for decay in [64u32 << 6, 20 << 6, 120 << 6] {
                for v in -40..=40 {
                    let mut enc = RangeEncoder::new(64);
                    let got = enc.laplace(v, fs, decay);
                    let bytes = enc.finish().unwrap();
                    let mut dec = RangeDecoder::new(&bytes);
                    assert_eq!(dec.laplace(fs, decay), got, "fs {fs} decay {decay} value {v}");
                }
            }
        }
    }
}
