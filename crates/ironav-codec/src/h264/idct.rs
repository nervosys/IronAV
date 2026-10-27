//! Inverse transforms (ITU-T H.264 8.5). Coefficients are in raster order (row * N + column).

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Inverse 4x4 transform of scaled coefficients, added to the prediction already in `dst`.
pub fn idct4_add(c: &[i32; 16], dst: &mut [u8], stride: usize) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let d = &c[i * 4..i * 4 + 4];
        let e0 = d[0].wrapping_add(d[2]);
        let e1 = d[0].wrapping_sub(d[2]);
        let e2 = (d[1] >> 1).wrapping_sub(d[3]);
        let e3 = d[1].wrapping_add(d[3] >> 1);
        t[i * 4] = e0.wrapping_add(e3);
        t[i * 4 + 1] = e1.wrapping_add(e2);
        t[i * 4 + 2] = e1.wrapping_sub(e2);
        t[i * 4 + 3] = e0.wrapping_sub(e3);
    }
    for j in 0..4 {
        let g0 = t[j].wrapping_add(t[8 + j]);
        let g1 = t[j].wrapping_sub(t[8 + j]);
        let g2 = (t[4 + j] >> 1).wrapping_sub(t[12 + j]);
        let g3 = t[4 + j].wrapping_add(t[12 + j] >> 1);
        let h = [g0.wrapping_add(g3), g1.wrapping_add(g2), g1.wrapping_sub(g2), g0.wrapping_sub(g3)];
        for (i, v) in h.iter().enumerate() {
            let p = &mut dst[i * stride + j];
            *p = clip(*p as i32 + (v.wrapping_add(32) >> 6));
        }
    }
}

fn row8(d: [i32; 8]) -> [i32; 8] {
    let a0 = d[0].wrapping_add(d[4]);
    let a4 = d[0].wrapping_sub(d[4]);
    let a2 = (d[2] >> 1).wrapping_sub(d[6]);
    let a6 = d[2].wrapping_add(d[6] >> 1);
    let b0 = a0.wrapping_add(a6);
    let b2 = a4.wrapping_add(a2);
    let b4 = a4.wrapping_sub(a2);
    let b6 = a0.wrapping_sub(a6);
    let a1 = d[5].wrapping_sub(d[3]).wrapping_sub(d[7]).wrapping_sub(d[7] >> 1);
    let a3 = d[1].wrapping_add(d[7]).wrapping_sub(d[3]).wrapping_sub(d[3] >> 1);
    let a5 = d[7].wrapping_sub(d[1]).wrapping_add(d[5]).wrapping_add(d[5] >> 1);
    let a7 = d[3].wrapping_add(d[5]).wrapping_add(d[1]).wrapping_add(d[1] >> 1);
    let b1 = a1.wrapping_add(a7 >> 2);
    let b7 = a7.wrapping_sub(a1 >> 2);
    let b3 = a3.wrapping_add(a5 >> 2);
    let b5 = (a3 >> 2).wrapping_sub(a5);
    [b0.wrapping_add(b7), b2.wrapping_add(b5), b4.wrapping_add(b3), b6.wrapping_add(b1), b6.wrapping_sub(b1), b4.wrapping_sub(b3), b2.wrapping_sub(b5), b0.wrapping_sub(b7)]
}

/// Inverse 8x8 transform of scaled coefficients, added to the prediction in `dst`.
pub fn idct8_add(c: &[i32; 64], dst: &mut [u8], stride: usize) {
    let mut t = [0i32; 64];
    for i in 0..8 {
        let mut d = [0i32; 8];
        d.copy_from_slice(&c[i * 8..i * 8 + 8]);
        t[i * 8..i * 8 + 8].copy_from_slice(&row8(d));
    }
    for j in 0..8 {
        let mut d = [0i32; 8];
        for i in 0..8 {
            d[i] = t[i * 8 + j];
        }
        let h = row8(d);
        for (i, v) in h.iter().enumerate() {
            let p = &mut dst[i * stride + j];
            *p = clip(*p as i32 + (v.wrapping_add(32) >> 6));
        }
    }
}

/// Inverse 4x4 Hadamard transform of the Intra16x16 luma DC coefficients (before scaling).
pub fn hadamard4(c: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let d = &c[i * 4..i * 4 + 4];
        let (s0, s1, s2, s3) = (d[0].wrapping_add(d[1]), d[0].wrapping_sub(d[1]), d[2].wrapping_add(d[3]), d[2].wrapping_sub(d[3]));
        t[i * 4] = s0.wrapping_add(s2);
        t[i * 4 + 1] = s0.wrapping_sub(s2);
        t[i * 4 + 2] = s1.wrapping_sub(s3);
        t[i * 4 + 3] = s1.wrapping_add(s3);
    }
    let mut o = [0i32; 16];
    for j in 0..4 {
        let (s0, s1, s2, s3) = (t[j].wrapping_add(t[4 + j]), t[j].wrapping_sub(t[4 + j]), t[8 + j].wrapping_add(t[12 + j]), t[8 + j].wrapping_sub(t[12 + j]));
        o[j] = s0.wrapping_add(s2);
        o[4 + j] = s0.wrapping_sub(s2);
        o[8 + j] = s1.wrapping_sub(s3);
        o[12 + j] = s1.wrapping_add(s3);
    }
    o
}
