//! Bitmap subtitles (Blu-ray PGS, DVD VobSub) read from raw `.sup` files, Matroska and MPEG program
//! streams, and decoded. FFmpeg cannot encode either kind, so the subtitle data is written here (by
//! `ironav_codec::bitmapsub::testing`) and FFmpeg only muxes it (`-c:s copy`) and renders it, for the
//! tests that need it. Skipped without `ffmpeg` on PATH.

use ironav_codec::bitmapsub::testing::*;
use ironav_codec::bitmapsub::{BitmapSubDecoder, SubtitleBitmap};
use ironav_core::{CodecId, Demuxer, StreamParams};
use ironav_formats::{open_demuxer, open_demuxer_lazy};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bitmapsubs-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> bool {
    let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output();
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!("ffmpeg {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            false
        }
        Err(_) => false,
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Every event of subtitle stream `stream`.
fn events(d: &mut dyn Demuxer, stream: usize) -> Vec<SubtitleBitmap> {
    let s = d.streams()[stream].clone();
    let mut dec = BitmapSubDecoder::new(s.codec, &s.extradata, s.time_base.as_f64()).expect("a bitmap subtitle codec");
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        if p.stream == stream {
            out.extend(dec.push(p.pts, p.duration, &p.data));
        }
    }
    out.extend(dec.finish());
    out
}

fn subtitle_stream(d: &dyn Demuxer, codec: CodecId) -> usize {
    d.streams().iter().position(|s| s.params == StreamParams::Subtitle && s.codec == codec).unwrap_or_else(|| panic!("a {codec:?} stream in {:?} / {:?}", d.streams(), d.unsupported_streams()))
}

// index, Y, Cr, Cb, alpha
const PALETTE: [[u8; 5]; 4] = [[0, 16, 128, 128, 0], [1, 235, 128, 128, 255], [2, 81, 240, 90, 255], [3, 145, 54, 34, 160]];

fn pgs_pixels(w: usize, h: usize, shift: usize) -> Vec<u8> {
    (0..w * h).map(|i| if (i / w + i % w + shift) % 9 < 3 { 0 } else { 1 + ((i % w + shift) / 6 % 3) as u8 }).collect()
}

fn pgs_object(x: u16, y: u16, w: u16, h: u16, shift: usize) -> PgsObject {
    PgsObject { id: 1, x, y, w, h, pixels: pgs_pixels(w as usize, h as usize, shift), crop: None, forced: false }
}

/// Two subtitles: at 1.0 s to 2.5 s, and at 3.0 s to 4.0 s.
fn pgs_sets() -> Vec<(u32, Vec<Vec<u8>>)> {
    let canvas = (1920, 1080);
    vec![
        // FFmpeg takes a file's first time as its start, so the file starts at 0
        (0, pgs_clear_set(canvas)),
        (90_000, pgs_display_set(canvas, 2, &PALETTE, &[pgs_object(300, 900, 200, 40, 0)], 0)),
        (225_000, pgs_clear_set(canvas)),
        (270_000, pgs_display_set(canvas, 2, &PALETTE, &[pgs_object(500, 120, 90, 31, 4)], 700)),
        (360_000, pgs_clear_set(canvas)),
    ]
}

fn check_pgs(events: &[SubtitleBitmap], what: &str) {
    assert_eq!(events.len(), 2, "{what}");
    for (e, (start, end, x, y, w, h, shift)) in events.iter().zip([(1.0, 2.5, 300, 900, 200, 40, 0), (3.0, 4.0, 500, 120, 90, 31, 4)]) {
        assert!((e.start - start).abs() < 1e-3 && (e.end - end).abs() < 1e-3, "{what}: {} to {}", e.start, e.end);
        assert_eq!((e.canvas_w, e.canvas_h), (1920, 1080), "{what}");
        let r = &e.rects[0];
        assert_eq!((r.x, r.y, r.w, r.h), (x, y, w, h), "{what}");
        assert_eq!(r.pixels, pgs_pixels(w as usize, h as usize, shift), "{what}");
    }
}

#[test]
fn a_sup_file_is_a_pgs_stream() {
    let dir = scratch();
    let sup = dir.join("in.sup");
    std::fs::write(&sup, sup_file(&pgs_sets())).unwrap();
    let mut d = open_demuxer(&sup).unwrap();
    assert_eq!(d.format_name(), "sup");
    let s = subtitle_stream(d.as_ref(), CodecId::HdmvPgs);
    assert_eq!(d.streams().len(), 1);
    check_pgs(&events(d.as_mut(), s), "sup");
    // the lazy opener, as the player uses, reads it the same way
    let mut d = open_demuxer_lazy(&sup).unwrap();
    check_pgs(&events(d.as_mut(), 0), "sup, lazy");
}

