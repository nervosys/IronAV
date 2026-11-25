//! Parameter-grid projection from 3GPP SP-040428, Tables 8.19 and 8.20.

use super::{Par, MAX_PAR};

/// A source band and the relative weights of consecutive bands starting there.
type Row = (usize, &'static [u8]);

const fn copy(band: usize) -> Row {
    (band, &[1])
}

const fn average(first: usize) -> Row {
    (first, &[1, 1])
}

const TO_34: [Row; 34] = [
    copy(0), average(0), copy(1), copy(2), average(2), copy(3),
    copy(4), copy(4), copy(5), copy(5), copy(6), copy(7),
    copy(8), copy(8), copy(9), copy(9), copy(10), copy(11),
    copy(12), copy(13), copy(14), copy(14), copy(15), copy(15),
    copy(16), copy(16), copy(17), copy(17), copy(18), copy(18),
    copy(18), copy(18), copy(19), copy(19),
];

const TO_20: [Row; 20] = [
    (0, &[2, 1]), (1, &[1, 2]), (3, &[2, 1]), (4, &[1, 2]),
    average(6), average(8), copy(10), copy(11), average(12), average(14),
    copy(16), copy(17), copy(18), copy(19), average(20), average(22),
    average(24), average(26), (28, &[1, 1, 1, 1]), average(32),
];

fn weighted((first, weights): Row, at: impl Fn(usize) -> f32) -> (f32, f32) {
    let mut total = 0.0;
    let mut weight = 0.0;
    for (i, &w) in weights.iter().enumerate() {
        total += f32::from(w) * at(first + i);
        weight += f32::from(w);
    }
    (total, weight)
}

/// Map the coded envelope parameters onto the active hybrid layout. The phase parameters
/// occupy only the low-band prefix of each grid. Uncoded source phase bands contribute zero.
pub(super) fn remap(par: &Par, coded: usize, envelopes: usize, wide: bool, full: bool) -> Par {
    let mut remapped = *par;
    let target_count = match (wide, full) {
        (false, true) => 20,
        (false, false) => 11,
        (true, true) => 34,
        (true, false) => 17,
    };
    let (projection, expand_pairs): (&[Row], bool) = match (wide, coded) {
        (true, 5 | 10) => (&TO_34, true),
        (true, 11 | 20) => (&TO_34, false),
        (false, 17 | 34) => (&TO_20, false),
        (false, 5 | 10) => {
            for (src, dst) in par.iter().zip(&mut remapped).take(envelopes) {
                for (band, value) in dst[..target_count].iter_mut().enumerate() {
                    *value = src[..coded].get(band / 2).copied().unwrap_or_default();
                }
            }
            return remapped;
        }
        _ => return remapped,
    };
    for (src, dst) in par.iter().zip(&mut remapped).take(envelopes) {
        for (value, &row) in dst.iter_mut().zip(&projection[..target_count]) {
            let (total, weight) = weighted(row, |i| {
                let band = if expand_pairs { i / 2 } else { i };
                f32::from(src[..coded].get(band).copied().unwrap_or_default())
            });
            // The weighted sum is an exact integer here. Conversion truncates toward zero.
            *value = (total / weight) as i8;
        }
    }
    remapped
}

/// Project the previous envelope's mixing coefficients after the grid changes.
pub(super) fn project_coefficients(coefficients: &mut [f32; MAX_PAR], wide: bool) {
    let previous = *coefficients;
    let projection: &[Row] = if wide { &TO_34 } else { &TO_20 };
    for (coefficient, &row) in coefficients.iter_mut().zip(projection) {
        let (total, weight) = weighted(row, |i| previous[i]);
        *coefficient = total * weight.recip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::ps::MAX_ENV;

    #[test]
    fn mapping_averages_signed_indices_before_dequantizing() {
        let mut par = [[0; MAX_PAR]; MAX_ENV];
        par[0][..6].copy_from_slice(&[-4, -1, 3, 6, 3, -8]);
        let wide = remap(&par, 20, 1, true, true);
        assert_eq!(&wide[0][..6], &[-4, -2, -1, 3, 4, 6]);
        let narrow = remap(&par, 34, 1, false, true);
        assert_eq!(&narrow[0][..4], &[-3, 1, 5, -4]);
    }

    #[test]
    fn phase_projection_uses_only_the_transmitted_bands() {
        let mut par = [[42; MAX_PAR]; MAX_ENV];
        par[0][..5].copy_from_slice(&[1, 2, 3, 4, 5]);
        let narrow = remap(&par, 5, 1, false, false);
        assert_eq!(&narrow[0][..11], &[1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 0]);
        let wide = remap(&par, 5, 1, true, false);
        assert_eq!(&wide[0][..17], &[1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 5, 5, 0]);
        assert_eq!(wide[1], par[1]);
    }

    #[test]
    fn mixing_coefficients_keep_their_fractional_averages() {
        let mut coefficients = std::array::from_fn(|i| i as f32);
        project_coefficients(&mut coefficients, true);
        assert_eq!(&coefficients[..7], &[0.0, 0.5, 1.0, 2.0, 2.5, 3.0, 4.0]);
        project_coefficients(&mut coefficients, false);
        assert_eq!(coefficients[0], 1.0 / 6.0);
        assert_eq!(coefficients[18], 18.0);
    }
}
