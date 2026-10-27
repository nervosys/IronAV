//! VP8 intra prediction. Neighbours outside the frame read as 127 above and 129 to the left;
//! the pixel above and left of the frame reads as 127 on the first row and 129 below it.

pub const DC_PRED8: u8 = 0;
pub const HOR_PRED8: u8 = 1;
pub const VERT_PRED8: u8 = 2;
pub const TM_PRED8: u8 = 3;

pub const B_VERT: u8 = 0;
pub const B_HOR: u8 = 1;
pub const B_DC: u8 = 2;
pub const B_DDL: u8 = 3;
pub const B_DDR: u8 = 4;
pub const B_VR: u8 = 5;
pub const B_HD: u8 = 6;
pub const B_VL: u8 = 7;
#[allow(dead_code)]
pub const B_HU: u8 = 8;
pub const B_TM: u8 = 9;

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Predicts a whole 16x16 luma or 8x8 chroma block in place from its decoded neighbours.
pub fn predict_block(plane: &mut [u8], stride: usize, x0: usize, y0: usize, size: usize, mode: u8, mb_x: usize, mb_y: usize) {
    let above: Vec<i32> = (0..size).map(|i| if mb_y == 0 { 127 } else { plane[(y0 - 1) * stride + x0 + i] as i32 }).collect();
    let left: Vec<i32> = (0..size).map(|i| if mb_x == 0 { 129 } else { plane[(y0 + i) * stride + x0 - 1] as i32 }).collect();
    let tl = if mb_y == 0 {
        127
    } else if mb_x == 0 {
        129
    } else {
        plane[(y0 - 1) * stride + x0 - 1] as i32
    };
    let shift = size.trailing_zeros();
    for r in 0..size {
        for c in 0..size {
            let v = match mode {
                VERT_PRED8 => above[c],
                HOR_PRED8 => left[r],
                TM_PRED8 => clip(left[r] + above[c] - tl) as i32,
                _ => {
                    let sa: i32 = above.iter().sum();
                    let sl: i32 = left.iter().sum();
                    let n = size as i32;
                    match (mb_x > 0, mb_y > 0) {
                        (true, true) => (sa + sl + n) >> (shift + 1),
                        (true, false) => (sl + n / 2) >> shift,
                        (false, true) => (sa + n / 2) >> shift,
                        (false, false) => 128,
                    }
                }
            };
            plane[(y0 + r) * stride + x0 + c] = v as u8;
        }
    }
}

