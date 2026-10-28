//! VP9 inverse transforms (DCT, ADST, Walsh-Hadamard), translated from FFmpeg's vp9dsp_template.c.

#[inline]
fn r14(x: i64) -> i64 {
    (x + (1 << 13)) >> 14
}

fn idct4(v: &[i64], out: &mut [i64]) {
    let t0 = r14((v[0] + v[2]) * 11585);
    let t1 = r14((v[0] - v[2]) * 11585);
    let t2 = r14(v[1] * 6270 - v[3] * 15137);
    let t3 = r14(v[1] * 15137 + v[3] * 6270);

    out[0] = t0 + t3;
    out[1] = t1 + t2;
    out[2] = t1 - t2;
    out[3] = t0 - t3;
}

fn iadst4(v: &[i64], out: &mut [i64]) {
    let t0 = 5283 * v[0] + 15212 * v[2] + 9929 * v[3];
    let t1 = 9929 * v[0] - 5283 * v[2] - 15212 * v[3];
    let t2 = 13377 * (v[0] - v[2] + v[3]);
    let t3 = 13377 * v[1];

    out[0] = r14(t0 + t3);
    out[1] = r14(t1 + t3);
    out[2] = r14(t2);
    out[3] = r14(t0 + t1 - t3);
}

fn idct8(v: &[i64], out: &mut [i64]) {
    let t0a = r14((v[0] + v[4]) * 11585);
    let t1a = r14((v[0] - v[4]) * 11585);
    let t2a = r14(v[2] * 6270 - v[6] * 15137);
    let t3a = r14(v[2] * 15137 + v[6] * 6270);
    let t4a = r14(v[1] * 3196 - v[7] * 16069);
    let t5a = r14(v[5] * 13623 - v[3] * 9102);
    let t6a = r14(v[5] * 9102 + v[3] * 13623);
    let t7a = r14(v[1] * 16069 + v[7] * 3196);

    let t0 = t0a + t3a;
    let t1 = t1a + t2a;
    let t2 = t1a - t2a;
    let t3 = t0a - t3a;
    let t4 = t4a + t5a;
    let t5a = t4a - t5a;
    let t7 = t7a + t6a;
    let t6a = t7a - t6a;

    let t5 = r14((t6a - t5a) * 11585);
    let t6 = r14((t6a + t5a) * 11585);

    out[0] = t0 + t7;
    out[1] = t1 + t6;
    out[2] = t2 + t5;
    out[3] = t3 + t4;
    out[4] = t3 - t4;
    out[5] = t2 - t5;
    out[6] = t1 - t6;
    out[7] = t0 - t7;
}

fn iadst8(v: &[i64], out: &mut [i64]) {
    let t0a = 16305 * v[7] + 1606 * v[0];
    let t1a = 1606 * v[7] - 16305 * v[0];
    let t2a = 14449 * v[5] + 7723 * v[2];
    let t3a = 7723 * v[5] - 14449 * v[2];
    let t4a = 10394 * v[3] + 12665 * v[4];
    let t5a = 12665 * v[3] - 10394 * v[4];
    let t6a = 4756 * v[1] + 15679 * v[6];
    let t7a = 15679 * v[1] - 4756 * v[6];

    let t0 = r14(t0a + t4a);
    let t1 = r14(t1a + t5a);
    let t2 = r14(t2a + t6a);
    let t3 = r14(t3a + t7a);
    let t4 = r14(t0a - t4a);
    let t5 = r14(t1a - t5a);
    let t6 = r14(t2a - t6a);
    let t7 = r14(t3a - t7a);

    let t4a = 15137 * t4 + 6270 * t5;
    let t5a = 6270 * t4 - 15137 * t5;
    let t6a = 15137 * t7 - 6270 * t6;
    let t7a = 6270 * t7 + 15137 * t6;

    out[0] = t0 + t2;
    out[7] = -(t1 + t3);
    let t2 = t0 - t2;
    let t3 = t1 - t3;

    out[1] = -r14(t4a + t6a);
    out[6] = r14(t5a + t7a);
    let t6 = r14(t4a - t6a);
    let t7 = r14(t5a - t7a);

    out[3] = -r14((t2 + t3) * 11585);
    out[4] = r14((t2 - t3) * 11585);
    out[2] = r14((t6 + t7) * 11585);
    out[5] = -r14((t6 - t7) * 11585);
}