#[test]
fn pgs_in_matroska_made_by_ffmpeg() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let sup = dir.join("in.sup");
    std::fs::write(&sup, sup_file(&pgs_sets())).unwrap();
    let mkv = dir.join("pgs.mkv");
    if !run(&["-f", "lavfi", "-i", "color=black:s=320x180:r=10:d=5", "-i", path(&sup), "-map", "0:v", "-map", "1:s", "-c:v", "mpeg4", "-c:s", "copy", path(&mkv)]) {
        eprintln!("ffmpeg cannot mux the PGS stream; skipped");
        return;
    }
    for lazy in [false, true] {
        let mut d = if lazy { open_demuxer_lazy(&mkv) } else { open_demuxer(&mkv) }.unwrap();
        let s = subtitle_stream(d.as_ref(), CodecId::HdmvPgs);
        check_pgs(&events(d.as_mut(), s), if lazy { "mkv, lazy" } else { "mkv" });
    }
}

/// A frame of black with the subtitle overlaid by FFmpeg, as raw RGB at `at` seconds.
fn ffmpeg_render(input_args: &[&str], at: f64, w: usize, h: usize, dir: &Path) -> Option<Vec<u8>> {
    let out = dir.join(format!("render-{at}.rgb"));
    let size = format!("{w}x{h}");
    let color = format!("color=black:s={size}:r=10:d=5");
    let t = at.to_string();
    let mut args = vec!["-f", "lavfi", "-i", &color];
    args.extend_from_slice(input_args);
    args.extend_from_slice(&["-filter_complex", "[0:v][1:s]overlay=format=rgb", "-ss", &t, "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", path(&out)]);
    if !run(&args) {
        return None;
    }
    std::fs::read(&out).ok().filter(|b| b.len() == w * h * 3)
}

#[test]
fn pgs_pixels_match_ffmpegs_rendering() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let sup = dir.join("in.sup");
    std::fs::write(&sup, sup_file(&pgs_sets())).unwrap();
    let Some(frame) = ffmpeg_render(&["-canvas_size", "1920x1080", "-i", path(&sup)], 1.5, 1920, 1080, &dir) else {
        eprintln!("ffmpeg cannot render the PGS stream; skipped");
        return;
    };
    let mut d = open_demuxer(&sup).unwrap();
    let ev = events(d.as_mut(), 0);
    let r = &ev[0].rects[0];
    let rgba = r.rgba();
    let (mut compared, mut differing) = (0, 0);
    for y in 0..r.h as usize {
        for x in 0..r.w as usize {
            let p = &rgba[(y * r.w as usize + x) * 4..][..4];
            let at = ((r.y as usize + y) * 1920 + r.x as usize + x) * 3;
            // FFmpeg blends over black: colour * alpha / 255
            let expect: Vec<i32> = p[..3].iter().map(|&c| (c as i32 * p[3] as i32 + 127) / 255).collect();
            let got: Vec<i32> = frame[at..at + 3].iter().map(|&c| c as i32).collect();
            compared += 1;
            if expect.iter().zip(&got).any(|(a, b)| (a - b).abs() > 2) {
                differing += 1;
                if differing < 6 {
                    eprintln!("({x}, {y}): index {} ours {expect:?} ffmpeg {got:?}", r.pixels[y * r.w as usize + x]);
                }
            }
        }
    }
    assert!(compared > 0 && differing == 0, "{differing} of {compared} pixels differ from FFmpeg's rendering");
    // and nothing is drawn where there is no subtitle
    assert!(frame[..1920 * 3 * 100].iter().all(|&b| b == 0));
}

/// A `.sub` file (an MPEG program stream of subpicture packets, as DVD rips keep them) and its `.idx`.
fn vobsub_files(spus: &[(f64, Vec<u8>)], dir: &Path, name: &str) -> PathBuf {
    let mut sub = Vec::new();
    let mut idx = String::from("# VobSub index file, v7 (do not modify this line!)\nsize: 720x480\npalette: 000000, ffffff, 808080, 404040, ff0000, 00ff00, 0000ff, ffff00, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000\nlangidx: 0\nid: en, index: 0\n");
    for (t, spu) in spus {
        let ms = (t * 1000.0) as u64;
        idx += &format!("timestamp: {:02}:{:02}:{:02}:{:03}, filepos: {:09x}\n", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000, sub.len());
        let pts = (t * 90_000.0) as u64;
        let mut first = true;
        for chunk in spu.chunks(2000) {
            sub.extend_from_slice(&[0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8]);
            let mut pes = vec![0x81, if first { 0x80 } else { 0 }, if first { 5 } else { 0 }];
            if first {
                pes.extend_from_slice(&[0x21 | ((pts >> 29) & 0x0E) as u8, (pts >> 22) as u8, 1 | ((pts >> 14) & 0xFE) as u8, (pts >> 7) as u8, 1 | ((pts << 1) & 0xFE) as u8]);
            }
            pes.push(0x20);
            pes.extend_from_slice(chunk);
            sub.extend_from_slice(&[0, 0, 1, 0xBD]);
            sub.extend_from_slice(&(pes.len() as u16).to_be_bytes());
            sub.extend_from_slice(&pes);
            first = false;
        }
    }
    std::fs::write(dir.join(format!("{name}.sub")), sub).unwrap();
    let idx_path = dir.join(format!("{name}.idx"));
    std::fs::write(&idx_path, idx).unwrap();
    idx_path
}

