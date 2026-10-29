//! Cheap measures of how a picture differs from the one before it, on luma averaged over 4x4
//! blocks. The H.264 encoder uses them to start an IDR picture at a scene cut, and the analyzers
//! to report cuts, motion and frozen pictures, so both see the same thing.

/// A picture predicted from the last one this much as badly as from its own block means starts a scene.
pub const SCENE_CUT: f64 = 0.8;

/// Luma averaged over 4x4 blocks, `w` x `h` of them.
#[derive(Debug, Clone)]
pub struct LowRes {
    pub w: usize,
    pub h: usize,
    pub data: Vec<i32>,
}

impl LowRes {
    /// `sample(x, y)` gives the luma at (x, y) for every x below `w * 4` and y below `h * 4`.
    pub fn new(w: usize, h: usize, sample: impl Fn(usize, usize) -> i32) -> Self {
        let mut data = vec![0; w * h];
        for (i, o) in data.iter_mut().enumerate() {
            let (x0, y0) = (i % w * 4, i / w * 4);
            *o = (0..16).map(|k| sample(x0 + k % 4, y0 + k / 4)).sum::<i32>() >> 4;
        }
        LowRes { w, h, data }
    }

    /// The low-resolution luma of an 8-bit or 16-bit (little-endian) plane, `stride` samples wide,
    /// scaled to 8 bits when `shift` is the bits to drop.
    pub fn of_plane(plane: &[u8], stride: usize, bytes: usize, shift: u32, width: usize, height: usize) -> Self {
        if bytes == 2 {
            Self::new(width / 4, height / 4, |x, y| {
                let o = (y * stride + x) * 2;
                (u16::from_le_bytes([plane[o], plane[o + 1]]) >> shift) as i32
            })
        } else {
            Self::new(width / 4, height / 4, |x, y| plane[y * stride + x] as i32)
        }
    }

    /// Mean absolute difference from `prev`, in 8-bit levels (0 when the sizes differ).
    pub fn mean_abs_diff(&self, prev: &LowRes) -> f64 {
        if prev.data.len() != self.data.len() || self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().zip(&prev.data).map(|(a, b)| (a - b).abs() as i64).sum::<i64>() as f64 / self.data.len() as f64
    }
}

/// How well `cur` is predicted from `prev` against from itself: the summed absolute error of its
/// 8x8 blocks, each moved up to 4 samples, from `prev` (inter), and from their own means (intra).
/// `inter` well above a fraction of `intra` means the picture starts a new scene.
pub fn change(prev: &LowRes, cur: &LowRes) -> (i64, i64) {
    let (lw, lh) = (cur.w, cur.h);
    let (cur, prev) = (&cur.data, &prev.data);
    let (mut inter, mut intra) = (0i64, 0i64);
    if prev.len() != cur.len() {
        return (1, 0);
    }
    for by in (0..lh.saturating_sub(7)).step_by(8) {
        for bx in (0..lw.saturating_sub(7)).step_by(8) {
            let at = |x: usize, y: usize| cur[(by + y) * lw + bx + x];
            let mean = (0..64).map(|k| at(k % 8, k / 8)).sum::<i32>() / 64;
            intra += (0..64).map(|k| (at(k % 8, k / 8) - mean).abs() as i64).sum::<i64>();
            let mut best = i64::MAX;
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (ox, oy) = (bx as i32 + dx, by as i32 + dy);
                    if ox < 0 || oy < 0 || ox as usize + 8 > lw || oy as usize + 8 > lh {
                        continue;
                    }
                    let sad = (0..64).map(|k| (at(k % 8, k / 8) - prev[(oy as usize + k / 8) * lw + ox as usize + k % 8]).abs() as i64).sum::<i64>();
                    best = best.min(sad);
                }
            }
            inter += best;
        }
    }
    (inter, intra)
}

/// [`change`] as one number: inter over intra, so 0 is a still picture and above [`SCENE_CUT`] a cut.
pub fn change_score(prev: &LowRes, cur: &LowRes) -> f64 {
    let (inter, intra) = change(prev, cur);
    inter as f64 / intra.max(1) as f64
}