fn idct16(v: &[i64], out: &mut [i64]) {
    let mut t0a = r14((v[0] + v[8]) * 11585);
    let mut t1a = r14((v[0] - v[8]) * 11585);
    let mut t2a = r14(v[4] * 6270 - v[12] * 15137);
    let mut t3a = r14(v[4] * 15137 + v[12] * 6270);
    let t4a = r14(v[2] * 3196 - v[14] * 16069);
    let t7a = r14(v[2] * 16069 + v[14] * 3196);
    let mut t5a = r14(v[10] * 13623 - v[6] * 9102);
    let mut t6a = r14(v[10] * 9102 + v[6] * 13623);
    let mut t8a = r14(v[1] * 1606 - v[15] * 16305);
    let mut t15a = r14(v[1] * 16305 + v[15] * 1606);
    let mut t9a = r14(v[9] * 12665 - v[7] * 10394);
    let mut t14a = r14(v[9] * 10394 + v[7] * 12665);
    let mut t10a = r14(v[5] * 7723 - v[11] * 14449);
    let mut t13a = r14(v[5] * 14449 + v[11] * 7723);
    let mut t11a = r14(v[13] * 15679 - v[3] * 4756);
    let mut t12a = r14(v[13] * 4756 + v[3] * 15679);

    let t0 = t0a + t3a;
    let t1 = t1a + t2a;
    let t2 = t1a - t2a;
    let t3 = t0a - t3a;
    let mut t4 = t4a + t5a;
    let mut t5 = t4a - t5a;
    let mut t6 = t7a - t6a;
    let mut t7 = t7a + t6a;
    let t8 = t8a + t9a;
    let mut t9 = t8a - t9a;
    let mut t10 = t11a - t10a;
    let mut t11 = t11a + t10a;
    let mut t12 = t12a + t13a;
    let mut t13 = t12a - t13a;
    let mut t14 = t15a - t14a;
    let t15 = t15a + t14a;

    t5a = r14((t6 - t5) * 11585);
    t6a = r14((t6 + t5) * 11585);
    t9a = r14(t14 * 6270 - t9 * 15137);
    t14a = r14(t14 * 15137 + t9 * 6270);
    t10a = r14(-(t13 * 15137 + t10 * 6270));
    t13a = r14(t13 * 6270 - t10 * 15137);

    t0a = t0 + t7;
    t1a = t1 + t6a;
    t2a = t2 + t5a;
    t3a = t3 + t4;
    t4 = t3 - t4;
    t5 = t2 - t5a;
    t6 = t1 - t6a;
    t7 = t0 - t7;
    t8a = t8 + t11;
    t9 = t9a + t10a;
    t10 = t9a - t10a;
    t11a = t8 - t11;
    t12a = t15 - t12;
    t13 = t14a - t13a;
    t14 = t14a + t13a;
    t15a = t15 + t12;

    t10a = r14((t13 - t10) * 11585);
    t13a = r14((t13 + t10) * 11585);
    t11 = r14((t12a - t11a) * 11585);
    t12 = r14((t12a + t11a) * 11585);

    out[0] = t0a + t15a;
    out[1] = t1a + t14;
    out[2] = t2a + t13a;
    out[3] = t3a + t12;
    out[4] = t4 + t11;
    out[5] = t5 + t10a;
    out[6] = t6 + t9;
    out[7] = t7 + t8a;
    out[8] = t7 - t8a;
    out[9] = t6 - t9;
    out[10] = t5 - t10a;
    out[11] = t4 - t11;
    out[12] = t3a - t12;
    out[13] = t2a - t13a;
    out[14] = t1a - t14;
    out[15] = t0a - t15a;
}

