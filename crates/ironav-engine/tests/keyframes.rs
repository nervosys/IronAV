//! The key frame interval: `keyint` makes the H.264, HEVC, VP8 and VP9 software encoders put a key
//! frame every N frames, which ffprobe reports. The tests skip when `ffmpeg` is not on the PATH.

use ironav_engine::{plan, run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-keyframes-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// The display indices of the key frames of the first video stream, from ffprobe's frame list.
fn key_frames(path: &Path) -> Vec<usize> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=key_frame,pts", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    let mut frames: Vec<(i64, bool)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (key, pts) = l.split_once(',').unwrap();
            (pts.parse().unwrap(), key == "1")
        })
        .collect();
    frames.sort();
    frames.iter().enumerate().filter(|(_, f)| f.1).map(|(i, _)| i).collect()
}

#[test]
fn keyint_puts_key_frames_every_n_frames_in_each_software_encoder() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("keyint");
    let src = d.join("src.y4m");
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=130x74:rate=25", "-frames:v", "30", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());
    // the codec and the container that holds it
    for (codec, ext) in [("h264", "mp4"), ("hevc", "mp4"), ("vp8", "webm"), ("vp9", "webm")] {
        let out = d.join(format!("{codec}.{ext}"));
        let mut j = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), ..Job::default() };
        j.video.codec = Some(codec.into());
        j.video.keyint = Some(10);
        run(&j, &mut |_| {}).unwrap();
        assert_eq!(key_frames(&out), [0, 10, 20], "{codec}: key frames");
    }
}

#[test]
fn keyint_must_be_a_frame_count_for_a_video_encoder_with_one() {
    let d = scratch("refuse");
    let src = d.join("src.y4m");
    std::fs::write(&src, b"YUV4MPEG2 W16 H16 F25:1 Ip A1:1 C420jpeg\nFRAME\n").unwrap();
    let mut j = Job { input: src.display().to_string(), output: d.join("out.mp4").display().to_string(), hwaccel: "auto".into(), ..Job::default() };
    j.video.codec = Some("h264".into());
    j.video.keyint = Some(0);
    let e = plan(&j).unwrap_err().to_string();
    assert!(e.contains("keyint must be at least 1 frame"), "{e}");
    j.video.keyint = Some(10);
    j.output = d.join("out.mov").display().to_string();
    j.video.codec = Some("prores".into());
    let e = plan(&j).unwrap_err().to_string();
    assert!(e.contains("keyint applies to the h264, hevc, vp8 and vp9 encoders"), "{e}");
}
