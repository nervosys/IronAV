//! Super-resolution (spec 7.16): a frame is decoded at its downscaled width and upscaled here, after
//! CDEF and before loop restoration. The filter is libaom's av1_resize_filter_normative (BSD-2-Clause,
//! Alliance for Open Media).

use super::block::FrameDec;
use super::ipred::Plane;

const RESIZE_FILTER: [[i16; 8]; 64] = [
    [0, 0, 0, 128, 0, 0, 0, 0],
    [0, 0, -1, 128, 2, -1, 0, 0],
    [0, 1, -3, 127, 4, -2, 1, 0],
    [0, 1, -4, 127, 6, -3, 1, 0],
    [0, 2, -6, 126, 8, -3, 1, 0],
    [0, 2, -7, 125, 11, -4, 1, 0],
    [-1, 2, -8, 125, 13, -5, 2, 0],
    [-1, 3, -9, 124, 15, -6, 2, 0],
    [-1, 3, -10, 123, 18, -6, 2, -1],
    [-1, 3, -11, 122, 20, -7, 3, -1],
    [-1, 4, -12, 121, 22, -8, 3, -1],
    [-1, 4, -13, 120, 25, -9, 3, -1],
    [-1, 4, -14, 118, 28, -9, 3, -1],
    [-1, 4, -15, 117, 30, -10, 4, -1],
    [-1, 5, -16, 116, 32, -11, 4, -1],
    [-1, 5, -16, 114, 35, -12, 4, -1],
    [-1, 5, -17, 112, 38, -12, 4, -1],
    [-1, 5, -18, 111, 40, -13, 5, -1],
    [-1, 5, -18, 109, 43, -14, 5, -1],
    [-1, 6, -19, 107, 45, -14, 5, -1],
    [-1, 6, -19, 105, 48, -15, 5, -1],
    [-1, 6, -19, 103, 51, -16, 5, -1],
    [-1, 6, -20, 101, 53, -16, 6, -1],
    [-1, 6, -20, 99, 56, -17, 6, -1],
    [-1, 6, -20, 97, 58, -17, 6, -1],
    [-1, 6, -20, 95, 61, -18, 6, -1],
    [-2, 7, -20, 93, 64, -18, 6, -2],
    [-2, 7, -20, 91, 66, -19, 6, -1],
    [-2, 7, -20, 88, 69, -19, 6, -1],
    [-2, 7, -20, 86, 71, -19, 6, -1],
    [-2, 7, -20, 84, 74, -20, 7, -2],
    [-2, 7, -20, 81, 76, -20, 7, -1],
    [-2, 7, -20, 79, 79, -20, 7, -2],
    [-1, 7, -20, 76, 81, -20, 7, -2],
    [-2, 7, -20, 74, 84, -20, 7, -2],
    [-1, 6, -19, 71, 86, -20, 7, -2],
    [-1, 6, -19, 69, 88, -20, 7, -2],
    [-1, 6, -19, 66, 91, -20, 7, -2],
    [-2, 6, -18, 64, 93, -20, 7, -2],
    [-1, 6, -18, 61, 95, -20, 6, -1],
    [-1, 6, -17, 58, 97, -20, 6, -1],
    [-1, 6, -17, 56, 99, -20, 6, -1],
    [-1, 6, -16, 53, 101, -20, 6, -1],
    [-1, 5, -16, 51, 103, -19, 6, -1],
    [-1, 5, -15, 48, 105, -19, 6, -1],
    [-1, 5, -14, 45, 107, -19, 6, -1],
    [-1, 5, -14, 43, 109, -18, 5, -1],
    [-1, 5, -13, 40, 111, -18, 5, -1],
    [-1, 4, -12, 38, 112, -17, 5, -1],
    [-1, 4, -12, 35, 114, -16, 5, -1],
    [-1, 4, -11, 32, 116, -16, 5, -1],
    [-1, 4, -10, 30, 117, -15, 4, -1],
    [-1, 3, -9, 28, 118, -14, 4, -1],
    [-1, 3, -9, 25, 120, -13, 4, -1],
    [-1, 3, -8, 22, 121, -12, 4, -1],
    [-1, 3, -7, 20, 122, -11, 3, -1],
    [-1, 2, -6, 18, 123, -10, 3, -1],
    [0, 2, -6, 15, 124, -9, 3, -1],
    [0, 2, -5, 13, 125, -8, 2, -1],
    [0, 1, -4, 11, 125, -7, 2, 0],
    [0, 1, -3, 8, 126, -6, 2, 0],
    [0, 1, -3, 6, 127, -4, 1, 0],
    [0, 1, -2, 4, 127, -3, 1, 0],
    [0, 0, -1, 2, 128, -1, 0, 0],
];

