//! Motion compensation of MPEG-1 and MPEG-2: half-sample bilinear prediction of a block from a
//! frame or from one field of a frame, put into the picture or averaged into what is there (the
//! second direction of a B macroblock, or the second prediction of dual prime). The rounding
//! follows FFmpeg's `hpeldsp` (`put_pixels` and `avg_pixels`, Michael Niedermayer and the FFmpeg
//! developers, LGPL-2.1-or-later).

/// One plane of a frame, at the macroblock-aligned size.
#[derive(Clone)]
pub(crate) struct Plane {
    pub d: Vec<u8>,
    pub stride: usize,
    pub h: usize,
}

impl Plane {
    pub fn new(stride: usize, h: usize, fill: u8) -> Self {
        Plane { d: vec![fill; stride * h], stride, h }
    }
}

/// Which rows of a plane a picture or a prediction works on: all of them (`step` 1), or the
/// rows of one field (`step` 2, starting at row `par`).
#[derive(Clone, Copy)]
pub(crate) struct View {
    pub par: usize,
    pub step: usize,
}

impl View {
    pub const FRAME: View = View { par: 0, step: 1 };

    pub fn field(parity: usize) -> View {
        View { par: parity, step: 2 }
    }

    /// The index in `p` of the sample at column `x` and row `y` of the view.
    #[inline]
    pub fn at(self, p: &Plane, x: usize, y: usize) -> usize {
        (self.par + y * self.step) * p.stride + x
    }
}

/// Predicts the `bw` by `bh` block at (`dx`, `dy`) of `dst`'s view from the block whose top left
/// sample is (`sx`, `sy`) in `src`'s view, `fx` and `fy` being the half-sample fractions. Samples
/// outside the reference repeat its edge. `avg` averages the prediction into `dst`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn block(dst: &mut Plane, dv: View, dx: usize, dy: usize, src: &Plane, sv: View, sx: i32, sy: i32, fx: usize, fy: usize, bw: usize, bh: usize, avg: bool) {
    // the (bh + 1) by (bw + 1) samples the filter can read, gathered from the reference
    let mut g = [0u8; 17 * 17];
    let gw = bw + 1;
    let (w, vh) = (src.stride as i32, (src.h / sv.step) as i32);
    let (rows, cols) = (bh + fy, bw + fx);
    if sx >= 0 && sy >= 0 && sx + cols as i32 <= w && sy + rows as i32 <= vh {
        for r in 0..rows {
            let o = sv.at(src, sx as usize, sy as usize + r);
            g[r * gw..r * gw + cols].copy_from_slice(&src.d[o..o + cols]);
        }
    } else {
        for r in 0..rows {
            let y = (sy + r as i32).clamp(0, vh - 1) as usize;
            for c in 0..cols {
                let x = (sx + c as i32).clamp(0, w - 1) as usize;
                g[r * gw + c] = src.d[sv.at(src, x, y)];
            }
        }
    }
    let mut out = [0u8; 16 * 16];
    match (fx, fy) {
        (0, 0) => {
            for r in 0..bh {
                out[r * 16..r * 16 + bw].copy_from_slice(&g[r * gw..r * gw + bw]);
            }
        }
        (1, 0) => {
            for r in 0..bh {
                for c in 0..bw {
                    out[r * 16 + c] = ((g[r * gw + c] as u32 + g[r * gw + c + 1] as u32 + 1) >> 1) as u8;
                }
            }
        }
        (0, _) => {
            for r in 0..bh {
                for c in 0..bw {
                    out[r * 16 + c] = ((g[r * gw + c] as u32 + g[(r + 1) * gw + c] as u32 + 1) >> 1) as u8;
                }
            }
        }
        _ => {
            for r in 0..bh {
                for c in 0..bw {
                    let (a, b) = (r * gw + c, (r + 1) * gw + c);
                    out[r * 16 + c] = ((g[a] as u32 + g[a + 1] as u32 + g[b] as u32 + g[b + 1] as u32 + 2) >> 2) as u8;
                }
            }
        }
    }
    for r in 0..bh {
        let o = dv.at(dst, dx, dy + r);
        let d = &mut dst.d[o..o + bw];
        let s = &out[r * 16..r * 16 + bw];
        if avg {
            for (a, &b) in d.iter_mut().zip(s) {
                *a = ((*a as u32 + b as u32 + 1) >> 1) as u8;
            }
        } else {
            d.copy_from_slice(s);
        }
    }
}
