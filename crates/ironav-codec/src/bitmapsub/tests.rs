use super::testing::*;
use super::*;

const TICK: f64 = 1.0 / 90_000.0;
// index, Y, Cr, Cb, alpha
const PALETTE: [[u8; 5]; 3] = [[0, 16, 128, 128, 0], [1, 235, 128, 128, 255], [2, 81, 240, 90, 128]];

fn bitmap(w: usize, h: usize) -> Vec<u8> {
    (0..w * h).map(|i| if (i / w + i % w) % 7 < 3 { 0 } else { 1 + ((i % w) / 5 % 2) as u8 }).collect()
}

fn object(id: u16, x: u16, y: u16, w: u16, h: u16) -> PgsObject {
    PgsObject { id, x, y, w, h, pixels: bitmap(w as usize, h as usize), crop: None, forced: false }
}

fn decode(sets: &[(i64, Vec<Vec<u8>>)]) -> Vec<SubtitleBitmap> {
    let mut d = PgsDecoder::new(TICK);
    let mut out = Vec::new();
    for (pts, segments) in sets {
        out.extend(d.push(*pts, &segments.concat()));
    }
    out.extend(d.finish());
    out
}

#[test]
fn pgs_pixels_and_timing_round_trip() {
    let o = object(1, 100, 900, 40, 6);
    let expect = o.pixels.clone();
    let set = pgs_display_set((1920, 1080), 2, &PALETTE, &[o], 0);
    let events = decode(&[(90_000, set), (270_000, pgs_clear_set((1920, 1080)))]);
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!((e.start, e.end, e.canvas_w, e.canvas_h), (1.0, 3.0, 1920, 1080));
    let r = &e.rects[0];
    assert_eq!((r.x, r.y, r.w, r.h), (100, 900, 40, 6));
    assert_eq!(r.pixels, expect);
    // HD sizes use BT.709: white stays white, and the transparent entry keeps its alpha
    let rgba = r.rgba();
    let at = |i: usize| &rgba[i * 4..i * 4 + 4];
    for (i, &p) in expect.iter().enumerate() {
        match p {
            0 => assert_eq!(at(i)[3], 0),
            1 => assert_eq!(at(i), [255, 255, 255, 255]),
            _ => assert_eq!(at(i)[3], 128),
        }
    }
    // Y 81, Cr 240, Cb 90 is a saturated red in either matrix
    assert!(r.palette[2][0] > 200 && r.palette[2][1] < 60 && r.palette[2][2] < 60, "{:?}", r.palette[2]);
}

#[test]
fn pgs_colours_follow_the_picture_height() {
    let pal = [[1u8, 100, 160, 100, 255]];
    let color = |h: u16| {
        let set = pgs_display_set((1280, h), 2, &pal, &[object(1, 0, 0, 4, 2)], 0);
        decode(&[(0, set)])[0].rects[0].palette[1]
    };
    assert_eq!(color(576), [super::yuv_to_rgb(100, 100, 160, false)[0], super::yuv_to_rgb(100, 100, 160, false)[1], super::yuv_to_rgb(100, 100, 160, false)[2], 255]);
    assert_ne!(color(576), color(1080));
    let [r, g, b] = super::yuv_to_rgb(100, 100, 160, true);
    assert_eq!(color(1080), [r, g, b, 255]);
}

#[test]
fn pgs_objects_split_over_segments_and_back_to_back_sets() {
    let a = object(1, 10, 20, 30, 12);
    let b = object(2, 50, 60, 20, 8);
    let (pa, pb) = (a.pixels.clone(), b.pixels.clone());
    let set1 = pgs_display_set((1920, 1080), 2, &PALETTE, &[a], 5);
    let set2 = pgs_display_set((1920, 1080), 0, &PALETTE, &[b], 3);
    let events = decode(&[(0, set1), (90_000, set2)]);
    assert_eq!(events.len(), 2);
    assert_eq!((events[0].start, events[0].end), (0.0, 1.0));
    assert_eq!(events[0].rects[0].pixels, pa);
    assert_eq!(events[1].rects[0].pixels, pb);
    assert_eq!(events[1].end, 1.0 + 10.0, "the last event is held for a while when nothing clears it");
}

