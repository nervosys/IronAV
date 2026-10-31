//! The MSB-first bit reader and the lookup-table VLCs of MPEG-4 Part 2.

/// Bits of a byte range, most significant first; reads past the end see zeros.
pub(crate) struct Bits<'a> {
    d: &'a [u8],
    pub pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }

    /// The next `n` bits (1 to 32) without consuming them.
    pub fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let mut w = 0u64;
        for i in 0..5 {
            w = (w << 8) | *self.d.get(byte + i).unwrap_or(&0) as u64;
        }
        let sh = 40 - (self.pos & 7) as u32 - n;
        ((w >> sh) & ((1u64 << n) - 1)) as u32
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    pub fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Whether the reader is past the end of its data.
    pub fn overrun(&self) -> bool {
        self.pos > self.d.len() * 8
    }

    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// A value of `size` bits as the signed VLC escape codes write it: the top bit clear means negative.
    pub fn xbits(&mut self, size: u32) -> i32 {
        if size == 0 {
            return 0;
        }
        let v = self.read(size) as i32;
        if v < 1 << (size - 1) {
            v - (1 << size) + 1
        } else {
            v
        }
    }
}

/// A prefix code over symbols `0..n`, each given as a (code, length) pair; a lookup table of the
/// longest code's length.
pub(crate) struct Vlc {
    lut: Vec<u32>,
    bits: u32,
}

impl Vlc {
    pub fn new(codes: impl Iterator<Item = (u32, u32)>) -> Self {
        let codes: Vec<(u32, u32)> = codes.collect();
        let bits = codes.iter().map(|c| c.1).max().unwrap_or(1).max(1);
        let mut lut = vec![0u32; 1 << bits];
        for (sym, &(code, len)) in codes.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let shift = bits - len;
            for j in 0..1u32 << shift {
                lut[((code << shift) + j) as usize] = ((sym as u32) << 8) | len;
            }
        }
        Vlc { lut, bits }
    }

    /// The next symbol, or `None` for a code that is not in the table.
    pub fn read(&self, br: &mut Bits) -> Option<usize> {
        let e = self.lut[br.peek(self.bits) as usize];
        let len = e & 0xFF;
        if len == 0 {
            return None;
        }
        br.skip(len as usize);
        Some((e >> 8) as usize)
    }
}

/// The run-level code of a block: symbols are (last, run, level) triples with the escape at the end.
pub(crate) struct Rl {
    pub vlc: Vlc,
    pub run: Vec<u8>,
    pub level: Vec<u8>,
    pub last_from: usize,
    pub escape: usize,
    /// The largest level for each run and the largest run for each level, by "last" (0 or 1).
    pub max_level: [[u8; 64]; 2],
    pub max_run: [[u8; 64]; 2],
}

impl Rl {
    pub fn new(codes: &[u16], run: &[i8], level: &[i8], last_from: usize) -> Self {
        let n = run.len();
        let vlc = Vlc::new((0..=n).map(|i| (codes[2 * i] as u32, codes[2 * i + 1] as u32)));
        let mut max_level = [[0u8; 64]; 2];
        let mut max_run = [[0u8; 64]; 2];
        for (last, (lo, hi)) in [(0, last_from), (last_from, n)].into_iter().enumerate() {
            for i in lo..hi {
                let (r, l) = (run[i] as usize, level[i] as u8);
                max_level[last][r] = max_level[last][r].max(l);
                max_run[last][l as usize] = max_run[last][l as usize].max(r as u8);
            }
        }
        Rl { vlc, run: run.iter().map(|&r| r as u8).collect(), level: level.iter().map(|&l| l as u8).collect(), last_from, escape: n, max_level, max_run }
    }
}
