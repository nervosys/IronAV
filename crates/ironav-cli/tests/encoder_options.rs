//! `ironav convert` takes the encoder options: `--bframes`, `--tiles`, `--quarter-mv` and `--altref`.
//! They reach the plan (checked with `--dry-run`), and bad values or codecs fail with a message.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-cli-encopts-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A small y4m file: two 64x48 frames.
fn write_source(dir: &Path) -> PathBuf {
    let path = dir.join("src.y4m");
    let (w, h) = (64usize, 48usize);
    let mut out = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420jpeg\n").into_bytes();
    for t in 0..2 {
        out.extend_from_slice(b"FRAME\n");
        out.extend((0..w * h * 3 / 2).map(|i| ((i + 5 * t) % 251) as u8));
    }
    std::fs::write(&path, out).unwrap();
    path
}

fn convert(src: &Path, out: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ironav"))
        .arg("convert")
        .args(["-i"])
        .arg(src)
        .args(["-o"])
        .arg(out)
        .args(["--hwaccel", "off", "-y"])
        .args(extra)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn options_reach_the_plan() {
    let dir = scratch("plan");
    let src = write_source(&dir);
    let out = dir.join("x.mp4");
    let o = convert(&src, &out, &["--vcodec", "hevc", "--bframes", "2", "--dry-run"]);
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["result"]["steps"][0]["bframes"], 2, "{v}");

    let out = dir.join("x.webm");
    let o = convert(&src, &out, &["--vcodec", "av1", "--tiles", "2x1", "--quarter-mv", "--altref", "4", "--dry-run"]);
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let step = &v["result"]["steps"][0];
    assert_eq!(step["tiles"], serde_json::json!([2, 1]), "{v}");
    assert_eq!(step["quarter_mv"], true, "{v}");
    assert_eq!(step["altref"], 4, "{v}");
}

#[test]
fn bad_encoder_options_fail_with_a_message() {
    let dir = scratch("refused");
    let src = write_source(&dir);
    let out = dir.join("x.mp4");
    let cases: [(&[&str], &str); 6] = [
        (&["--vcodec", "hevc", "--bframes", "4"], "--bframes must be a number of B pictures from 0 to 3"),
        (&["--vcodec", "h264", "--bframes", "2"], "bframes applies to the hevc, mpeg4 and mpeg2 encoders"),
        (&["--vcodec", "av1", "--tiles", "3x1"], "--tiles must be COLSxROWS"),
        (&["--vcodec", "av1", "--altref", "1"], "--altref must be a lag from 2 to 16 frames"),
        (&["--vcodec", "hevc", "--quarter-mv"], "quarter_mv applies to the av1 encoder"),
        (&["--vcodec", "hevc", "--bframes", "2", "--latency", "low"], "bframes conflicts with latency low"),
    ];
    for (args, want) in cases {
        let o = convert(&src, &out, &[args, &["--dry-run"]].concat());
        assert!(!o.status.success(), "{args:?} is refused");
        assert!(text(&o).contains(want), "{args:?}: expected '{want}' in {}", text(&o));
    }
}
