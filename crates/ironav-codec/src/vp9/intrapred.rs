//! VP9 intra predictors, a bit-exact port of FFmpeg's vp9dsp_template.c.

pub const VERT_PRED: u8 = 0;
pub const HOR_PRED: u8 = 1;
pub const DC_PRED: u8 = 2;
pub const DIAG_DOWN_LEFT_PRED: u8 = 3;
pub const DIAG_DOWN_RIGHT_PRED: u8 = 4;
pub const VERT_RIGHT_PRED: u8 = 5;
pub const HOR_DOWN_PRED: u8 = 6;
pub const VERT_LEFT_PRED: u8 = 7;
pub const HOR_UP_PRED: u8 = 8;
pub const TM_VP8_PRED: u8 = 9;
pub const LEFT_DC_PRED: u8 = 10;
pub const TOP_DC_PRED: u8 = 11;
pub const DC_128_PRED: u8 = 12;
pub const DC_127_PRED: u8 = 13;
pub const DC_129_PRED: u8 = 14;

const MAX_N: usize = 32;

#[inline]
fn avg2(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}

#[inline]
fn avg3(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}

/// Predicts an n*n block (n = 4 << log2m4, log2m4 in 0..=3) into `dst` (row stride `stride`,
/// starting at dst[0]). `top` holds 2*n+1 samples: top[0] is the top-left sample, top[1..=n] the
/// samples above, top[n+1..=2n] the above-right samples. `left` holds n samples, top to bottom.
pub fn predict(
    mode: u8,
    log2m4: usize,
    dst: &mut [u16],
    stride: usize,
    top: &[u16],
    left: &[u16],
    bd: u32,
) {
    assert!(log2m4 <= 3 && (8..=12).contains(&bd));
    let n = 4usize << log2m4;
    assert!(top.len() > 2 * n && left.len() >= n);

    // tb[0] = top-left, tb[1 + i] = above sample i.
    let mut tb = [0i32; 2 * MAX_N + 1];
    for (d, &s) in tb.iter_mut().zip(&top[..2 * n + 1]) {
        *d = s as i32;
    }
    // lf uses FFmpeg's edge order: lf[i] is row n-1-i. HOR_UP reads the edge in row order.
    let mut lf = [0i32; MAX_N];
    for i in 0..n {
        let s = if mode == HOR_UP_PRED { left[i] } else { left[n - 1 - i] };
        lf[i] = s as i32;
    }

    let max = (1i32 << bd) - 1;
    let base = 128i32 << (bd - 8);
    let ni = n as i32;
    let shift = log2m4 as i32 + 3;
    let sum_l: i32 = lf[..n].iter().sum();
    let sum_t: i32 = tb[1..=n].iter().sum();
    let tl = tb[0];

    match mode {
        VERT_PRED => vert(dst, stride, n, &tb),
        HOR_PRED => hor(dst, stride, n, &lf),
        DC_PRED => fill(dst, stride, n, (sum_l + sum_t + ni) >> shift),
        LEFT_DC_PRED => fill(dst, stride, n, (sum_l + ni / 2) >> (shift - 1)),
        TOP_DC_PRED => fill(dst, stride, n, (sum_t + ni / 2) >> (shift - 1)),
        DC_128_PRED => fill(dst, stride, n, base),
        DC_127_PRED => fill(dst, stride, n, base - 1),
        DC_129_PRED => fill(dst, stride, n, base + 1),
        TM_VP8_PRED => tm(dst, stride, n, &tb, &lf, tl, max),
        DIAG_DOWN_LEFT_PRED => {
            if n == 4 {
                diag_dl4(dst, stride, &tb)
            } else {
                diag_dl(dst, stride, n, &tb)
            }
        }
        DIAG_DOWN_RIGHT_PRED => {
            if n == 4 {
                diag_dr4(dst, stride, &tb, &lf)
            } else {
                diag_dr(dst, stride, n, &tb, &lf)
            }
        }
        VERT_RIGHT_PRED => {
            if n == 4 {
                vert_right4(dst, stride, &tb, &lf)
            } else {
                vert_right(dst, stride, n, &tb, &lf)
            }
        }
        HOR_DOWN_PRED => {
            if n == 4 {
                hor_down4(dst, stride, &tb, &lf)
            } else {
                hor_down(dst, stride, n, &tb, &lf)
            }
        }
        VERT_LEFT_PRED => {
            if n == 4 {
                vert_left4(dst, stride, &tb)
            } else {
                vert_left(dst, stride, n, &tb)
            }
        }
        HOR_UP_PRED => hor_up(dst, stride, n, &lf),
        _ => panic!("invalid VP9 intra mode {mode}"),
    }
}

fn fill(dst: &mut [u16], s: usize, n: usize, v: i32) {
    for y in 0..n {
        dst[y * s..y * s + n].fill(v as u16);
    }
}