fn dvd_pixels(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..w * h)
        .map(|i| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let (x, y) = (i % w, i / w);
            // a framed block of noise (a large SPU, so FFmpeg splits it over PES packets)
            if x < 3 || y < 2 || x >= w - 3 || y >= h - 2 {
                0
            } else {
                ((s >> 24) % 4) as u8
            }
        })
        .collect()
}

fn dvd_spus() -> Vec<(f64, Vec<u8>)> {
    let (w, h) = (300usize, 80usize);
    vec![
        (1.0, dvd_spu(200, 380, w as u16, h as u16, &dvd_pixels(w, h, 1), [0, 1, 2, 3], [0, 15, 15, 15], 0, Some(88))),
        (3.0, dvd_spu(100, 400, 120, 20, &dvd_pixels(120, 20, 2), [0, 1, 2, 3], [0, 15, 8, 15], 0, Some(176))),
    ]
}

fn check_dvd(events: &[SubtitleBitmap], what: &str) {
    assert_eq!(events.len(), 2, "{what}");
    for (e, (start, len, x, y, w, h, seed)) in events.iter().zip([(1.0, 1.001, 200, 380, 300usize, 80usize, 1), (3.0, 2.003, 100, 400, 120, 20, 2)]) {
        assert!((e.start - start).abs() < 2e-3 && (e.end - start - len).abs() < 2e-3, "{what}: {} to {}", e.start, e.end);
        assert_eq!((e.canvas_w, e.canvas_h), (720, 480), "{what}");
        let r = &e.rects[0];
        // the transparent frame is cropped away
        assert_eq!((r.x, r.y, r.w as usize, r.h as usize), (x + 3, y + 2, w - 6, h - 4), "{what}");
        let src = dvd_pixels(w, h, seed);
        for row in 0..h - 4 {
            assert_eq!(&r.pixels[row * (w - 6)..(row + 1) * (w - 6)], &src[(row + 2) * w + 3..(row + 2) * w + w - 3], "{what} row {row}");
        }
    }
}

#[test]
fn dvd_subpictures_in_matroska_and_mpeg_ps_made_by_ffmpeg() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let idx = vobsub_files(&dvd_spus(), &dir, "in");
    let (mkv, vob) = (dir.join("dvd.mkv"), dir.join("dvd.vob"));
    let common = ["-f", "lavfi", "-i", "color=black:s=720x480:r=25:d=5", "-i", path(&idx), "-map", "0:v", "-map", "1:s", "-c:s", "copy"];
    let mut mux_mkv: Vec<&str> = common.to_vec();
    mux_mkv.extend_from_slice(&["-c:v", "mpeg4", path(&mkv)]);
    if !run(&mux_mkv) {
        eprintln!("ffmpeg cannot read the VobSub files; skipped");
        return;
    }
    for lazy in [false, true] {
        let mut d = if lazy { open_demuxer_lazy(&mkv) } else { open_demuxer(&mkv) }.unwrap();
        let s = subtitle_stream(d.as_ref(), CodecId::DvdSub);
        let text = String::from_utf8_lossy(&d.streams()[s].extradata).into_owned();
        assert!(text.contains("palette:") && text.contains("size: 720x480"), "{text}");
        check_dvd(&events(d.as_mut(), s), if lazy { "mkv, lazy" } else { "mkv" });
    }
    let mut mux_vob: Vec<&str> = common.to_vec();
    mux_vob.extend_from_slice(&["-c:v", "mpeg2video", "-f", "vob", path(&vob)]);
    if !run(&mux_vob) {
        eprintln!("ffmpeg cannot write the subpictures into a program stream; skipped");
        return;
    }
    let mut d = open_demuxer(&vob).unwrap();
    assert_eq!(d.format_name(), "mpegps");
    let s = subtitle_stream(d.as_ref(), CodecId::DvdSub);
    assert_eq!(d.streams()[s].extradata, b"size: 720x480\n");
    // no palette in the stream: the decoder guesses white levels, and the pixels and timing are the same
    let ev = events(d.as_mut(), s);
    check_dvd(&ev, "mpeg-ps");
    let palette = &ev[0].rects[0].palette;
    assert_eq!(palette.as_ref(), &[[0, 0, 0, 0], [0, 0, 0, 255], [128, 128, 128, 255], [255, 255, 255, 255]]);
}
