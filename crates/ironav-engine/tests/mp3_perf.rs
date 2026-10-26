//! Timing of the MP3 path: `IRONAV_MP3=path cargo test --release -p ironav-engine --test mp3_perf -- --ignored --nocapture`.

use std::time::Instant;

#[test]
#[ignore]
fn time_mp3_stages() {
    let Ok(path) = std::env::var("IRONAV_MP3") else { return };
    for _ in 0..3 {
        let t0 = Instant::now();
        let mut d = ironav_formats::open_demuxer(std::path::Path::new(&path)).unwrap();
        let info = d.streams()[0].clone();
        let t_open = t0.elapsed();
        let mut packets = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            packets.push(p);
        }
        let t_read = t0.elapsed();
        let mut dec = ironav_codec::new_decoder_for(&info).unwrap();
        let t1 = Instant::now();
        let mut n = 0usize;
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                if let ironav_core::Frame::Audio(a) = f {
                    n += a.samples.len();
                }
            }
        }
        println!("open {:?}, read {:?}, decode {:?} for {n} samples", t_open, t_read - t_open, t1.elapsed());
    }
}
