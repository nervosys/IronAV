use super::*;

#[test]
fn bits_read_across_bytes_and_past_the_end() {
    let mut b = Bits::new(&[0b1010_0101, 0xFF]);
    assert_eq!(b.get(3), 0b101);
    assert_eq!(b.get(7), 0b001_0111);
    assert_eq!(b.left(), 6);
    assert_eq!(b.get(32), 0b11_1111 << 26);
    assert_eq!(b.left(), -26);
}

#[test]
fn run_length_and_motion_vector_codes_are_prefix_codes() {
    let sb: Vec<(u8, u8)> = SUPERBLOCK_RUN_LENS.iter().enumerate().map(|(i, &l)| (l, i as u8 + 1)).collect();
    let v = Vlc::new(&sb).unwrap();
    // run 1 is "0", run 2 is "100", run 3 is "101", "111111" starts the long run
    let mut b = Bits::new(&[0b0100_1011, 0b1111_1100]);
    assert_eq!(v.get(&mut b), Some(1));
    assert_eq!(v.get(&mut b), Some(2));
    assert_eq!(v.get(&mut b), Some(3));
    assert_eq!(v.get(&mut b), Some(34));
    // the vector table: 000 is 0, 001 is +1, 010 is -1, 0110 is +2
    let v = Vlc::new(&MV_CODES).unwrap();
    let mut b = Bits::new(&[0b0000_0101, 0b0011_0000]);
    let got: Vec<i32> = (0..4).map(|_| v.get(&mut b).unwrap() as i32 - 31).collect();
    assert_eq!(got, vec![0, 1, -1, 2]);
    // every one of the 63 vectors has a code of its own
    let mut seen = [false; 63];
    for &(_, s) in MV_CODES.iter() {
        seen[s as usize] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn a_flat_block_is_flat_and_dc_only_matches_the_full_transform() {
    let mut out = vec![0u8; 64];
    let mut block = [0i16; 64];
    block[0] = 800;
    idct(&mut block, &mut out, 0, 8, true);
    assert!(out.iter().all(|&p| p == out[0]), "{out:?}");
    assert_eq!(out[0], 128 + 25);
    // intra and added to mid-grey agree
    let mut add = vec![128u8; 64];
    let mut block = [0i16; 64];
    block[0] = 800;
    idct(&mut block, &mut add, 0, 8, false);
    assert_eq!(add, out);
    assert!(block.iter().all(|&v| v == 0));
}

#[test]
fn bounding_values_follow_the_limit() {
    let b = bounding_values(4);
    let at = |x: i32| b[(127 + x) as usize];
    assert_eq!([at(0), at(1), at(3), at(4), at(5), at(7), at(8)], [0, 1, 3, 4, 3, 1, 0]);
    assert_eq!(at(-3), -3);
    assert_eq!(at(-5), -3);
}

#[test]
fn laced_headers_round_trip() {
    let a = vec![1u8; 300];
    let b = vec![2u8; 5];
    let c = vec![3u8; 70];
    let joined = join_headers([&a, &b, &c]);
    let [x, y, z] = split_headers(&joined).unwrap();
    assert_eq!((x, y, z), (&a[..], &b[..], &c[..]));
    assert!(split_headers(&joined[..joined.len() - 71]).is_err());
    assert!(split_headers(&[]).is_err());
}

#[test]
fn damaged_headers_are_errors() {
    assert!(TheoraInfo::parse(&[0x80; 10]).is_err());
    let mut id = vec![0x80];
    id.extend(b"theora");
    id.extend([3, 2, 1]); // version 3.2.1
    id.extend([0, 11, 0, 9]); // 176x144 macroblocks
    id.extend([0, 0, 176, 0, 0, 144, 0, 0]);
    id.extend([0, 0, 0, 30, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0]);
    id.extend([0x30, 0x00]);
    assert_eq!(id.len(), 42);
    let info = TheoraInfo::parse(&id).unwrap();
    assert_eq!((info.width, info.height, info.pix_fmt), (176, 144, PixelFormat::Yuv420p));
    // an old alpha version, a huge frame and a reserved pixel format are refused
    let mut old = id.clone();
    old[8] = 1;
    assert!(TheoraInfo::parse(&old).is_err());
    let mut huge = id.clone();
    huge[10..12].copy_from_slice(&[0xFF, 0xFF]);
    huge[12..14].copy_from_slice(&[0xFF, 0xFF]);
    assert!(TheoraInfo::parse(&huge).is_err());
    let mut pf = id.clone();
    pf[41] |= 0x08;
    assert!(TheoraInfo::parse(&pf).is_err());
    // the setup header: wrong magic, truncated, random
    assert!(Setup::parse(b"\x82theora").is_err());
    let mut x = 12345u32;
    for len in [8usize, 100, 1000, 5000] {
        let mut s = vec![0x82];
        s.extend(b"theora");
        for _ in 0..len {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            s.push((x >> 24) as u8);
        }
        let _ = Setup::parse(&s);
    }
}