fn iadst16(v: &[i64], out: &mut [i64]) {
    let mut t0 = v[15] * 16364 + v[0] * 804;
    let mut t1 = v[15] * 804 - v[0] * 16364;
    let mut t2 = v[13] * 15893 + v[2] * 3981;
    let mut t3 = v[13] * 3981 - v[2] * 15893;
    let mut t4 = v[11] * 14811 + v[4] * 7005;
    let mut t5 = v[11] * 7005 - v[4] * 14811;
    let mut t6 = v[9] * 13160 + v[6] * 9760;
    let mut t7 = v[9] * 9760 - v[6] * 13160;
    let mut t8 = v[7] * 11003 + v[8] * 12140;
    let mut t9 = v[7] * 12140 - v[8] * 11003;
    let mut t10 = v[5] * 8423 + v[10] * 14053;
    let mut t11 = v[5] * 14053 - v[10] * 8423;
    let mut t12 = v[3] * 5520 + v[12] * 15426;
    let mut t13 = v[3] * 15426 - v[12] * 5520;
    let mut t14 = v[1] * 2404 + v[14] * 16207;
    let mut t15 = v[1] * 16207 - v[14] * 2404;

    let t0a = r14(t0 + t8);
    let t1a = r14(t1 + t9);
    let t2a = r14(t2 + t10);
    let t3a = r14(t3 + t11);
    let mut t4a = r14(t4 + t12);
    let mut t5a = r14(t5 + t13);
    let mut t6a = r14(t6 + t14);
    let mut t7a = r14(t7 + t15);
    let mut t8a = r14(t0 - t8);
    let mut t9a = r14(t1 - t9);
    let mut t10a = r14(t2 - t10);
    let mut t11a = r14(t3 - t11);
    let mut t12a = r14(t4 - t12);
    let mut t13a = r14(t5 - t13);
    let mut t14a = r14(t6 - t14);
    let mut t15a = r14(t7 - t15);

    t8 = t8a * 16069 + t9a * 3196;
    t9 = t8a * 3196 - t9a * 16069;
    t10 = t10a * 9102 + t11a * 13623;
    t11 = t10a * 13623 - t11a * 9102;
    t12 = t13a * 16069 - t12a * 3196;
    t13 = t13a * 3196 + t12a * 16069;
    t14 = t15a * 9102 - t14a * 13623;
    t15 = t15a * 13623 + t14a * 9102;

    t0 = t0a + t4a;
    t1 = t1a + t5a;
    t2 = t2a + t6a;
    t3 = t3a + t7a;
    t4 = t0a - t4a;
    t5 = t1a - t5a;
    t6 = t2a - t6a;
    t7 = t3a - t7a;
    t8a = r14(t8 + t12);
    t9a = r14(t9 + t13);
    t10a = r14(t10 + t14);
    t11a = r14(t11 + t15);
    t12a = r14(t8 - t12);
    t13a = r14(t9 - t13);
    t14a = r14(t10 - t14);
    t15a = r14(t11 - t15);

    t4a = t4 * 15137 + t5 * 6270;
    t5a = t4 * 6270 - t5 * 15137;
    t6a = t7 * 15137 - t6 * 6270;
    t7a = t7 * 6270 + t6 * 15137;
    t12 = t12a * 15137 + t13a * 6270;
    t13 = t12a * 6270 - t13a * 15137;
    t14 = t15a * 15137 - t14a * 6270;
    t15 = t15a * 6270 + t14a * 15137;

    out[0] = t0 + t2;
    out[15] = -(t1 + t3);
    let t2a = t0 - t2;
    let t3a = t1 - t3;
    out[3] = -r14(t4a + t6a);
    out[12] = r14(t5a + t7a);
    t6 = r14(t4a - t6a);
    t7 = r14(t5a - t7a);
    out[1] = -(t8a + t10a);
    out[14] = t9a + t11a;
    t10 = t8a - t10a;
    t11 = t9a - t11a;
    out[2] = r14(t12 + t14);
    out[13] = -r14(t13 + t15);
    let t14a = r14(t12 - t14);
    let t15a = r14(t13 - t15);

    out[7] = r14(-(t2a + t3a) * 11585);
    out[8] = r14((t2a - t3a) * 11585);
    out[4] = r14((t7 + t6) * 11585);
    out[11] = r14((t7 - t6) * 11585);
    out[6] = r14((t11 + t10) * 11585);
    out[9] = r14((t11 - t10) * 11585);
    out[5] = r14(-(t14a + t15a) * 11585);
    out[10] = r14((t14a - t15a) * 11585);
}