const SCALE_BITS: u32 = 14;

/// Upscales the planes of a frame decoded at its downscaled width (`f.h.width`) to the upscaled
/// width (`f.h.upscaled_width`). `planes` holds the frame's planes, or a copy of them (the
/// restoration filter reads the deblocked planes too). Does nothing without superres.
pub fn upscale(f: &FrameDec, planes: &mut [Plane; 3]) {
    if f.h.superres_denom == 8 {
        return;
    }
    let up_w = f.h.upscaled_width as usize;
    let max_v = (1i32 << f.bd) - 1;
    let one = (1i64 << SCALE_BITS) - 1;
    for pl in 0..f.num_planes {
        let (sx, sy) = if pl == 0 { (0, 0) } else { (f.ss[0], f.ss[1]) };
        let down_w = (f.h.width as usize + sx) >> sx;
        let ups_w = (up_w + sx) >> sx;
        let rows = (f.h.height as usize + sy) >> sy;
        let max_x = ((f.h.mi_cols >> sx) * 4 - 1) as i64;
        let (dw, uw) = (down_w as i64, ups_w as i64);
        let step = ((dw << SCALE_BITS) + uw / 2) / uw;
        let err = uw * step - (dw << SCALE_BITS);
        // The position of output sample 0, in 1/2^14 downscaled samples, not reduced to its fraction:
        // the centre of output sample 0 falls just before the first downscaled sample (at about
        // -(uw - dw) / (2 uw)), so the first outputs filter the samples before the first one, which
        // clamp to the edge. Reducing it to its fraction moves every output sample one sample right,
        // and the output then differs from libaom's (checked against libaom's decoder).
        let x0 = (-((uw - dw) << (SCALE_BITS - 1)) + uw / 2) / uw + 128 - err / 2;
        let src = &planes[pl];
        let stride = ((up_w + 63) & !63) >> sx;
        let mut dst = Plane::new(stride, src.data.len() / src.stride);
        for y in 0..rows {
            let s = &src.data[y * src.stride..(y + 1) * src.stride];
            for x in 0..ups_w {
                let pos = x0 + x as i64 * step;
                let base = (pos >> SCALE_BITS) - 3;
                let filt = &RESIZE_FILTER[((pos & one) >> 8) as usize];
                let mut sum = 0i32;
                for (k, &t) in filt.iter().enumerate() {
                    sum += s[(base + k as i64).clamp(0, max_x) as usize] as i32 * t as i32;
                }
                dst.data[y * stride + x] = ((sum + 64) >> 7).clamp(0, max_v) as u16;
            }
        }
        planes[pl] = dst;
    }
}

/// Upscales the frame's own planes in place.
pub fn upscale_in_place(f: &mut FrameDec) {
    if f.h.superres_denom == 8 {
        return;
    }
    let empty = [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)];
    let mut planes = std::mem::replace(&mut f.planes, empty);
    upscale(f, &mut planes);
    f.planes = planes;
}

/// The downscaled width of a frame of upscaled width `up` (spec 5.9.8): FrameWidth.
pub fn downscaled_width(up: u32, denom: u32) -> u32 {
    (up * 8 + denom / 2) / denom
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each filter phase is a unit-gain filter: its taps sum to 128.
    #[test]
    fn filter_phases_have_unit_gain() {
        for (i, phase) in RESIZE_FILTER.iter().enumerate() {
            assert_eq!(phase.iter().map(|&t| t as i32).sum::<i32>(), 128, "phase {i}");
        }
    }

    #[test]
    fn downscaled_width_rounds_to_nearest() {
        assert_eq!(downscaled_width(352, 16), 176);
        assert_eq!(downscaled_width(352, 9), 313);
        assert_eq!(downscaled_width(177, 12), 118);
    }
}
