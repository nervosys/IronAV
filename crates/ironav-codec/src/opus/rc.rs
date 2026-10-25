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
        let total_bits = self.total_bits << 3;
        let mut rcbuffer = log2(self.range) + 1;
        let mut range = self.range >> (rcbuffer - 16);
        for _ in 0..3 {
            range = range * range >> 15;
            let bit = range >> 16;
            rcbuffer = rcbuffer << 1 | bit;
            range >>= bit;
        }
        (total_bits - rcbuffer) as i32
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
