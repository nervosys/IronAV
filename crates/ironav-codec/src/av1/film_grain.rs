//! Film grain: the film_grain_params syntax (spec 5.9.30) and grain synthesis (spec 7.18.3), applied
//! to output frames only. The synthesis follows the structure of FFmpeg's aom_film_grain (LGPL-2.1
//! or later), which reproduces libaom's output.

use super::grain_tables::GAUSSIAN_SEQUENCE;
use super::ipred::Plane;
use super::obu::{Bits, SequenceHeader};
use ironav_core::{Error, ErrorCode, Result};

/// The film grain parameters of a frame, with the AR coefficients and multipliers already offset.
#[derive(Clone, Debug, Default)]
pub struct FilmGrain {
    seed: u32,
    points: [Vec<(u8, u8)>; 3],
    chroma_from_luma: bool,
    scaling_shift: u32,
    ar_lag: usize,
    ar_coeffs: [[i32; 25]; 3],
    ar_shift: u32,
    grain_scale_shift: u32,
    mult: [i32; 2],
    mult_luma: [i32; 2],
    offset: [i32; 2],
    overlap: bool,
    limit_range: bool,
    sub: [usize; 2],
}

const GW: usize = 82;
const GH: usize = 73;
const BLOCK: usize = 32;

/// film_grain_params: None when the frame has no grain.
pub fn parse(b: &mut Bits, seq: &SequenceHeader, inter: bool) -> Result<Option<FilmGrain>> {
    if !b.flag()? {
        return Ok(None);
    }
    let mut g = FilmGrain { seed: b.f(16)?, sub: [seq.subsampling_x as usize, seq.subsampling_y as usize], ..Default::default() };
    if inter && !b.flag()? {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "AV1: film grain parameters of a reference frame are not decoded yet"));
    }
    let ny = b.f(4)? as usize;
    for _ in 0..ny {
        g.points[0].push((b.f(8)? as u8, b.f(8)? as u8));
    }
    let mono = seq.mono_chrome;
    g.chroma_from_luma = !mono && b.flag()?;
    if !(mono || g.chroma_from_luma || (seq.subsampling_x && seq.subsampling_y && ny == 0)) {
        for pl in 1..3 {
            for _ in 0..b.f(4)? {
                g.points[pl].push((b.f(8)? as u8, b.f(8)? as u8));
            }
        }
    }
    g.scaling_shift = b.f(2)? + 8;
    let lag = b.f(2)? as usize;
    g.ar_lag = lag;
    let npl = 2 * lag * (lag + 1);
    if ny > 0 {
        for i in 0..npl {
            g.ar_coeffs[0][i] = b.f(8)? as i32 - 128;
        }
    }
    let npc = if ny > 0 { npl + 1 } else { npl };
    for pl in 1..3 {
        if g.chroma_from_luma || !g.points[pl].is_empty() {
            for i in 0..npc {
                g.ar_coeffs[pl][i] = b.f(8)? as i32 - 128;
            }
        }
    }
    g.ar_shift = b.f(2)? + 6;
    g.grain_scale_shift = b.f(2)?;
    for uv in 0..2 {
        if !g.points[uv + 1].is_empty() {
            g.mult[uv] = b.f(8)? as i32 - 128;
            g.mult_luma[uv] = b.f(8)? as i32 - 128;
            g.offset[uv] = b.f(9)? as i32 - 256;
        }
    }
    g.overlap = b.flag()?;
    g.limit_range = b.flag()?;
    Ok(Some(g))
}

fn rand(bits: u32, state: &mut u32) -> u32 {
    let r = *state;
    let bit = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
    let r = (r >> 1) | (bit << 15);
    *state = r;
    (r >> (16 - bits)) & ((1 << bits) - 1)
}

fn round2(x: i32, n: u32) -> i32 {
    if n == 0 {
        x
    } else {
        (x + (1 << (n - 1))) >> n
    }
}

type Grain = Vec<Vec<i32>>;