fn idct32(v: &[i64], out: &mut [i64]) {
    let mut t0a = r14((v[0] + v[16]) * 11585);
    let mut t1a = r14((v[0] - v[16]) * 11585);
    let mut t2a = r14(v[8] * 6270 - v[24] * 15137);
    let mut t3a = r14(v[8] * 15137 + v[24] * 6270);
    let mut t4a = r14(v[4] * 3196 - v[28] * 16069);
    let mut t7a = r14(v[4] * 16069 + v[28] * 3196);
    let mut t5a = r14(v[20] * 13623 - v[12] * 9102);
    let mut t6a = r14(v[20] * 9102 + v[12] * 13623);
    let mut t8a = r14(v[2] * 1606 - v[30] * 16305);
    let mut t15a = r14(v[2] * 16305 + v[30] * 1606);
    let mut t9a = r14(v[18] * 12665 - v[14] * 10394);
    let mut t14a = r14(v[18] * 10394 + v[14] * 12665);
    let mut t10a = r14(v[10] * 7723 - v[22] * 14449);
    let mut t13a = r14(v[10] * 14449 + v[22] * 7723);
    let mut t11a = r14(v[26] * 15679 - v[6] * 4756);
    let mut t12a = r14(v[26] * 4756 + v[6] * 15679);
    let mut t16a = r14(v[1] * 804 - v[31] * 16364);
    let mut t31a = r14(v[1] * 16364 + v[31] * 804);
    let mut t17a = r14(v[17] * 12140 - v[15] * 11003);
    let mut t30a = r14(v[17] * 11003 + v[15] * 12140);
    let mut t18a = r14(v[9] * 7005 - v[23] * 14811);
    let mut t29a = r14(v[9] * 14811 + v[23] * 7005);
    let mut t19a = r14(v[25] * 15426 - v[7] * 5520);
    let mut t28a = r14(v[25] * 5520 + v[7] * 15426);
    let mut t20a = r14(v[5] * 3981 - v[27] * 15893);
    let mut t27a = r14(v[5] * 15893 + v[27] * 3981);
    let mut t21a = r14(v[21] * 14053 - v[11] * 8423);
    let mut t26a = r14(v[21] * 8423 + v[11] * 14053);
    let mut t22a = r14(v[13] * 9760 - v[19] * 13160);
    let mut t25a = r14(v[13] * 13160 + v[19] * 9760);
    let mut t23a = r14(v[29] * 16207 - v[3] * 2404);
    let mut t24a = r14(v[29] * 2404 + v[3] * 16207);

    let mut t0 = t0a + t3a;
    let mut t1 = t1a + t2a;
    let mut t2 = t1a - t2a;
    let mut t3 = t0a - t3a;
    let mut t4 = t4a + t5a;
    let mut t5 = t4a - t5a;
    let mut t6 = t7a - t6a;
    let mut t7 = t7a + t6a;
    let mut t8 = t8a + t9a;
    let mut t9 = t8a - t9a;
    let mut t10 = t11a - t10a;
    let mut t11 = t11a + t10a;
    let mut t12 = t12a + t13a;
    let mut t13 = t12a - t13a;
    let mut t14 = t15a - t14a;
    let mut t15 = t15a + t14a;
    let mut t16 = t16a + t17a;
    let mut t17 = t16a - t17a;
    let mut t18 = t19a - t18a;
    let mut t19 = t19a + t18a;
    let mut t20 = t20a + t21a;
    let mut t21 = t20a - t21a;
    let mut t22 = t23a - t22a;
    let mut t23 = t23a + t22a;
    let mut t24 = t24a + t25a;
    let mut t25 = t24a - t25a;
    let mut t26 = t27a - t26a;
    let mut t27 = t27a + t26a;
    let mut t28 = t28a + t29a;
    let mut t29 = t28a - t29a;
    let mut t30 = t31a - t30a;
    let mut t31 = t31a + t30a;

    t5a = r14((t6 - t5) * 11585);
    t6a = r14((t6 + t5) * 11585);
    t9a = r14(t14 * 6270 - t9 * 15137);
    t14a = r14(t14 * 15137 + t9 * 6270);
    t10a = r14(-(t13 * 15137 + t10 * 6270));
    t13a = r14(t13 * 6270 - t10 * 15137);
    t17a = r14(t30 * 3196 - t17 * 16069);
    t30a = r14(t30 * 16069 + t17 * 3196);
    t18a = r14(-(t29 * 16069 + t18 * 3196));
    t29a = r14(t29 * 3196 - t18 * 16069);
    t21a = r14(t26 * 13623 - t21 * 9102);
    t26a = r14(t26 * 9102 + t21 * 13623);
    t22a = r14(-(t25 * 9102 + t22 * 13623));
    t25a = r14(t25 * 13623 - t22 * 9102);

    t0a = t0 + t7;
    t1a = t1 + t6a;
    t2a = t2 + t5a;
    t3a = t3 + t4;
    t4a = t3 - t4;
    t5 = t2 - t5a;
    t6 = t1 - t6a;
    t7a = t0 - t7;
    t8a = t8 + t11;
    t9 = t9a + t10a;
    t10 = t9a - t10a;
    t11a = t8 - t11;
    t12a = t15 - t12;
    t13 = t14a - t13a;
    t14 = t14a + t13a;
    t15a = t15 + t12;
    t16a = t16 + t19;
    t17 = t17a + t18a;
    t18 = t17a - t18a;
    t19a = t16 - t19;
    t20a = t23 - t20;
    t21 = t22a - t21a;
    t22 = t22a + t21a;
    t23a = t23 + t20;
    t24a = t24 + t27;
    t25 = t25a + t26a;
    t26 = t25a - t26a;
    t27a = t24 - t27;
    t28a = t31 - t28;
    t29 = t30a - t29a;
    t30 = t30a + t29a;
    t31a = t31 + t28;

    t10a = r14((t13 - t10) * 11585);
    t13a = r14((t13 + t10) * 11585);
    t11 = r14((t12a - t11a) * 11585);
    t12 = r14((t12a + t11a) * 11585);
    t18a = r14(t29 * 6270 - t18 * 15137);
    t29a = r14(t29 * 15137 + t18 * 6270);
    t19 = r14(t28a * 6270 - t19a * 15137);
    t28 = r14(t28a * 15137 + t19a * 6270);
    t20 = r14(-(t27a * 15137 + t20a * 6270));
    t27 = r14(t27a * 6270 - t20a * 15137);
    t21a = r14(-(t26 * 15137 + t21 * 6270));
    t26a = r14(t26 * 6270 - t21 * 15137);

    t0 = t0a + t15a;
    t1 = t1a + t14;
    t2 = t2a + t13a;
    t3 = t3a + t12;
    t4 = t4a + t11;
    t5a = t5 + t10a;
    t6a = t6 + t9;
    t7 = t7a + t8a;
    t8 = t7a - t8a;
    t9a = t6 - t9;
    t10 = t5 - t10a;
    t11a = t4a - t11;
    t12a = t3a - t12;
    t13 = t2a - t13a;
    t14a = t1a - t14;
    t15 = t0a - t15a;
    t16 = t16a + t23a;
    t17a = t17 + t22;
    t18 = t18a + t21a;
    t19a = t19 + t20;
    t20a = t19 - t20;
    t21 = t18a - t21a;
    t22a = t17 - t22;
    t23 = t16a - t23a;
    t24 = t31a - t24a;
    t25a = t30 - t25;
    t26 = t29a - t26a;
    t27a = t28 - t27;
    t28a = t28 + t27;
    t29 = t29a + t26a;
    t30a = t30 + t25;
    t31 = t31a + t24a;

    t20 = r14((t27a - t20a) * 11585);
    t27 = r14((t27a + t20a) * 11585);
    t21a = r14((t26 - t21) * 11585);
    t26a = r14((t26 + t21) * 11585);
    t22 = r14((t25a - t22a) * 11585);
    t25 = r14((t25a + t22a) * 11585);
    t23a = r14((t24 - t23) * 11585);
    t24a = r14((t24 + t23) * 11585);

    out[0] = t0 + t31;
    out[1] = t1 + t30a;
    out[2] = t2 + t29;
    out[3] = t3 + t28a;
    out[4] = t4 + t27;
    out[5] = t5a + t26a;
    out[6] = t6a + t25;
    out[7] = t7 + t24a;
    out[8] = t8 + t23a;
    out[9] = t9a + t22;
    out[10] = t10 + t21a;
    out[11] = t11a + t20;
    out[12] = t12a + t19a;
    out[13] = t13 + t18;
    out[14] = t14a + t17a;
    out[15] = t15 + t16;
    out[16] = t15 - t16;
    out[17] = t14a - t17a;
    out[18] = t13 - t18;
    out[19] = t12a - t19a;
    out[20] = t11a - t20;
    out[21] = t10 - t21a;
    out[22] = t9a - t22;
    out[23] = t8 - t23a;
    out[24] = t7 - t24a;
    out[25] = t6a - t25;
    out[26] = t5a - t26a;
    out[27] = t4 - t27;
    out[28] = t3 - t28a;
    out[29] = t2 - t29;
    out[30] = t1 - t30a;
    out[31] = t0 - t31;
}