#[test]
fn pgs_two_objects_in_one_composition_and_forced_flags() {
    let mut a = object(1, 10, 20, 8, 4);
    a.forced = true;
    let b = object(2, 100, 200, 6, 6);
    let set = pgs_display_set((720, 480), 2, &PALETTE, &[a, b], 0);
    let events = decode(&[(0, set), (45_000, pgs_clear_set((720, 480)))]);
    assert_eq!(events[0].rects.len(), 2);
    assert_eq!(events[0].rects.iter().map(|r| r.forced).collect::<Vec<_>>(), vec![true, false]);
    assert_eq!(events[0].end, 0.5);
}

#[test]
fn pgs_cropping_cuts_the_object() {
    let mut o = object(1, 100, 100, 20, 10);
    let full = o.pixels.clone();
    o.crop = Some([4, 2, 8, 5]);
    let set = pgs_display_set((1920, 1080), 2, &PALETTE, &[o], 0);
    let r = decode(&[(0, set)]).remove(0).rects.remove(0);
    assert_eq!((r.x, r.y, r.w, r.h), (104, 102, 8, 5));
    for y in 0..5 {
        assert_eq!(&r.pixels[y * 8..y * 8 + 8], &full[(y + 2) * 20 + 4..(y + 2) * 20 + 12]);
    }
}

#[test]
fn pgs_a_missing_palette_or_object_shows_nothing() {
    // objects with no palette segment: the composition is dropped
    let mut set = pgs_display_set((720, 480), 2, &PALETTE, &[object(1, 0, 0, 4, 4)], 0);
    set.remove(2); // PDS
    assert!(decode(&[(0, set)]).is_empty());
    // a composition naming an object that was never sent
    let mut set = pgs_display_set((720, 480), 2, &PALETTE, &[object(1, 0, 0, 4, 4)], 0);
    set.remove(3); // ODS
    assert!(decode(&[(0, set)]).is_empty());
}

#[test]
fn pgs_rle_cannot_write_past_the_bitmap() {
    // a run longer than the bitmap is ignored and the missing pixels make the object unusable
    let mut set = pgs_display_set((720, 480), 2, &PALETTE, &[object(1, 0, 0, 4, 2)], 0);
    let ods = set.iter().position(|s| s[0] == 0x15).unwrap();
    let rle = [0u8, 0x7F, 1, 0, 0];
    let mut body = vec![0, 1, 0, 0xC0, 0, 0, (rle.len() + 4) as u8, 0, 4, 0, 2];
    body.extend_from_slice(&rle);
    set[ods] = pgs_segment(0x15, &body);
    assert!(decode(&[(0, set)]).is_empty());
}

#[test]
fn pgs_garbage_and_truncation_never_panic() {
    let a = object(1, 10, 20, 30, 12);
    let good: Vec<u8> = pgs_display_set((1920, 1080), 2, &PALETTE, &[a], 11).concat();
    let mut seed = 0x1234_5678u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed >> 8
    };
    for cut in 0..good.len() {
        let mut d = PgsDecoder::new(TICK);
        d.push(0, &good[..cut]);
        d.push(1, &good);
        d.finish();
    }
    for _ in 0..2000 {
        let mut bad = good.clone();
        for _ in 0..1 + next() % 6 {
            let i = next() as usize % bad.len();
            bad[i] = next() as u8;
        }
        let mut d = PgsDecoder::new(TICK);
        d.push(0, &bad);
        d.push(1, &good);
        d.push(2, &pgs_clear_set((1920, 1080)).concat());
        d.finish();
    }
    // huge declared sizes
    let mut d = PgsDecoder::new(TICK);
    let mut set = pgs_display_set((65535, 65535), 2, &PALETTE, &[], 0).concat();
    set.extend_from_slice(&[0x15, 0, 11, 0, 1, 0, 0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 1]);
    assert!(d.push(0, &set).is_empty());
}

fn spu_pixels(w: usize, h: usize) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if (4..w - 4).contains(&x) && (3..h - 3).contains(&y) {
                1 + ((x / 3 + y) % 3) as u8
            } else {
                0
            }
        })
        .collect()
}