/// The grain template of a plane: luma is 73 by 82; chroma is smaller where subsampled, and gets
/// its luma contribution from `luma`.
fn generate(g: &FilmGrain, pl: usize, bd: u32, luma: &Grain) -> Grain {
    let (sx, sy) = if pl == 0 { (0, 0) } else { (g.sub[0], g.sub[1]) };
    let w = if sx == 1 { 44 } else { GW };
    let h = if sy == 1 { 38 } else { GH };
    let ctr = 128i32 << (bd - 8);
    let shift = 12 - bd + g.grain_scale_shift;
    let mut seed = g.seed ^ [0, 0xb524, 0x49d8][pl];
    let mut buf = vec![vec![0i32; w]; h];
    for row in buf.iter_mut() {
        for v in row.iter_mut() {
            *v = round2(GAUSSIAN_SEQUENCE[rand(11, &mut seed) as usize] as i32, shift);
        }
    }
    let lag = g.ar_lag as i32;
    let coeffs = &g.ar_coeffs[pl];
    for y in 3..h {
        for x in 3..w - 3 {
            let mut sum = 0;
            let mut c = 0;
            'taps: for dy in -lag..=0 {
                for dx in -lag..=lag {
                    if dx == 0 && dy == 0 {
                        if pl > 0 && !g.points[0].is_empty() {
                            let lx = ((x - 3) << sx) + 3;
                            let ly = ((y - 3) << sy) + 3;
                            let mut l = 0;
                            for i in 0..=sy {
                                for j in 0..=sx {
                                    l += luma[ly + i][lx + j];
                                }
                            }
                            sum += round2(l, (sx + sy) as u32) * coeffs[c];
                        }
                        break 'taps;
                    }
                    sum += coeffs[c] * buf[(y as i32 + dy) as usize][(x as i32 + dx) as usize];
                    c += 1;
                }
            }
            buf[y][x] = (buf[y][x] + round2(sum, g.ar_shift)).clamp(-ctr, ctr - 1);
        }
    }
    buf
}

/// The scaling function of a plane, indexed by sample value.
fn scaling_lut(points: &[(u8, u8)], bd: u32) -> Vec<i32> {
    let mut s = vec![0i32; 1 << bd];
    let (Some(&(lx, ly)), Some(&(fx, fy))) = (points.last(), points.first()) else { return s };
    let sh = bd - 8;
    for v in s.iter_mut().take((fx as usize) << sh) {
        *v = fy as i32;
    }
    for w in points.windows(2) {
        let (bx, by) = (w[0].0 as i32, w[0].1 as i32);
        let dx = w[1].0 as i32 - bx;
        if dx <= 0 {
            continue;
        }
        let delta = (w[1].1 as i32 - by) * ((0x10000 + (dx >> 1)) / dx);
        let mut d = 0x8000;
        for x in 0..dx {
            s[((bx + x) as usize) << sh] = by + (d >> 16);
            d += delta;
        }
    }
    for v in s.iter_mut().skip((lx as usize) << sh) {
        *v = ly as i32;
    }
    if bd > 8 {
        let pad = 1usize << sh;
        let rnd = (pad >> 1) as i32;
        for w in points.windows(2) {
            let bx = (w[0].0 as usize) << sh;
            let ex = (w[1].0 as usize) << sh;
            for x in (bx..ex).step_by(pad) {
                let range = s[x + pad] - s[x];
                let mut r = rnd;
                for n in 1..pad {
                    r += range;
                    s[x + n] = s[x] + (r >> sh);
                }
            }
        }
    }
    s
}

fn copy(p: &Plane) -> Plane {
    Plane { data: p.data.clone(), stride: p.stride }
}

/// Grain of one 32x32 luma block (or its chroma part) at the block offsets `off` ([column][row]).
fn sample(grain: &Grain, off: &[[usize; 2]; 2], sub: [usize; 2], bx: usize, by: usize, x: usize, y: usize) -> i32 {
    let r = off[bx][by];
    let offx = 3 + (2 >> sub[0]) * (3 + (r >> 4));
    let offy = 3 + (2 >> sub[1]) * (3 + (r & 0xF));
    grain[offy + y + (BLOCK >> sub[1]) * by][offx + x + (BLOCK >> sub[0]) * bx]
}

/// The weights blending overlapped grain: two taps for luma, one pair for subsampled chroma.
fn weights(sub: usize, i: usize) -> [i32; 2] {
    if sub == 0 {
        [[27, 17], [17, 27]][i]
    } else {
        [23, 22]
    }
}

struct Noise<'a> {
    g: &'a FilmGrain,
    src: &'a [Plane; 3],
    scal: [Vec<i32>; 3],
    w: usize,
    bd: u32,
    min: i32,
    max: [i32; 3],
}

impl Noise<'_> {
    /// The output sample of plane `pl` at (`x`, `y`) with grain `gr` added.
    fn px(&self, pl: usize, x: usize, y: usize, gr: i32) -> u16 {
        let src = self.src[pl].at(x, y);
        let scale = |v: i32| self.scal[pl][v as usize];
        let g = self.g;
        let v = if pl == 0 {
            src
        } else {
            let (lx, ly) = (x << g.sub[0], y << g.sub[1]);
            let mut avg = self.src[0].at(lx, ly);
            if g.sub[0] == 1 {
                avg = (avg + self.src[0].at((lx + 1).min(self.w - 1), ly) + 1) >> 1;
            }
            if g.chroma_from_luma {
                avg
            } else {
                let uv = pl - 1;
                let combined = avg * g.mult_luma[uv] + src * g.mult[uv];
                ((combined >> 6) + g.offset[uv] * (1 << (self.bd - 8))).clamp(0, (1 << self.bd) - 1)
            }
        };
        let noise = round2(scale(v) * gr, g.scaling_shift);
        (src + noise).clamp(self.min, self.max[pl]) as u16
    }
}