fn tx_1d(adst: bool, n: usize, v: &[i64], out: &mut [i64]) {
    match (n, adst) {
        (4, false) => idct4(v, out),
        (4, true) => iadst4(v, out),
        (8, false) => idct8(v, out),
        (8, true) => iadst8(v, out),
        (16, false) => idct16(v, out),
        (16, true) => iadst16(v, out),
        _ => idct32(v, out),
    }
}

/// Adds the inverse transform of `coefs` (row-major n*n, n = 4<<tx_size) to `dst`,
/// clipping to bit depth `bd`, then zeroes `coefs`.
pub fn inverse_add(tx_size: usize, tx_type: usize, coefs: &mut [i32], dst: &mut [u16], stride: usize, eob: usize, bd: u32) {
    if eob == 0 {
        return;
    }
    let n = 4usize << tx_size;
    let tx_type = if tx_size == 3 { 0 } else { tx_type };
    // FFmpeg numbers the types DCT_DCT, DCT_ADST, ADST_DCT, ADST_ADST; the first (vertical) pass is ADST for 1 and 3
    let first_adst = tx_type == 1 || tx_type == 3;
    let second_adst = tx_type == 2 || tx_type == 3;
    let bits: usize = match tx_size {
        0 => 4,
        1 => 5,
        _ => 6,
    };
    let max = (1i64 << bd) - 1;

    let mut tmp = [0i64; 1024];
    let mut inp = [0i64; 32];
    let mut out = [0i64; 32];

    for i in 0..n {
        for x in 0..n {
            inp[x] = coefs[x * n + i] as i64;
        }
        tx_1d(first_adst, n, &inp[..n], &mut out[..n]);
        for k in 0..n {
            tmp[i * n + k] = out[k] as i32 as i64;
        }
    }
    coefs[..n * n].fill(0);

    for i in 0..n {
        for x in 0..n {
            inp[x] = tmp[x * n + i];
        }
        tx_1d(second_adst, n, &inp[..n], &mut out[..n]);
        for j in 0..n {
            let v = (out[j] + (1i64 << (bits - 1))) >> bits;
            let p = &mut dst[j * stride + i];
            *p = (*p as i64 + v).clamp(0, max) as u16;
        }
    }
}

