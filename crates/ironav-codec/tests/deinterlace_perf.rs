//! Time per 1080i picture of the `deinterlace` filter. Run with
//! `cargo test --release -p ironav-codec --test deinterlace_perf -- --ignored --nocapture`.

use ironav_codec::{DeintMethod, DeintMode, DeintOptions, Deinterlacer};
use ironav_core::{FieldOrder, PixelFormat, VideoFrame};
use std::time::Instant;

fn picture(t: usize) -> VideoFrame {
    let (w, h) = (1920usize, 1080usize);
    let planes = PixelFormat::Yuv420p
        .planes(w, h)
        .iter()
        .map(|&(pw, ph, _)| (0..pw * ph).map(|i| ((i % pw) * 3 + (i / pw) * 5 + (i / pw % 2) * t * 9) as u8).collect())
        .collect();
    VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes }
}

#[test]
#[ignore = "a timing, not a check"]
fn time_per_1080i_picture() {
    let input: Vec<VideoFrame> = (0..8).map(picture).collect();
    for (name, opts) in [
        ("bwdif, frame mode", DeintOptions::default()),
        ("bwdif, field mode", DeintOptions { mode: DeintMode::Field, ..Default::default() }),
        ("linear, frame mode", DeintOptions { method: DeintMethod::Linear, ..Default::default() }),
    ] {
        let mut d = Deinterlacer::new(opts);
        let rounds = 5;
        let t0 = Instant::now();
        let mut n = 0;
        for _ in 0..rounds {
            for f in &input {
                n += d.push(f.clone(), FieldOrder::TopFirst).unwrap().len();
            }
        }
        n += d.flush().unwrap().len();
        let per = t0.elapsed().as_secs_f64() * 1000.0 / (rounds * input.len()) as f64;
        println!("{name}: {per:.2} ms per 1080i picture in ({n} pictures out), including the input clone");
    }
}