/// The frame's planes with film grain added (`src` is left as the references hold it). `identity`
/// is matrix_coefficients being identity, which lifts the chroma range limit to 235.
pub fn apply(g: &FilmGrain, src: &[Plane; 3], w: usize, h: usize, bd: u32, num_planes: usize, identity: bool) -> [Plane; 3] {
    let sh = bd - 8;
    let lg = generate(g, 0, bd, &Vec::new());
    let grains = [lg.clone(), generate(g, 1, bd, &lg), generate(g, 2, bd, &lg)];
    let luma_scale = scaling_lut(&g.points[0], bd);
    let scal = if g.chroma_from_luma {
        [luma_scale.clone(), luma_scale.clone(), luma_scale]
    } else {
        [luma_scale, scaling_lut(&g.points[1], bd), scaling_lut(&g.points[2], bd)]
    };
    let (min, max) = if g.limit_range {
        let chroma_max = if identity { 235 } else { 240 };
        (16 << sh, [235 << sh, chroma_max << sh, chroma_max << sh])
    } else {
        (0, [(1 << bd) - 1; 3])
    };
    let mut out = [copy(&src[0]), copy(&src[1]), copy(&src[2])];
    let noise = Noise { g, src, scal, w, bd, min, max };
    for pl in 0..num_planes {
        if (pl == 0 && g.points[0].is_empty()) || (pl > 0 && !g.chroma_from_luma && g.points[pl].is_empty()) {
            continue;
        }
        let (sx, sy) = if pl == 0 { (0, 0) } else { (g.sub[0], g.sub[1]) };
        let pw = (w + sx) >> sx;
        let gr = &grains[pl];
        for row in 0..h.div_ceil(BLOCK) {
            let rows = 1 + (g.overlap && row > 0) as usize;
            let mut seeds = [0u32; 2];
            for (i, s) in seeds.iter_mut().enumerate().take(rows) {
                let r = (row - i) as u32;
                *s = g.seed ^ ((((r * 37 + 178) & 0xFF) << 8) | ((r * 173 + 105) & 0xFF));
            }
            let mut off = [[0usize; 2]; 2];
            let (py0, bh) = ((row * BLOCK) >> sy, ((BLOCK.min(h - row * BLOCK) + sy) >> sy));
            for bx in (0..pw).step_by(BLOCK >> sx) {
                let bw = (BLOCK >> sx).min(pw - bx);
                let ystart = if g.overlap && row > 0 { (2 >> sy).min(bh) } else { 0 };
                let xstart = if g.overlap && bx > 0 { (2 >> sx).min(bw) } else { 0 };
                if g.overlap && bx > 0 {
                    for i in 0..rows {
                        off[1][i] = off[0][i];
                    }
                }
                for i in 0..rows {
                    off[0][i] = rand(8, &mut seeds[i]) as usize;
                }
                let mut put = |x: usize, y: usize, v: i32| {
                    let (px, py) = (bx + x, py0 + y);
                    out[pl].data[py * out[pl].stride + px] = noise.px(pl, px, py, v);
                };
                let s = |bxf, byf, x, y| sample(gr, &off, [sx, sy], bxf, byf, x, y);
                let clip = |v: i32| v.clamp(-(128 << sh), (128 << sh) - 1);
                for y in ystart..bh {
                    for x in xstart..bw {
                        put(x, y, s(0, 0, x, y));
                    }
                    for x in 0..xstart {
                        let [w0, w1] = weights(sx, x);
                        put(x, y, clip(round2(s(1, 0, x, y) * w0 + s(0, 0, x, y) * w1, 5)));
                    }
                }
                for y in 0..ystart {
                    let [v0, v1] = weights(sy, y);
                    for x in xstart..bw {
                        put(x, y, clip(round2(s(0, 1, x, y) * v0 + s(0, 0, x, y) * v1, 5)));
                    }
                    for x in 0..xstart {
                        let [w0, w1] = weights(sx, x);
                        let top = clip(round2(s(1, 1, x, y) * w0 + s(0, 1, x, y) * w1, 5));
                        let left = clip(round2(s(1, 0, x, y) * w0 + s(0, 0, x, y) * w1, 5));
                        put(x, y, clip(round2(top * v0 + left * v1, 5)));
                    }
                }
            }
        }
    }
    out
}