fn vert(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65]) {
    for y in 0..n {
        for x in 0..n {
            dst[y * s + x] = tb[x + 1] as u16;
        }
    }
}

fn hor(dst: &mut [u16], s: usize, n: usize, lf: &[i32; MAX_N]) {
    for y in 0..n {
        let v = lf[n - 1 - y] as u16;
        dst[y * s..y * s + n].fill(v);
    }
}

fn tm(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65], lf: &[i32; MAX_N], tl: i32, max: i32) {
    for y in 0..n {
        let l = lf[n - 1 - y] - tl;
        for x in 0..n {
            dst[y * s + x] = (tb[x + 1] + l).clamp(0, max) as u16;
        }
    }
}

fn diag_dl4(dst: &mut [u16], s: usize, tb: &[i32; 65]) {
    let a = |i: usize| tb[i + 1];
    let mut p = |xy: &[(usize, usize)], v: i32| {
        for &(x, y) in xy {
            dst[y * s + x] = v as u16;
        }
    };
    p(&[(0, 0)], avg3(a(0), a(1), a(2)));
    p(&[(1, 0), (0, 1)], avg3(a(1), a(2), a(3)));
    p(&[(2, 0), (1, 1), (0, 2)], avg3(a(2), a(3), a(4)));
    p(&[(3, 0), (2, 1), (1, 2), (0, 3)], avg3(a(3), a(4), a(5)));
    p(&[(3, 1), (2, 2), (1, 3)], avg3(a(4), a(5), a(6)));
    p(&[(3, 2), (2, 3)], avg3(a(5), a(6), a(7)));
    p(&[(3, 3)], a(7)); // not avg3: differs from VP8
}

fn diag_dl(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65]) {
    let mut v = [0i32; MAX_N];
    for i in 0..n - 2 {
        v[i] = avg3(tb[i + 1], tb[i + 2], tb[i + 3]);
    }
    v[n - 2] = (tb[n - 1] + 3 * tb[n] + 2) >> 2;
    for j in 0..n {
        for x in 0..n {
            let val = if x < n - 1 - j { v[j + x] } else { tb[n] };
            dst[j * s + x] = val as u16;
        }
    }
}

fn diag_dr4(dst: &mut [u16], s: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let (l0, l1, l2, l3) = (lf[3], lf[2], lf[1], lf[0]);
    let (tl, a0, a1, a2, a3) = (tb[0], tb[1], tb[2], tb[3], tb[4]);
    let mut p = |xy: &[(usize, usize)], v: i32| {
        for &(x, y) in xy {
            dst[y * s + x] = v as u16;
        }
    };
    p(&[(0, 3)], avg3(l1, l2, l3));
    p(&[(0, 2), (1, 3)], avg3(l0, l1, l2));
    p(&[(0, 1), (1, 2), (2, 3)], avg3(tl, l0, l1));
    p(&[(0, 0), (1, 1), (2, 2), (3, 3)], avg3(l0, tl, a0));
    p(&[(1, 0), (2, 1), (3, 2)], avg3(tl, a0, a1));
    p(&[(2, 0), (3, 1)], avg3(a0, a1, a2));
    p(&[(3, 0)], avg3(a1, a2, a3));
}

fn diag_dr(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let tl = tb[0];
    let mut v = [0i32; 2 * MAX_N];
    for i in 0..n - 2 {
        v[i] = avg3(lf[i], lf[i + 1], lf[i + 2]);
        v[n + 1 + i] = avg3(tb[i + 1], tb[i + 2], tb[i + 3]);
    }
    v[n - 2] = avg3(lf[n - 2], lf[n - 1], tl);
    v[n - 1] = avg3(lf[n - 1], tl, tb[1]);
    v[n] = avg3(tl, tb[1], tb[2]);
    for j in 0..n {
        for x in 0..n {
            dst[j * s + x] = v[n - 1 - j + x] as u16;
        }
    }
}

fn vert_right4(dst: &mut [u16], s: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let (l0, l1, l2) = (lf[3], lf[2], lf[1]);
    let (tl, a0, a1, a2, a3) = (tb[0], tb[1], tb[2], tb[3], tb[4]);
    let mut p = |xy: &[(usize, usize)], v: i32| {
        for &(x, y) in xy {
            dst[y * s + x] = v as u16;
        }
    };
    p(&[(0, 3)], avg3(l0, l1, l2));
    p(&[(0, 2)], avg3(tl, l0, l1));
    p(&[(0, 0), (1, 2)], avg2(tl, a0));
    p(&[(0, 1), (1, 3)], avg3(l0, tl, a0));
    p(&[(1, 0), (2, 2)], avg2(a0, a1));
    p(&[(1, 1), (2, 3)], avg3(tl, a0, a1));
    p(&[(2, 0), (3, 2)], avg2(a1, a2));
    p(&[(2, 1), (3, 3)], avg3(a0, a1, a2));
    p(&[(3, 0)], avg2(a2, a3));
    p(&[(3, 1)], avg3(a1, a2, a3));
}

