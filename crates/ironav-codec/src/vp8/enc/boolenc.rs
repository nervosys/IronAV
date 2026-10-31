//! The boolean entropy encoder of RFC 6386 section 7.3: the inverse of the decoder's `BoolDecoder`.

/// Writes booleans, each false with probability `prob`/256, as bytes.
pub struct BoolEncoder {
    out: Vec<u8>,
    range: u32,
    bottom: u32,
    bit_count: i32,
}

impl Default for BoolEncoder {
    fn default() -> Self {
        Self { out: Vec::new(), range: 255, bottom: 0, bit_count: 24 }
    }
}

impl BoolEncoder {
    pub fn put(&mut self, prob: u8, bit: bool) {
        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        if bit {
            self.bottom = self.bottom.wrapping_add(split);
            self.range -= split;
        } else {
            self.range = split;
        }
        while self.range < 128 {
            self.range <<= 1;
            if self.bottom & (1 << 31) != 0 {
                self.carry();
            }
            self.bottom <<= 1;
            self.bit_count -= 1;
            if self.bit_count == 0 {
                self.out.push((self.bottom >> 24) as u8);
                self.bottom &= (1 << 24) - 1;
                self.bit_count = 8;
            }
        }
    }

    fn carry(&mut self) {
        for b in self.out.iter_mut().rev() {
            if *b == 255 {
                *b = 0;
            } else {
                *b += 1;
                break;
            }
        }
    }

    /// The low `n` bits of `v`, most significant first, at even odds.
    pub fn literal(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.put(128, (v >> i) & 1 == 1);
        }
    }

    /// `value` as a path through `tree` (leaves are the negated values), with the node probabilities.
    pub fn tree(&mut self, tree: &[[i8; 2]], probs: &[u8], value: u8) {
        for (node, bit) in tree_path(tree, 0, value).expect("value is a leaf of the tree") {
            self.put(probs[node], bit);
        }
    }

    /// Ends the partition: pads with enough zero bits that the decoder reads every bit written.
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..32 {
            self.put(128, false);
        }
        self.out
    }
}

fn tree_path(tree: &[[i8; 2]], node: usize, value: u8) -> Option<Vec<(usize, bool)>> {
    for bit in 0..2 {
        let next = tree[node][bit];
        if next <= 0 {
            if (-next) as u8 == value {
                return Some(vec![(node, bit == 1)]);
            }
        } else if let Some(mut path) = tree_path(tree, next as usize, value) {
            path.insert(0, (node, bit == 1));
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp8::BoolDecoder;

    #[test]
    fn decodes_what_was_written() {
        let mut seed = 12345u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            seed >> 8
        };
        let items: Vec<(u8, bool)> = (0..5000).map(|_| (((next() % 255) + 1) as u8, next() % 3 == 0)).collect();
        let mut e = BoolEncoder::default();
        for &(p, b) in &items {
            e.put(p, b);
        }
        let bytes = e.finish();
        let mut d = BoolDecoder::new(&bytes);
        for (i, &(p, b)) in items.iter().enumerate() {
            assert_eq!(d.get(p), b, "bit {i}");
        }
    }
}
