//! `probe` reports a video stream's colour description and HDR metadata when the container has them.

use ironav_engine::call_tool;
use serde_json::json;
use std::process::Command;

#[test]
fn probe_lists_the_colour_and_hdr_metadata_of_a_video_stream() {
    let d = std::env::temp_dir().join(format!("ironav-probe-color-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let ffmpeg = |args: &[&str]| Command::new("ffmpeg").args(["-v", "error", "-y", "-nostdin"]).args(args).output().is_ok_and(|o| o.status.success());
    let first = d.join("sei.mkv");
    let x265 = "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:hdr10=1:repeat-headers=1:log-level=error:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,50):max-cll=4000,1000";
    let made = ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=size=128x72:rate=10", "-t", "1", "-pix_fmt", "yuv420p10le", "-c:v", "libx265", "-preset", "ultrafast", "-x265-params", x265,
        "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc", first.to_str().unwrap(),
    ]);
    if !made {
        eprintln!("skipped: FFmpeg with libx265 is not available");
        return;
    }
    // the container's own boxes and elements: the second encode writes no SEI
    for name in ["c.mp4", "c.mkv"] {
        let path = d.join(name);
        assert!(ffmpeg(&["-i", first.to_str().unwrap(), "-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "log-level=error", path.to_str().unwrap()]));
        let out = call_tool("probe", &json!({ "path": path.to_str().unwrap() })).unwrap();
        let s = &out["streams"][0];
        assert_eq!(s["color"]["transfer"], "pq", "{name}: {out}");
        assert_eq!((s["color"]["primaries"].as_u64(), s["color"]["hdr"].as_bool()), (Some(9), Some(true)));
        assert_eq!((s["hdr_metadata"]["max_cll"].as_u64(), s["hdr_metadata"]["max_fall"].as_u64()), (Some(4000), Some(1000)), "{name}: {out}");
        assert_eq!((s["hdr_metadata"]["mastering"]["max_nits"].as_f64(), s["hdr_metadata"]["mastering"]["min_nits"].as_f64()), (Some(4000.0), Some(0.005)), "{name}: {out}");
    }
    // a plain clip has neither key
    let plain = d.join("plain.mp4");
    assert!(ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10", "-t", "1", "-c:v", "libx265", "-preset", "ultrafast", plain.to_str().unwrap()]));
    let out = call_tool("probe", &json!({ "path": plain.to_str().unwrap() })).unwrap();
    assert!(out["streams"][0].get("color").is_none() && out["streams"][0].get("hdr_metadata").is_none(), "{out}");
    let _ = std::fs::remove_dir_all(&d);
}
