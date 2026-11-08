//! Live-encode timing of the H.264 encoder in the constrained-baseline low-latency setting the
//! remote-desktop case uses, on two synthetic contents (screen-like and gradient plus noise) at
//! 912x704 and 1920x1080. Prints the median and 90th percentile milliseconds per picture, the bytes
//! per picture and the PSNR against the source after IronAV's own decoder.
//!
//! `cargo run --release -p ironav-codec --example h264_realtime_bench [-- default|realtime|both [frames [qp]]]`
//! (4 Mbit/s rate control, or a fixed quantiser when `qp` is given, which compares the bytes fairly).

use ironav_codec::h264::{H264Decoder, H264Encoder};
use ironav_core::{Decoder, Encoder, Frame, PixelFormat, Rational, StreamParams, VideoFrame};
use std::time::Instant;

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
        self.0 >> 16
    }
}

/// A 5x7 pixel text-like glyph pattern, one bit per pixel.
fn glyph(c: u32, x: usize, y: usize) -> bool {
    let h = c.wrapping_mul(2_654_435_761) ^ (x as u32 * 40503) ^ (y as u32 * 9973);
    (h >> 7) & 3 != 0 && x < 6 && y < 9
}

/// Moving gradient plus noise: harsh, every sample changes every picture.
fn noisy(w: usize, h: usize, t: usize) -> VideoFrame {
    let mut rng = Rng(7 + t as u32 * 1000);
    let mut y = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            let v = ((c + 3 * t) * 160 / w + r * 60 / h) as i32 + 20 + (rng.next() % 9) as i32 - 4;
            y[r * w + c] = v.clamp(0, 255) as u8;
        }
    }
    let (cw, ch) = (w / 2, h / 2);
    let u = (0..cw * ch).map(|i| (100 + (i % cw) * 50 / cw + t) as u8).collect();
    let v = (0..cw * ch).map(|i| (150 - (i / cw) * 40 / ch) as u8).collect();
    VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y, u, v] }
}

/// Flat UI panels, text-like rows, a typed line growing, a moving cursor and a scrolling band.
fn screen(w: usize, h: usize, t: usize) -> VideoFrame {
    let mut y = vec![235u8; w * h];
    let mut u = vec![128u8; w * h / 4];
    let mut v = vec![128u8; w * h / 4];
    let cw = w / 2;
    // title bar and side panel
    for r in 0..h {
        for c in 0..w {
            if r < 28 {
                y[r * w + c] = 60;
            } else if c < w / 6 {
                y[r * w + c] = 200;
            }
        }
    }
    for r in 0..14 {
        for c in 0..w / 2 {
            u[r * cw + c] = 90;
            v[r * cw + c] = 170;
        }
    }
    // static text lines in the main area, a line of glyphs every 14 rows
    let text_x0 = w / 6 + 16;
    for line in 0..(h - 60) / 14 {
        let r0 = 40 + line * 14;
        let n = ((line * 37) % 50 + 20).min((w - text_x0 - 16) / 8);
        for ch in 0..n {
            for gy in 0..9 {
                for gx in 0..6 {
                    if glyph((line * 131 + ch) as u32, gx, gy) {
                        y[(r0 + gy) * w + text_x0 + ch * 8 + gx] = 20;
                    }
                }
            }
        }
    }
    // a typed line: one more glyph each picture, in an otherwise empty strip at the bottom
    let r0 = h - 20;
    for ch in 0..(t % ((w - text_x0 - 16) / 8)) {
        for gy in 0..9 {
            for gx in 0..6 {
                if glyph((7000 + ch) as u32, gx, gy) {
                    y[(r0 + gy) * w + text_x0 + ch * 8 + gx] = 20;
                }
            }
        }
    }
    // a scrolling band 64 rows high at the right: 4 rows up each picture
    let (bx, bw, by) = (w - w / 6, w / 6 - 8, 60);
    for r in 0..64 {
        for c in 0..bw {
            let row = r + 4 * t;
            y[(by + r) * w + bx + c] = if glyph((row / 14) as u32 * 31 + (c / 8) as u32, c % 8, row % 14) { 30 } else { 250 };
        }
    }
    // the cursor: a 2x14 bar jumping across the text area
    let cx = text_x0 + (t * 11) % (w - text_x0 - 40);
    let cy = 40 + ((t / 7) * 14) % (h - 80);
    for r in 0..14 {
        for c in 0..2 {
            y[(cy + r) * w + cx + c] = 0;
        }
    }
    VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y, u, v] }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / se.max(1e-9)).log10()
}

fn run(name: &str, mode: &str, w: usize, h: usize, frames: usize, qp: Option<u8>, make: fn(usize, usize, usize) -> VideoFrame) {
    let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) };
    let mut enc = H264Encoder::new(&params, qp, if qp.is_some() { None } else { Some(4_000_000) }).unwrap().low_latency().constrained_baseline();
    if mode == "realtime" {
        enc = enc.realtime();
    }
    let mut dec = H264Decoder::new();
    let (mut times, mut sizes, mut psnrs) = (Vec::new(), Vec::new(), Vec::new());
    let mut src = Vec::new();
    let mut out: Vec<VideoFrame> = Vec::new();
    for t in 0..frames {
        let f = make(w, h, t);
        let fr = Frame::Video(f.clone());
        let now = Instant::now();
        let pkts = enc.encode(&fr).unwrap();
        times.push(now.elapsed().as_secs_f64() * 1e3);
        for p in &pkts {
            sizes.push(p.data.len());
            if std::env::var("SIZES").is_ok() { eprint!("{} ", p.data.len()); }
            for d in dec.decode(p).unwrap() {
                if let Frame::Video(v) = d {
                    out.push(v);
                }
            }
        }
        src.push(f);
    }
    for d in dec.flush().unwrap() {
        if let Frame::Video(v) = d {
            out.push(v);
        }
    }
    assert_eq!(out.len(), frames);
    if std::env::var("PSNRS").is_ok() {
        for (s, d) in src.iter().zip(&out) {
            eprint!("{:.1} ", psnr(&s.planes[0], &d.planes[0]));
        }
        eprintln!();
    }
    for (s, d) in src.iter().zip(&out) {
        psnrs.push(psnr(&s.planes[0], &d.planes[0]));
    }
    // the first picture is an IDR and the rate control settling: leave out the first 3
    let mut ts = times[3..].to_vec();
    ts.sort_by(f64::total_cmp);
    let (med, p90) = (ts[ts.len() / 2], ts[ts.len() * 9 / 10]);
    let bytes = sizes[3..].iter().sum::<usize>() / (sizes.len() - 3);
    let ps = psnrs[3..].iter().sum::<f64>() / (psnrs.len() - 3) as f64;
    println!("{mode:9} {name:7} {w:4}x{h:<4} median {med:7.2} ms  p90 {p90:7.2} ms  {bytes:7} B/pic  PSNR {ps:5.2} dB");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("both", |s| s.as_str());
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let qp: Option<u8> = args.get(3).and_then(|s| s.parse().ok());
    for m in ["default", "realtime"] {
        if mode != "both" && mode != m {
            continue;
        }
        for (w, h) in [(912, 704), (1920, 1080)] {
            run("screen", m, w, h, frames, qp, screen);
            run("noisy", m, w, h, frames, qp, noisy);
        }
    }
}
