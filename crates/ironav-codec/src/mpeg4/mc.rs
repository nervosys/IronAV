//! Motion compensation: half-sample bilinear (chroma, and luma without quarter-sample) and the
//! MPEG-4 quarter-sample filters. The filters follow FFmpeg's `qpeldsp.c` and `hpeldsp.c`
//! (Michael Niedermayer, Fabrice Bellard and the FFmpeg developers, LGPL-2.1-or-later), including
//! how the 8-tap lowpass treats the block's edge, so that the prediction matches FFmpeg's.

/// A reference plane: samples are read with the edge replicated from its `w` by `h` area.
pub(crate) struct Plane<'a> {
    pub d: &'a [u8],
    pub stride: usize,
    pub w: i32,
    pub h: i32,
}

impl Plane<'_> {
    #[inline]
    fn px(&self, x: i32, y: i32) -> i32 {
        let x = x.clamp(0, self.w - 1) as usize;
        let y = y.clamp(0, self.h - 1) as usize;
        self.d[y * self.stride + x] as i32
    }
}

#[inline]
fn store(dst: &mut u8, v: i32, avg: bool) {
    *dst = if avg { ((*dst as i32 + v + 1) >> 1) as u8 } else { v as u8 };
}

/// Half-sample motion compensation of a `bw` by `bh` block whose top left sample in the reference is
/// `(sx, sy)` and whose half-sample fraction is `dxy` (bit 0 horizontal, bit 1 vertical). `nr` is
/// the no-rounding control; `avg` averages the result into `dst` (the second direction of B-VOPs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn hpel(dst: &mut [u8], ds: usize, bw: usize, bh: usize, p: &Plane, sx: i32, sy: i32, dxy: u32, nr: bool, avg: bool) {
    let nr = nr as i32;
    for r in 0..bh {
        for c in 0..bw {
            let (x, y) = (sx + c as i32, sy + r as i32);
            let v = match dxy {
                0 => p.px(x, y),
                1 => (p.px(x, y) + p.px(x + 1, y) + 1 - nr) >> 1,
                2 => (p.px(x, y) + p.px(x, y + 1) + 1 - nr) >> 1,
                _ => (p.px(x, y) + p.px(x + 1, y) + p.px(x, y + 1) + p.px(x + 1, y + 1) + 2 - nr) >> 2,
            };
            store(&mut dst[r * ds + c], v, avg);
        }
    }
}

/// The index a lowpass tap at `j` reads from a window `0..=n`: outside it, the samples are mirrored
/// about the window's edges.
#[inline]
fn reflect(j: i32, n: i32) -> i32 {
    if j < 0 {
        -j - 1
    } else if j > n {
        2 * n + 1 - j
    } else {
        j
    }
}

/// The 8-tap lowpass at output `k` (between samples `k` and `k + 1`) over `get`, rounded by
/// `rnd` and clipped to a sample.
#[inline]
fn lowpass(get: &dyn Fn(i32) -> i32, k: i32, n: i32, rnd: i32) -> i32 {
    let g = |j: i32| get(reflect(j, n));
    let v = 20 * (g(k) + g(k + 1)) - 6 * (g(k - 1) + g(k + 2)) + 3 * (g(k - 2) + g(k + 3)) - (g(k - 3) + g(k + 4));
    ((v + rnd) >> 5).clamp(0, 255)
}

/// The quarter-sample motion compensation of an `n` by `n` block whose top left sample in the
/// reference is `(sx, sy)`; `dxy` is `(y fraction << 2) | x fraction`, as FFmpeg numbers them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn qpel(dst: &mut [u8], ds: usize, n: usize, p: &Plane, sx: i32, sy: i32, dxy: u32, nr: bool, avg: bool) {
    let (xf, yf) = ((dxy & 3) as i32, (dxy >> 2) as i32);
    let ni = n as i32;
    let w = n + 1;
    // the (n + 1) square of reference samples the filters read, from the block's top left
    let mut reg = [0i32; 17 * 17];
    for r in 0..w {
        for c in 0..w {
            reg[r * w + c] = p.px(sx + c as i32, sy + r as i32);
        }
    }
    let s = |y: i32, x: i32| reg[y as usize * w + x as usize];
    let rnd = if nr { 15 } else { 16 };
    let r2 = if nr { 0 } else { 1 };
    let a2s = |a: i32, b: i32| (a + b + r2) >> 1;
    // the horizontal half samples of every row of the (n + 1) square, and the averages of them
    // with the full samples (or the next column) for the diagonal positions
    let mut hh = [0i32; 17 * 16];
    for y in 0..=ni {
        for k in 0..ni {
            let h = lowpass(&|j| s(y, j), k, ni, rnd);
            hh[y as usize * 16 + k as usize] = match xf {
                1 => a2s(h, s(y, k)),
                3 => a2s(h, s(y, k + 1)),
                _ => h,
            };
        }
    }
    let hv = |y: i32, k: i32| hh[y as usize * 16 + k as usize];
    let hz = |y: i32, k: i32| lowpass(&|j| s(y, j), k, ni, rnd);
    let vs = |y: i32, x: i32| lowpass(&|j| s(j, x), y, ni, rnd);
    let vh = |y: i32, x: i32| lowpass(&|j| hv(j, x), y, ni, rnd);
    for y in 0..ni {
        for x in 0..ni {
            let v = match (xf, yf) {
                (0, 0) => s(y, x),
                (1, 0) => a2s(s(y, x), hz(y, x)),
                (2, 0) => hz(y, x),
                (3, 0) => a2s(s(y, x + 1), hz(y, x)),
                (0, 1) => a2s(s(y, x), vs(y, x)),
                (0, 2) => vs(y, x),
                (0, 3) => a2s(s(y + 1, x), vs(y, x)),
                (_, 2) => vh(y, x),
                (_, 1) => a2s(hv(y, x), vh(y, x)),
                _ => a2s(hv(y + 1, x), vh(y, x)),
            };
            store(&mut dst[y as usize * ds + x as usize], v, avg);
        }
    }
}
