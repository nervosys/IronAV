//! The Huffman tables: quantisation indices, bit allocation, scale factors and transition modes.
//! The codes are assigned as FFmpeg's `ff_vlc_init_from_lengths` does from `dcahuff.c`'s source
//! tables: each entry's code follows on from the one before, so the codes are in increasing order
//! and a code is found by searching the next 32 bits among them.

use super::bits::Bits;
use super::tables::VLC_SRC;
use std::sync::OnceLock;

pub(super) struct Vlc {
    /// Each code, left-aligned in 32 bits.
    codes: Vec<u32>,
    lens: Vec<u8>,
    syms: Vec<i16>,
}

impl Vlc {
    fn new(src: &[(u8, u8)], offset: i16) -> Vlc {
        let mut v = Vlc { codes: Vec::new(), lens: Vec::new(), syms: Vec::new() };
        let mut code = 0u64;
        for &(sym, len) in src {
            v.codes.push(code as u32);
            v.lens.push(len);
            v.syms.push(sym as i16 + offset);
            code += 1u64 << (32 - len as u32);
        }
        v
    }

    /// The symbol whose code starts the next bits, or `None` if none does.
    pub(super) fn get(&self, r: &mut Bits) -> Option<i32> {
        let peek = r.peek32();
        let i = self.codes.partition_point(|&c| c <= peek).checked_sub(1)?;
        let len = self.lens[i] as u32;
        if peek >> (32 - len) != self.codes[i] >> (32 - len) {
            return None;
        }
        r.skip(len as usize);
        Some(self.syms[i] as i32)
    }
}

pub(super) struct Tables {
    /// Quantisation indices by bit allocation index minus one, then by code book.
    pub quant_index: Vec<Vec<Vlc>>,
    pub bit_allocation: Vec<Vlc>,
    pub scale_factor: Vec<Vlc>,
    pub transition_mode: Vec<Vlc>,
}

/// Number of symbols of each quantisation index table (`ff_dca_bitalloc_sizes`).
const SIZES: [usize; 10] = [3, 5, 7, 9, 13, 17, 25, 33, 65, 129];
/// Code books of each (`ff_dca_quant_index_group_size`).
pub(super) const GROUP_SIZE: [u8; 10] = [1, 3, 3, 3, 3, 7, 7, 7, 7, 7];
/// Symbol offsets (`ff_dca_bitalloc_offsets`).
const OFFSETS: [i16; 10] = [-1, -2, -3, -4, -6, -8, -12, -16, -32, -64];

pub(super) fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut at = 0;
        let mut take = |n: usize| {
            let s = &VLC_SRC[at..at + n];
            at += n;
            s
        };
        let quant_index = (0..10).map(|i| (0..GROUP_SIZE[i]).map(|_| Vlc::new(take(SIZES[i]), OFFSETS[i])).collect()).collect();
        let bit_allocation = (0..5).map(|_| Vlc::new(take(12), 1)).collect();
        let scale_factor = (0..5).map(|_| Vlc::new(take(129), -64)).collect();
        let transition_mode = (0..4).map(|_| Vlc::new(take(4), 0)).collect();
        assert_eq!(at, VLC_SRC.len());
        Tables { quant_index, bit_allocation, scale_factor, transition_mode }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table is a complete or incomplete prefix code: no code is a prefix of another, and the
    /// codes never overrun 32 bits.
    #[test]
    fn tables_are_prefix_codes() {
        let t = tables();
        let all: Vec<&Vlc> = t.quant_index.iter().flatten().chain(&t.bit_allocation).chain(&t.scale_factor).chain(&t.transition_mode).collect();
        assert_eq!(all.len(), 48 + 5 + 5 + 4);
        for v in all {
            let mut kraft = 0f64;
            for (i, (&c, &l)) in v.codes.iter().zip(&v.lens).enumerate() {
                kraft += 0.5f64.powi(l as i32);
                if let Some(&n) = v.codes.get(i + 1) {
                    // the next code starts at or after the end of this one's range
                    assert!(n as u64 >= c as u64 + (1u64 << (32 - l as u32)));
                }
            }
            assert!(kraft <= 1.0 + 1e-12, "Kraft sum {kraft}");
        }
    }

    #[test]
    fn decodes_each_symbol_of_a_table_from_its_own_code() {
        let t = tables();
        for v in t.scale_factor.iter().chain(&t.bit_allocation).chain(&t.transition_mode).chain(t.quant_index.iter().flatten()) {
            for i in 0..v.codes.len() {
                // the code followed by zeros, left-aligned in whole bytes
                let word = v.codes[i].to_be_bytes();
                let mut data = word.to_vec();
                data.extend([0u8; 4]);
                let mut r = Bits::new(&data);
                assert_eq!(v.get(&mut r), Some(v.syms[i] as i32));
                assert_eq!(r.pos, v.lens[i] as usize);
            }
        }
    }
}