#[test]
fn dvd_pixels_colours_and_timing_round_trip() {
    let (w, h) = (40usize, 14usize);
    let pixels = spu_pixels(w, h);
    let spu = dvd_spu(100, 380, w as u16, h as u16, &pixels, [0, 1, 2, 3], [0, 15, 15, 8], 0, Some(88));
    let idx = b"size: 720x576\npalette: 000000, ff0000, 00ff00, 0000ff, 111111, 222222, 333333, 444444, 555555, 666666, 777777, 888888, 999999, aaaaaa, bbbbbb, cccccc\n";
    let mut d = DvdSubDecoder::new(idx, 1.0 / 1000.0);
    let events = d.push(2000, 0, &spu);
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!((e.canvas_w, e.canvas_h), (720, 576));
    assert!((e.start - 2.0).abs() < 1e-9 && (e.end - 3.001).abs() < 1e-9, "{} {}", e.start, e.end);
    let r = &e.rects[0];
    // the outer transparent border is cropped away
    assert_eq!((r.x, r.y, r.w, r.h), (104, 383, 32, 8));
    assert_eq!(r.palette.as_ref(), &[[0, 0, 0, 0], [255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 136]]);
    for y in 0..8 {
        for x in 0..32 {
            assert_eq!(r.pixels[y * 32 + x], pixels[(y + 3) * w + x + 4], "({x}, {y})");
        }
    }
}

#[test]
fn dvd_without_a_palette_guesses_white_levels_and_without_a_stop_uses_the_duration() {
    let (w, h) = (16usize, 6usize);
    let pixels = spu_pixels(w, h + 2);
    let spu = dvd_spu(0, 0, w as u16, (h + 2) as u16, &pixels, [0, 1, 2, 3], [0, 15, 15, 15], 0, None);
    let mut d = DvdSubDecoder::new(b"", 1.0 / 1000.0);
    let e = d.push(1000, 1500, &spu).remove(0);
    assert_eq!((e.canvas_w, e.canvas_h), (720, 480));
    assert_eq!((e.start, e.end), (1.0, 2.5));
    // three opaque colours: 0x00, 0x80, 0xff in turn
    assert_eq!(e.rects[0].palette.as_ref(), &[[0, 0, 0, 0], [0, 0, 0, 255], [128, 128, 128, 255], [255, 255, 255, 255]]);
}

#[test]
fn dvd_an_all_transparent_picture_shows_nothing() {
    let pixels = vec![0u8; 16 * 4];
    let spu = dvd_spu(0, 0, 16, 4, &pixels, [0, 1, 2, 3], [0, 15, 15, 15], 0, None);
    assert!(DvdSubDecoder::new(b"", 0.001).push(0, 0, &spu).is_empty());
}

#[test]
fn dvd_garbage_and_truncation_never_panic() {
    let pixels = spu_pixels(40, 14);
    let good = dvd_spu(100, 380, 40, 14, &pixels, [0, 1, 2, 3], [0, 15, 15, 8], 0, Some(88));
    let mut seed = 0x9E37_79B9u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed >> 8
    };
    let mut d = DvdSubDecoder::new(b"palette: zz, 1", 0.001);
    for cut in 0..good.len() {
        d.push(0, 0, &good[..cut]);
    }
    for _ in 0..4000 {
        let mut bad = good.clone();
        for _ in 0..1 + next() % 6 {
            let i = next() as usize % bad.len();
            bad[i] = next() as u8;
        }
        d.push(0, 0, &bad);
    }
    // an HD packet with a huge area and no data behind it
    let mut hd = vec![0, 0, 0, 40, 0, 0, 0, 12, 0, 0];
    hd.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 20, 0x05, 0x00, 0x0F, 0xFF, 0x00, 0x0F, 0xFF, 0x86, 0, 0, 0, 30, 0, 0, 0, 31, 0xFF]);
    assert!(d.push(0, 0, &hd).is_empty());
}

#[test]
fn the_decoder_for_a_codec() {
    assert!(BitmapSubDecoder::new(CodecId::HdmvPgs, b"", TICK).is_some());
    assert!(BitmapSubDecoder::new(CodecId::DvdSub, b"", TICK).is_some());
    assert!(BitmapSubDecoder::new(CodecId::SubRip, b"", TICK).is_none());
}