fn vert_right(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let half = n / 2;
    let tl = tb[0];
    let mut ve = [0i32; 48];
    let mut vo = [0i32; 48];
    for i in 0..half - 2 {
        vo[i] = avg3(lf[2 * i + 1], lf[2 * i + 2], lf[2 * i + 3]);
        ve[i] = avg3(lf[2 * i + 2], lf[2 * i + 3], lf[2 * i + 4]);
    }
    vo[half - 2] = avg3(lf[n - 3], lf[n - 2], lf[n - 1]);
    ve[half - 2] = avg3(lf[n - 2], lf[n - 1], tl);
    ve[half - 1] = avg2(tl, tb[1]);
    vo[half - 1] = avg3(lf[n - 1], tl, tb[1]);
    for i in 0..n - 1 {
        ve[half + i] = avg2(tb[i + 1], tb[i + 2]);
        vo[half + i] = avg3(tb[i], tb[i + 1], tb[i + 2]);
    }
    for j in 0..half {
        for x in 0..n {
            dst[2 * j * s + x] = ve[half - 1 - j + x] as u16;
            dst[(2 * j + 1) * s + x] = vo[half - 1 - j + x] as u16;
        }
    }
}

fn hor_down4(dst: &mut [u16], s: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let (l0, l1, l2, l3) = (lf[3], lf[2], lf[1], lf[0]);
    let (tl, a0, a1, a2) = (tb[0], tb[1], tb[2], tb[3]);
    let mut p = |xy: &[(usize, usize)], v: i32| {
        for &(x, y) in xy {
            dst[y * s + x] = v as u16;
        }
    };
    p(&[(2, 0)], avg3(tl, a0, a1));
    p(&[(3, 0)], avg3(a0, a1, a2));
    p(&[(0, 0), (2, 1)], avg2(tl, l0));
    p(&[(1, 0), (3, 1)], avg3(a0, tl, l0));
    p(&[(0, 1), (2, 2)], avg2(l0, l1));
    p(&[(1, 1), (3, 2)], avg3(tl, l0, l1));
    p(&[(0, 2), (2, 3)], avg2(l1, l2));
    p(&[(1, 2), (3, 3)], avg3(l0, l1, l2));
    p(&[(0, 3)], avg2(l2, l3));
    p(&[(1, 3)], avg3(l1, l2, l3));
}

fn hor_down(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65], lf: &[i32; MAX_N]) {
    let tl = tb[0];
    let mut v = [0i32; 3 * MAX_N];
    for i in 0..n - 2 {
        v[2 * i] = avg2(lf[i + 1], lf[i]);
        v[2 * i + 1] = avg3(lf[i], lf[i + 1], lf[i + 2]);
        v[2 * n + i] = avg3(tb[i], tb[i + 1], tb[i + 2]);
    }
    v[2 * n - 2] = avg2(tl, lf[n - 1]);
    v[2 * n - 4] = avg2(lf[n - 1], lf[n - 2]);
    v[2 * n - 1] = avg3(tb[1], tl, lf[n - 1]);
    v[2 * n - 3] = avg3(tl, lf[n - 1], lf[n - 2]);
    for j in 0..n {
        for x in 0..n {
            dst[j * s + x] = v[2 * n - 2 - 2 * j + x] as u16;
        }
    }
}

fn vert_left4(dst: &mut [u16], s: usize, tb: &[i32; 65]) {
    let a = |i: usize| tb[i + 1];
    let mut p = |xy: &[(usize, usize)], v: i32| {
        for &(x, y) in xy {
            dst[y * s + x] = v as u16;
        }
    };
    p(&[(0, 0)], avg2(a(0), a(1)));
    p(&[(0, 1)], avg3(a(0), a(1), a(2)));
    p(&[(1, 0), (0, 2)], avg2(a(1), a(2)));
    p(&[(1, 1), (0, 3)], avg3(a(1), a(2), a(3)));
    p(&[(2, 0), (1, 2)], avg2(a(2), a(3)));
    p(&[(2, 1), (1, 3)], avg3(a(2), a(3), a(4)));
    p(&[(3, 0), (2, 2)], avg2(a(3), a(4)));
    p(&[(3, 1), (2, 3)], avg3(a(3), a(4), a(5)));
    p(&[(3, 2)], avg2(a(4), a(5)));
    p(&[(3, 3)], avg3(a(4), a(5), a(6)));
}

