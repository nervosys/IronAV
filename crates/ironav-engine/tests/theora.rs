//! A Theora file in Ogg goes through the engine like any other video: the codec registry finds the
//! decoder, and the pictures are re-encoded. The test skips when `ffmpeg` (with libtheora) is not on the PATH.

use ironav_engine::{run, Job};
use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-theora-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have_libtheora() -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("libtheora"))
}

#[test]
fn an_ogg_theora_file_is_decoded_and_re_encoded() {
    if !have_libtheora() {
        eprintln!("ffmpeg with libtheora not on PATH; skipping");
        return;
    }
    let d = scratch("transcode");
    let src = d.join("src.ogv");
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25", "-frames:v", "30", "-c:v", "libtheora", "-q:v", "6", "-g", "12"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());
    let out = d.join("out.mp4");
    let mut j = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    j.video.codec = Some("h264".into());
    run(&j, &mut |_| {}).unwrap();
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "csv=p=0"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(probe.status.success());
    assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "h264,176,144,30");
    let _ = std::fs::remove_dir_all(&d);
}
