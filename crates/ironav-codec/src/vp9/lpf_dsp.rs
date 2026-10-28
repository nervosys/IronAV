//! VP9 loop-filter edge kernels, translated from FFmpeg's vp9dsp_template.c.

/// Clamp to the signed range `[-(1 << bits), (1 << bits) - 1]` (av_clip_intp2).
#[inline]
fn clip_intp2(x: i32, bits: u32) -> i32 {
    x.clamp(-(1i32 << bits), (1i32 << bits) - 1)
}

/// Where the filters read and write samples: a slice, or a plane shared by the loop-filter threads.
pub trait Samples {
    fn get(&self, i: usize) -> u16;
    fn set(&mut self, i: usize, v: u16);
}

impl Samples for [u16] {
    #[inline]
    fn get(&self, i: usize) -> u16 {
        self[i]
    }

    #[inline]
    fn set(&mut self, i: usize, v: u16) {
        self[i] = v;
    }
}

/// Read the sample at offset `j` (in units of `across`) from q0 at index `q0`.
#[inline]
fn rd<B: Samples + ?Sized>(buf: &B, q0: usize, across: usize, j: isize) -> i32 {
    buf.get((q0 as isize + j * across as isize) as usize) as i32
}

/// Flat-region smoothing over `s` (8 or 16 taps). Writes the smoothed value for
/// each interior index `1..n-1` into `out[c]`, with `c = n/2` being q0.
fn wide_smooth(s: &[i32], out: &mut [i32; 16]) {
    let n = s.len();
    let r = (n / 2 - 1) as isize;
    let shift = if n == 8 { 3 } else { 4 };
    let get = |t: isize| s[t.clamp(0, n as isize - 1) as usize];
    // Window sum over taps c-r..=c+r with edge samples replicated.
    let mut win: i32 = (1 - r..=1 + r).map(|t| get(t)).sum();
    for c in 1..n - 1 {
        out[c] = (win + s[c] + (1 << (shift - 1))) >> shift;
        let ci = c as isize;
        win += get(ci + 1 + r) - get(ci - r);
    }
}