fn vert_left(dst: &mut [u16], s: usize, n: usize, tb: &[i32; 65]) {
    let mut ve = [0i32; MAX_N];
    let mut vo = [0i32; MAX_N];
    for i in 0..n - 2 {
        ve[i] = avg2(tb[i + 1], tb[i + 2]);
        vo[i] = avg3(tb[i + 1], tb[i + 2], tb[i + 3]);
    }
    ve[n - 2] = avg2(tb[n - 1], tb[n]);
    vo[n - 2] = (tb[n - 1] + 3 * tb[n] + 2) >> 2;
    let last = tb[n];
    for j in 0..n / 2 {
        for x in 0..n {
            let (e, o) = if x < n - 1 - j { (ve[j + x], vo[j + x]) } else { (last, last) };
            dst[2 * j * s + x] = e as u16;
            dst[(2 * j + 1) * s + x] = o as u16;
        }
    }
}

fn hor_up(dst: &mut [u16], s: usize, n: usize, lf: &[i32; MAX_N]) {
    let mut v = [0i32; 2 * MAX_N];
    for i in 0..n - 2 {
        v[2 * i] = avg2(lf[i], lf[i + 1]);
        v[2 * i + 1] = avg3(lf[i], lf[i + 1], lf[i + 2]);
    }
    v[2 * n - 4] = avg2(lf[n - 2], lf[n - 1]);
    v[2 * n - 3] = avg3(lf[n - 2], lf[n - 1], lf[n - 1]);
    let last = lf[n - 1];
    for j in 0..n {
        let cnt = if j < n / 2 { n } else { 2 * n - 2 - 2 * j };
        for x in 0..cnt {
            dst[j * s + x] = v[2 * j + x] as u16;
        }
        for x in cnt..n {
            dst[j * s + x] = last as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mode: u8, log2m4: usize, top: &[u16], left: &[u16], bd: u32) -> Vec<u16> {
        let n = 4 << log2m4;
        let mut dst = vec![0u16; n * n];
        predict(mode, log2m4, &mut dst, n, top, left, bd);
        dst
    }

    #[test]
    fn dc_of_constant_edges_is_constant() {
        for log2 in 0..4 {
            let n = 4 << log2;
            let top = vec![100u16; 2 * n + 1];
            let left = vec![100u16; n];
            assert!(run(DC_PRED, log2, &top, &left, 8).iter().all(|&v| v == 100));
            assert!(run(LEFT_DC_PRED, log2, &top, &left, 8).iter().all(|&v| v == 100));
            assert!(run(TOP_DC_PRED, log2, &top, &left, 8).iter().all(|&v| v == 100));
        }
    }

    #[test]
    fn vertical_copies_top() {
        for log2 in 0..4 {
            let n = 4 << log2;
            let mut top = vec![0u16; 2 * n + 1];
            for i in 0..n {
                top[i + 1] = 10 + i as u16;
            }
            let left = vec![0u16; n];
            let d = run(VERT_PRED, log2, &top, &left, 8);
            for y in 0..n {
                assert_eq!(&d[y * n..y * n + n], &top[1..=n]);
            }
        }
    }

    #[test]
    fn horizontal_uses_left_top_to_bottom() {
        let n = 4;
        let top = vec![0u16; 2 * n + 1];
        let left = [10u16, 20, 30, 40];
        let d = run(HOR_PRED, 0, &top, &left, 8);
        for y in 0..n {
            assert!(d[y * n..y * n + n].iter().all(|&v| v == left[y]));
        }
    }

    #[test]
    fn tm_constant_edges() {
        for log2 in 0..4 {
            let n = 4 << log2;
            let mut top = vec![50u16; 2 * n + 1];
            top[0] = 40; // top-left
            let left = vec![60u16; n];
            assert!(run(TM_VP8_PRED, log2, &top, &left, 8).iter().all(|&v| v == 70));
        }
        // Clipping to the bit depth maximum.
        let mut top = vec![200u16; 9];
        top[0] = 0;
        let left = vec![200u16; 4];
        assert!(run(TM_VP8_PRED, 0, &top, &left, 8).iter().all(|&v| v == 255));
    }

    #[test]
    fn dc_127_129_scale_with_bit_depth() {
        let top = vec![0u16; 9];
        let left = vec![0u16; 4];
        assert!(run(DC_127_PRED, 0, &top, &left, 8).iter().all(|&v| v == 127));
        assert!(run(DC_129_PRED, 0, &top, &left, 8).iter().all(|&v| v == 129));
        assert!(run(DC_128_PRED, 0, &top, &left, 10).iter().all(|&v| v == 512));
        assert!(run(DC_127_PRED, 0, &top, &left, 10).iter().all(|&v| v == 511));
        assert!(run(DC_129_PRED, 0, &top, &left, 10).iter().all(|&v| v == 513));
    }
}