fn iwht1d(v: [i64; 4], first: bool) -> [i64; 4] {
    let (mut t0, mut t1, mut t2, mut t3) = if first {
        (v[0] >> 2, v[3] >> 2, v[1] >> 2, v[2] >> 2)
    } else {
        (v[0], v[3], v[1], v[2])
    };

    t0 += t2;
    t3 -= t1;
    let t4 = (t0 - t3) >> 1;
    t1 = t4 - t1;
    t2 = t4 - t2;
    t0 -= t1;
    t3 += t2;

    [t0, t1, t2, t3]
}

/// 4x4 Walsh-Hadamard (lossless) inverse transform add.
pub fn iwht_add(coefs: &mut [i32], dst: &mut [u16], stride: usize, eob: usize, bd: u32) {
    if eob == 0 {
        return;
    }
    let max = (1i64 << bd) - 1;
    let mut tmp = [0i64; 16];

    for i in 0..4 {
        let v = [coefs[i] as i64, coefs[4 + i] as i64, coefs[8 + i] as i64, coefs[12 + i] as i64];
        let o = iwht1d(v, true);
        for k in 0..4 {
            tmp[i * 4 + k] = o[k] as i32 as i64;
        }
    }
    coefs[..16].fill(0);

    for i in 0..4 {
        let v = [tmp[i], tmp[4 + i], tmp[8 + i], tmp[12 + i]];
        let o = iwht1d(v, false);
        for j in 0..4 {
            let p = &mut dst[j * stride + i];
            *p = (*p as i64 + o[j]).clamp(0, max) as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_8x8_adds_constant() {
        let mut coefs = [0i32; 64];
        coefs[0] = 64;
        let mut dst = [128u16; 64];
        inverse_add(1, 0, &mut coefs, &mut dst, 8, 1, 8);
        assert!(dst.iter().all(|&p| p == 129));
        assert!(coefs.iter().all(|&c| c == 0));
    }

    #[test]
    fn dc_only_4x4_clips_to_bit_depth() {
        let mut coefs = [0i32; 16];
        coefs[0] = 4000;
        let mut dst = [250u16; 16];
        inverse_add(0, 0, &mut coefs, &mut dst, 4, 1, 8);
        assert!(dst.iter().all(|&p| p == 255));
    }

    #[test]
    fn zero_block_is_noop() {
        let mut coefs = [0i32; 256];
        let mut dst = [77u16; 256];
        inverse_add(2, 3, &mut coefs, &mut dst, 16, 16, 10);
        assert!(dst.iter().all(|&p| p == 77));
        assert!(coefs.iter().all(|&c| c == 0));
    }

    #[test]
    fn zero_wht_is_noop() {
        let mut coefs = [0i32; 16];
        let mut dst = [5u16; 16];
        iwht_add(&mut coefs, &mut dst, 4, 16, 8);
        assert!(dst.iter().all(|&p| p == 5));
        assert!(coefs.iter().all(|&c| c == 0));
    }
}