/// Filters `count` consecutive positions along an edge.
///
/// `pos` is the index of q0 for the first position. `across` is the step
/// perpendicular to the edge, `along` the step between positions. `e`, `i`
/// and `h` are the 8-bit-scale mb limit, interior limit and hev threshold;
/// they are shifted left by `bd - 8` here, as the FFmpeg template does.
pub fn filter_edge<B: Samples + ?Sized>(
    buf: &mut B,
    pos: usize,
    across: usize,
    along: usize,
    count: usize,
    wd: usize,
    e: i32,
    i: i32,
    h: i32,
    bd: u32,
) {
    let sh = bd - 8;
    let flat_thr = 1i32 << sh;
    let elim = e << sh;
    let ilim = i << sh;
    let hlim = h << sh;
    let pmax = (1i32 << bd) - 1;
    let sclip = bd - 1;
    let slim = (1i32 << sclip) - 1;

    for k in 0..count {
        let q0 = pos + k * along;
        let p3 = rd(buf, q0, across, -4);
        let p2 = rd(buf, q0, across, -3);
        let p1 = rd(buf, q0, across, -2);
        let p0 = rd(buf, q0, across, -1);
        let q0v = rd(buf, q0, across, 0);
        let q1 = rd(buf, q0, across, 1);
        let q2 = rd(buf, q0, across, 2);
        let q3 = rd(buf, q0, across, 3);

        let fm = (p3 - p2).abs() <= ilim
            && (p2 - p1).abs() <= ilim
            && (p1 - p0).abs() <= ilim
            && (q1 - q0v).abs() <= ilim
            && (q2 - q1).abs() <= ilim
            && (q3 - q2).abs() <= ilim
            && (p0 - q0v).abs() * 2 + ((p1 - q1).abs() >> 1) <= elim;
        if !fm {
            continue;
        }

        let flat_in = wd >= 8
            && (p3 - p0).abs() <= flat_thr
            && (p2 - p0).abs() <= flat_thr
            && (p1 - p0).abs() <= flat_thr
            && (q1 - q0v).abs() <= flat_thr
            && (q2 - q0v).abs() <= flat_thr
            && (q3 - q0v).abs() <= flat_thr;

        if wd >= 16 && flat_in {
            let outer = [
                rd(buf, q0, across, -8),
                rd(buf, q0, across, -7),
                rd(buf, q0, across, -6),
                rd(buf, q0, across, -5),
                rd(buf, q0, across, 4),
                rd(buf, q0, across, 5),
                rd(buf, q0, across, 6),
                rd(buf, q0, across, 7),
            ];
            let flat_out = (0..4).all(|n| (outer[n] - p0).abs() <= flat_thr)
                && (4..8).all(|n| (outer[n] - q0v).abs() <= flat_thr);
            if flat_out {
                let s = [
                    outer[0], outer[1], outer[2], outer[3], p3, p2, p1, p0, q0v, q1, q2, q3,
                    outer[4], outer[5], outer[6], outer[7],
                ];
                let mut o = [0i32; 16];
                wide_smooth(&s, &mut o);
                for c in 1..15 {
                    buf.set((q0 as isize + (c as isize - 8) * across as isize) as usize, o[c] as u16);
                }
                continue;
            }
        }

        if flat_in {
            let s = [p3, p2, p1, p0, q0v, q1, q2, q3];
            let mut o = [0i32; 16];
            wide_smooth(&s, &mut o);
            for c in 1..7 {
                buf.set((q0 as isize + (c as isize - 4) * across as isize) as usize, o[c] as u16);
            }
            continue;
        }

        // Narrow filter (filter4).
        let hev = (p1 - p0).abs() > hlim || (q1 - q0v).abs() > hlim;
        let mut f = clip_intp2(3 * (q0v - p0), sclip);
        if hev {
            f = clip_intp2(3 * (q0v - p0) + clip_intp2(p1 - q1, sclip), sclip);
        }
        let f1 = (f + 4).min(slim) >> 3;
        let f2 = (f + 3).min(slim) >> 3;

        let at = |j: isize| (q0 as isize + j * across as isize) as usize;
        buf.set(at(-1), (p0 + f2).clamp(0, pmax) as u16);
        buf.set(at(0), (q0v - f1).clamp(0, pmax) as u16);

        if !hev {
            let f = (f1 + 1) >> 1;
            buf.set(at(-2), (p1 + f).clamp(0, pmax) as u16);
            buf.set(at(1), (q1 - f).clamp(0, pmax) as u16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16x16 buffer with a vertical edge between column 7 and 8.
    fn vertical_step(lo: u16, hi: u16) -> Vec<u16> {
        let mut v = vec![0u16; 256];
        for y in 0..16 {
            for x in 0..16 {
                v[y * 16 + x] = if x < 8 { lo } else { hi };
            }
        }
        v
    }

    #[test]
    fn flat_region_unchanged() {
        for &wd in &[4usize, 8, 16] {
            let mut buf = vec![100u16; 256];
            filter_edge(&mut buf[..], 8, 1, 16, 16, wd, 20, 10, 2, 8);
            assert!(buf.iter().all(|&v| v == 100), "wd={wd}");
        }
    }

    #[test]
    fn small_step_is_smoothed() {
        let mut buf = vertical_step(100, 104);
        filter_edge(&mut buf[..], 8, 1, 16, 16, 4, 20, 10, 2, 8);
        for y in 0..16 {
            let row = &buf[y * 16..y * 16 + 16];
            assert_eq!(&row[0..6], &[100; 6]);
            assert_eq!(&row[10..16], &[104; 6]);
            assert!(row[7] > 100 && row[7] <= 104);
            assert!(row[8] >= 100 && row[8] < 104);
        }
    }

    #[test]
    fn horizontal_matches_transposed_vertical() {
        let mut v = vertical_step(100, 104);
        filter_edge(&mut v[..], 8, 1, 16, 16, 8, 20, 10, 2, 8);

        // Horizontal edge between row 7 and row 8.
        let mut h = vec![0u16; 256];
        for y in 0..16 {
            for x in 0..16 {
                h[y * 16 + x] = if y < 8 { 100 } else { 104 };
            }
        }
        filter_edge(&mut h[..], 8 * 16, 16, 1, 16, 8, 20, 10, 2, 8);

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(h[y * 16 + x], v[x * 16 + y], "y={y} x={x}");
            }
        }
    }

    #[test]
    fn high_bitdepth_flat_region_unchanged() {
        let mut buf = vec![600u16; 256];
        filter_edge(&mut buf[..], 8, 1, 16, 16, 16, 20, 10, 2, 10);
        assert!(buf.iter().all(|&v| v == 600));
    }
}