/// One 4x4 sub-block. `a` holds the four pixels above and the four above-right, `l` the left
/// column and `tl` the corner. The result is row-major.
pub fn predict4x4(mode: u8, a: [u8; 8], l: [u8; 4], tl: u8) -> [u8; 16] {
    let t: Vec<i32> = a.iter().map(|&v| v as i32).collect();
    let (l0, l1, l2, l3) = (l[0] as i32, l[1] as i32, l[2] as i32, l[3] as i32);
    let lt = tl as i32;
    let mut o = [0u8; 16];
    // `p(x, y)` assigns one pixel; the formulas below follow the usual column-then-row notation
    let mut set = |x: usize, y: usize, v: i32| o[y * 4 + x] = v as u8;
    let avg3 = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
    let avg2 = |a: i32, b: i32| (a + b + 1) >> 1;
    match mode {
        B_DC => {
            let s: i32 = t[..4].iter().sum::<i32>() + l0 + l1 + l2 + l3;
            let v = (s + 4) >> 3;
            for y in 0..4 {
                for x in 0..4 {
                    set(x, y, v);
                }
            }
        }
        B_TM => {
            for y in 0..4 {
                for x in 0..4 {
                    set(x, y, clip(l[y] as i32 + t[x] - lt) as i32);
                }
            }
        }
        B_VERT => {
            let v = [avg3(lt, t[0], t[1]), avg3(t[0], t[1], t[2]), avg3(t[1], t[2], t[3]), avg3(t[2], t[3], t[4])];
            for y in 0..4 {
                for x in 0..4 {
                    set(x, y, v[x]);
                }
            }
        }
        B_HOR => {
            let v = [avg3(lt, l0, l1), avg3(l0, l1, l2), avg3(l1, l2, l3), avg3(l2, l3, l3)];
            for y in 0..4 {
                for x in 0..4 {
                    set(x, y, v[y]);
                }
            }
        }
        B_DDL => {
            set(0, 0, avg3(t[0], t[1], t[2]));
            let v = avg3(t[1], t[2], t[3]);
            set(1, 0, v);
            set(0, 1, v);
            let v = avg3(t[2], t[3], t[4]);
            set(2, 0, v);
            set(1, 1, v);
            set(0, 2, v);
            let v = avg3(t[3], t[4], t[5]);
            set(3, 0, v);
            set(2, 1, v);
            set(1, 2, v);
            set(0, 3, v);
            let v = avg3(t[4], t[5], t[6]);
            set(3, 1, v);
            set(2, 2, v);
            set(1, 3, v);
            let v = avg3(t[5], t[6], t[7]);
            set(3, 2, v);
            set(2, 3, v);
            set(3, 3, (t[6] + 3 * t[7] + 2) >> 2);
        }
        B_DDR => {
            set(0, 3, avg3(l3, l2, l1));
            let v = avg3(l2, l1, l0);
            set(0, 2, v);
            set(1, 3, v);
            let v = avg3(l1, l0, lt);
            set(0, 1, v);
            set(1, 2, v);
            set(2, 3, v);
            let v = avg3(l0, lt, t[0]);
            set(0, 0, v);
            set(1, 1, v);
            set(2, 2, v);
            set(3, 3, v);
            let v = avg3(lt, t[0], t[1]);
            set(1, 0, v);
            set(2, 1, v);
            set(3, 2, v);
            let v = avg3(t[0], t[1], t[2]);
            set(2, 0, v);
            set(3, 1, v);
            set(3, 0, avg3(t[1], t[2], t[3]));
        }
        B_VR => {
            let v = avg2(lt, t[0]);
            set(0, 0, v);
            set(1, 2, v);
            let v = avg2(t[0], t[1]);
            set(1, 0, v);
            set(2, 2, v);
            let v = avg2(t[1], t[2]);
            set(2, 0, v);
            set(3, 2, v);
            set(3, 0, avg2(t[2], t[3]));
            let v = avg3(l0, lt, t[0]);
            set(0, 1, v);
            set(1, 3, v);
            let v = avg3(lt, t[0], t[1]);
            set(1, 1, v);
            set(2, 3, v);
            let v = avg3(t[0], t[1], t[2]);
            set(2, 1, v);
            set(3, 3, v);
            set(3, 1, avg3(t[1], t[2], t[3]));
            set(0, 2, avg3(lt, l0, l1));
            set(0, 3, avg3(l0, l1, l2));
        }
        B_VL => {
            set(0, 0, avg2(t[0], t[1]));
            let v = avg2(t[1], t[2]);
            set(1, 0, v);
            set(0, 2, v);
            let v = avg2(t[2], t[3]);
            set(2, 0, v);
            set(1, 2, v);
            let v = avg2(t[3], t[4]);
            set(3, 0, v);
            set(2, 2, v);
            set(0, 1, avg3(t[0], t[1], t[2]));
            let v = avg3(t[1], t[2], t[3]);
            set(1, 1, v);
            set(0, 3, v);
            let v = avg3(t[2], t[3], t[4]);
            set(2, 1, v);
            set(1, 3, v);
            let v = avg3(t[3], t[4], t[5]);
            set(3, 1, v);
            set(2, 3, v);
            set(3, 2, avg3(t[4], t[5], t[6]));
            set(3, 3, avg3(t[5], t[6], t[7]));
        }
        B_HD => {
            let v = avg2(lt, l0);
            set(0, 0, v);
            set(2, 1, v);
            let v = avg3(l0, lt, t[0]);
            set(1, 0, v);
            set(3, 1, v);
            set(2, 0, avg3(lt, t[0], t[1]));
            set(3, 0, avg3(t[0], t[1], t[2]));
            let v = avg2(l0, l1);
            set(0, 1, v);
            set(2, 2, v);
            let v = avg3(lt, l0, l1);
            set(1, 1, v);
            set(3, 2, v);
            let v = avg2(l1, l2);
            set(0, 2, v);
            set(2, 3, v);
            let v = avg3(l0, l1, l2);
            set(1, 2, v);
            set(3, 3, v);
            set(0, 3, avg2(l2, l3));
            set(1, 3, avg3(l1, l2, l3));
        }
        _ => {
            // B_HU
            set(0, 0, avg2(l0, l1));
            set(1, 0, avg3(l0, l1, l2));
            let v = avg2(l1, l2);
            set(2, 0, v);
            set(0, 1, v);
            let v = avg3(l1, l2, l3);
            set(3, 0, v);
            set(1, 1, v);
            let v = avg2(l2, l3);
            set(2, 1, v);
            set(0, 2, v);
            let v = avg3(l2, l3, l3);
            set(3, 1, v);
            set(1, 2, v);
            for (x, y) in [(3, 2), (1, 3), (0, 3), (2, 2), (2, 3), (3, 3)] {
                set(x, y, l3);
            }
        }
    }
    o
}
