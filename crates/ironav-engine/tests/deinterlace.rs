//! The `deinterlace` filter in jobs: the frame rate and time base in field mode, and the field order
//! the MPEG-2 decoder reports reaching the filter (`deint=interlaced`). The MPEG-2 tests need `ffmpeg`
//! on PATH and are skipped without it.

use ironav_engine::{plan, run, FilterCall, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-deint-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path, filter: Option<&str>) -> Job {
    let mut j = Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() };
    if let Some(f) = filter {
        j.filters.push(FilterCall::parse_shorthand(f).unwrap());
    }
    j
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

/// A Y4M file of `n` 64x48 pictures whose two fields differ: a bar that moves between them.
fn write_y4m(path: &Path, n: usize) {
    let (w, h) = (64usize, 48usize);
    let mut f = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420jpeg\n").into_bytes();
    for t in 0..n {
        f.extend_from_slice(b"FRAME\n");
        for y in 0..h {
            let time = 2 * t + y % 2;
            for x in 0..w {
                f.push(if (4 + 3 * time..14 + 3 * time).contains(&x) { 230 } else { 60 + y as u8 });
            }
        }
        f.extend(std::iter::repeat_n(128u8, w * h / 2));
    }
    std::fs::write(path, f).unwrap();
}

fn header_and_frames(path: &Path) -> (String, usize) {
    let b = std::fs::read(path).unwrap();
    let end = b.iter().position(|&c| c == b'\n').unwrap();
    let frames = b.windows(6).filter(|w| *w == b"FRAME\n").count();
    (String::from_utf8_lossy(&b[..end]).to_string(), frames)
}

#[test]
fn field_mode_doubles_the_frame_rate_and_halves_the_time_base() {
    let d = scratch("rate");
    let a = d.join("a.y4m");
    write_y4m(&a, 6);

    let j = job(&a, &d.join("frame.y4m"), Some("deinterlace"));
    let p = plan(&j).unwrap();
    assert_eq!(p.output.streams[0].time_base, plan(&job(&a, &d.join("x.y4m"), None)).unwrap().output.streams[0].time_base);
    run(&j, &mut |_| {}).unwrap();
    let (head, frames) = header_and_frames(&d.join("frame.y4m"));
    assert!(head.contains("F25:1"), "{head}");
    assert_eq!(frames, 6, "one picture per picture");

    let j = job(&a, &d.join("field.y4m"), Some("deinterlace:mode=field,parity=tff"));
    let (before, after) = (plan(&job(&a, &d.join("x.y4m"), None)).unwrap(), plan(&j).unwrap());
    let (tb0, tb1) = (before.output.streams[0].time_base, after.output.streams[0].time_base);
    assert_eq!((tb1.num, tb1.den), (tb0.num, tb0.den * 2), "timestamps are in half the units");
    assert_eq!(after.output.streams[0].duration, before.output.streams[0].duration.map(|d| d * 2));
    run(&j, &mut |_| {}).unwrap();
    let (head, frames) = header_and_frames(&d.join("field.y4m"));
    assert!(head.contains("F50:1"), "{head}");
    assert_eq!(frames, 12, "one picture per field");
}

#[test]
fn bad_options_and_formats_are_refused_when_planning() {
    let d = scratch("bad");
    let a = d.join("a.y4m");
    write_y4m(&a, 2);
    let e = plan(&job(&a, &d.join("o.y4m"), Some("deinterlace:mode=both"))).err().unwrap();
    assert_eq!(e.alternatives, vec!["frame", "field"]);
    assert!(plan(&job(&a, &d.join("o.y4m"), Some("deinterlace:speed=2"))).is_err());
    // a filter for video does not take audio
    let w = d.join("a.wav");
    std::fs::write(&w, b"RIFF").unwrap();
    assert!(plan(&job(&w, &d.join("o.wav"), Some("deinterlace"))).is_err());
}

fn picture_data(path: &Path) -> Vec<u8> {
    let b = std::fs::read(path).unwrap();
    let end = b.iter().position(|&c| c == b'\n').unwrap();
    b[end + 1..].to_vec()
}

#[test]
fn the_decoders_field_flags_reach_the_filter() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let d = scratch("flags");
    let (inter, prog) = (d.join("interlaced.m2v"), d.join("progressive.m2v"));
    ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=176x144:r=50,interlace=scan=tff", "-frames:v", "8", "-pix_fmt", "yuv420p", "-c:v", "mpeg2video", "-flags", "+ildct+ilme", "-b:v", "3M", "-f", "mpeg2video", inter.to_str().unwrap()]);
    ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=176x144:r=25", "-frames:v", "8", "-pix_fmt", "yuv420p", "-c:v", "mpeg2video", "-b:v", "3M", "-f", "mpeg2video", prog.to_str().unwrap()]);

    let out = |name: &str, input: &Path, filter: Option<&str>| {
        let o = d.join(name);
        run(&job(input, &o, filter), &mut |_| {}).unwrap();
        picture_data(&o)
    };
    // an interlaced stream: deint=interlaced does the same as deint=all, and both change the pictures
    let plain = out("plain.y4m", &inter, None);
    let all = out("all.y4m", &inter, Some("deinterlace"));
    let flagged = out("flagged.y4m", &inter, Some("deinterlace:deint=interlaced"));
    assert_eq!(all, flagged);
    assert_ne!(all, plain);
    assert_eq!(all.len(), plain.len());
    // a progressive stream is left alone by deint=interlaced
    let plain = out("pplain.y4m", &prog, None);
    let flagged = out("pflagged.y4m", &prog, Some("deinterlace:deint=interlaced"));
    assert_eq!(plain, flagged);
    // and deint=all on it changes it, so the flag is what made the difference
    assert_ne!(out("pall.y4m", &prog, Some("deinterlace")), plain);
    // the flag survives a format conversion placed before the filter
    let conv = d.join("conv.y4m");
    let mut j = job(&inter, &conv, Some("format:pix_fmt=yuv420p"));
    j.filters.push(FilterCall::parse_shorthand("deinterlace:deint=interlaced").unwrap());
    run(&j, &mut |_| {}).unwrap();
    assert_eq!(picture_data(&conv), all);
}
