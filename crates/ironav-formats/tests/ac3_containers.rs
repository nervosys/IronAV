//! AC-3 in its containers: a stream made with FFmpeg's AC-3 encoder, read with `open_demuxer`, decodes
//! with `Ac3Decoder` to what FFmpeg's decoder makes of the same file. Skipped without `ffmpeg` on PATH.

use ironav_codec::ac3::Ac3Decoder;
use ironav_core::{CodecId, Decoder, Demuxer, Frame, Packet, StreamParams};
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
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ac3-container-{}", std::process::id()));
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
    let mut dec = Ac3Decoder::new();
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
