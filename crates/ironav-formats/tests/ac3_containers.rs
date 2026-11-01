//! AC-3 in its containers: a stream made with FFmpeg's AC-3 encoder, read with `open_demuxer`, decodes
//! with the engine's decoder for the stream to what FFmpeg's decoder makes of the same file. Skipped
//! without `ffmpeg` on PATH.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIGNAL: &str = "aevalsrc=exprs='0.4*sin(2*PI*440*t)+0.3*(random(0)-0.5)*(lt(mod(t,0.25),0.02))|0.3*sin(2*PI*660*t)':s=48000:d=2";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ac3-container-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// FFmpeg's decode of `file` as f32 samples.
fn reference(dir: &Path, name: &str, file: &Path) -> Vec<f32> {
    let out = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    std::fs::read(&out).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Demux `file` with the formats registry, decode its AC-3 packets, and return the samples with the
/// stream's rate and channel count.
fn demux_and_decode(file: &Path) -> (Vec<f32>, u32, u16) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, CodecId::Ac3);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    // the engine's decoder for the stream, which keeps the samples the container's edit keeps
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("an AC-3 decoder");
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        let p = Packet { stream: 0, ..p };
        for f in dec.decode(&p).expect("a decoded frame") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (sample_rate, channels));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    }
    (out, sample_rate, channels)
}

/// Compare the demuxed and decoded samples with FFmpeg's, and check the worst difference.
fn check(name: &str, got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let worst = got.iter().zip(want).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
    eprintln!("{name:>16}: max abs diff {worst:.3e} over {} samples", got.len());
    // The bound is the decoder's float error on this signal (the codec's own test measures it on
    // others, at up to 2.5e-5); a container that dropped or reordered frames would miss by far more.
    assert!(worst < 5e-5, "{name}: worst difference {worst}");
}

/// Encode a stereo AC-3 stream into `name` (any container FFmpeg writes it to) and compare what our
/// demuxer and decoder make of it with FFmpeg's decode.
fn encode_and_compare(dir: &Path, name: &str, format: &str, channels: &str) -> PathBuf {
    let file = dir.join(name);
    assert!(
        ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", channels, "-c:a", "ac3", "-b:a", "192k", "-f", format, file.to_str().unwrap()]),
        "encoding {name}"
    );
    let want = reference(dir, name, &file);
    let (got, _, _) = demux_and_decode(&file);
    check(name, &got, &want);
    file
}

#[test]
fn matroska_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    encode_and_compare(&dir, "stereo_192.mkv", "matroska", "2");
    encode_and_compare(&dir, "surround_384.mkv", "matroska", "6");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A raw AC-3 file copied into Matroska with our writer reads back to the same samples.
#[test]
fn raw_ac3_copied_to_matroska_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let raw = dir.join("copy_source.ac3");
    assert!(ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", "2", "-c:a", "ac3", "-b:a", "192k", "-f", "ac3", raw.to_str().unwrap()]), "encoding the source");
    let want = reference(&dir, "copy_source", &raw);
    let mut input = open_demuxer(&raw).expect("the raw file opens");
    let streams = input.streams().to_vec();
    let out = dir.join("copied.mkv");
    let mut muxer = ironav_formats::create_muxer(&out, "mkv").expect("a Matroska writer");
    muxer.write_header(&streams).expect("the header");
    while let Some(p) = input.read_packet().expect("a packet") {
        muxer.write_packet(&p).expect("the packet");
    }
    muxer.finish().expect("the trailer");
    let (got, _, _) = demux_and_decode(&out);
    check("copied.mkv", &got, &want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn raw_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    for (name, channels, kbps) in [("raw_stereo_192", "2", "192k"), ("raw_surround_384", "6", "384k")] {
        let file = dir.join(format!("{name}.ac3"));
        assert!(
            ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", channels, "-c:a", "ac3", "-b:a", kbps, "-f", "ac3", file.to_str().unwrap()]),
            "encoding {name}"
        );
        let want = reference(&dir, name, &file);
        let (got, _, _) = demux_and_decode(&file);
        check(name, &got, &want);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
