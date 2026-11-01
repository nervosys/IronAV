//! The encoder options a job takes: `bframes` (HEVC and MPEG-4 B pictures), and for AV1 `tiles`,
//! `quarter_mv` and `altref`. Each setting must change the stream the way it says (ffprobe shows the B
//! pictures, FFmpeg's trace of the headers shows the tile grid and the hidden frames), the streams must
//! decode to the frames IronAV encoded, and bad values or codecs are refused by name. The stream checks
//! skip when `ffmpeg` and `ffprobe` are not on the PATH; the refusals need neither.

use ironav_engine::{plan, run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-encopts-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tool_available(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A y4m file of `frames` 4:2:0 frames with a moving gradient, so that motion search has work to do.
fn write_source(dir: &Path, w: usize, h: usize, frames: usize) -> PathBuf {
    let path = dir.join("src.y4m");
    let mut out = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420jpeg\n").into_bytes();
    for t in 0..frames {
        out.extend_from_slice(b"FRAME\n");
        for y in 0..h {
            for x in 0..w {
                out.push(((x + 3 * t + y / 2) * 2) as u8);
            }
        }
        for plane in 0..2 {
            for y in 0..h / 2 {
                for x in 0..w / 2 {
                    out.push((128 + ((x + t + plane * 7 + y) % 9) as i32 - 4) as u8);
                }
            }
        }
    }
    std::fs::write(&path, out).unwrap();
    path
}

/// A job from `src` to `dir/name` with the software encoder and `codec`, `qp` 26 unless given.
fn job(src: &Path, dir: &Path, name: &str, codec: &str, out_ext: &str) -> Job {
    let mut j = Job { input: src.display().to_string(), output: dir.join(format!("{name}.{out_ext}")).display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    j.video.codec = Some(codec.into());
    j.video.qp = Some(26);
    j
}

/// The output of `ffprobe` for `args` on the first video stream of `path`, one line per frame or entry.
fn probe(path: &Path, entries: &str) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    String::from_utf8(out.stdout).unwrap().lines().map(|l| l.trim().trim_end_matches(',').to_string()).collect()
}

/// The lines of FFmpeg's header trace of `path` (`trace_headers`) that name `field`, with the value
/// each one gives at the end of the line.
fn trace_values(path: &Path, field: &str) -> Vec<u64> {
    let out = Command::new("ffmpeg")
        .args(["-v", "trace", "-i"])
        .arg(path)
        .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.split_whitespace().any(|t| t == field))
        .map(|l| l.split_whitespace().last().unwrap().parse().unwrap())
        .collect()
}

/// The decoded frames of `stream` by IronAV and by FFmpeg, which must be the same, as the frame count.
fn decode_matches_ffmpeg(dir: &Path, stream: &Path, name: &str, frames: usize, frame_bytes: usize) {
    let raw = dir.join(format!("{name}.ffmpeg.raw"));
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(stream)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&raw)
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: FFmpeg could not decode the stream");
    let y4m = dir.join(format!("{name}.ironav.y4m"));
    let j = Job { input: stream.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{name}: IronAV could not decode: {e}"));
    let data = std::fs::read(&y4m).unwrap();
    let mut got = Vec::new();
    let mut i = data.iter().position(|&b| b == b'\n').unwrap() + 1;
    while i < data.len() {
        let eol = i + data[i..].iter().position(|&b| b == b'\n').unwrap();
        got.extend_from_slice(&data[eol + 1..eol + 1 + frame_bytes]);
        i = eol + 1 + frame_bytes;
    }
    let want = std::fs::read(&raw).unwrap();
    assert_eq!(got.len(), frames * frame_bytes, "{name}: IronAV's frame count");
    assert!(got == want, "{name}: IronAV decodes the stream differently from FFmpeg");
}

fn refusal(j: &Job) -> String {
    match plan(j) {
        Ok(_) => panic!("the job was accepted: {:?}", j.video),
        Err(e) => e.to_string(),
    }
}

#[test]
fn bframes_put_b_pictures_in_hevc_and_mpeg4_streams() {
    if !(tool_available("ffmpeg") && tool_available("ffprobe")) {
        eprintln!("ffmpeg and ffprobe not on PATH; skipping");
        return;
    }
    let dir = scratch("bframes");
    let (w, h, frames) = (160usize, 96usize, 8usize);
    let src = write_source(&dir, w, h, frames);
    for codec in ["hevc", "mpeg4"] {
        let mut kinds = Vec::new();
        for n in [0u32, 2] {
            let mut j = job(&src, &dir, &format!("{codec}{n}"), codec, "mp4");
            j.video.bframes = Some(n);
            let out = PathBuf::from(&j.output);
            run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{codec} with {n} B pictures: IronAV could not encode: {e}"));
            let types = probe(&out, "frame=pict_type");
            assert_eq!(types.len(), frames, "{codec} {n}: the frame count");
            kinds.push(types.iter().filter(|t| *t == "B").count());
            if n == 2 {
                decode_matches_ffmpeg(&dir, &out, &format!("{codec}{n}"), frames, w * h * 3 / 2);
                if codec == "mpeg4" {
                    let profile = probe(&out, "stream=profile");
                    assert!(profile[0].contains("Advanced Simple"), "B pictures take the Advanced Simple profile: {profile:?}");
                }
            }
        }
        assert_eq!(kinds[0], 0, "{codec}: no B pictures by default");
        assert!(kinds[1] >= frames / 3, "{codec}: two B pictures between anchors give about two in three frames of B: {kinds:?}");
    }
}

#[test]
fn tiles_quarter_mv_and_altref_show_in_av1_streams() {
    if !(tool_available("ffmpeg") && tool_available("ffprobe")) {
        eprintln!("ffmpeg and ffprobe not on PATH; skipping");
        return;
    }
    let dir = scratch("av1");
    let (w, h, frames) = (320usize, 192usize, 8usize);
    let src = write_source(&dir, w, h, frames);
    let frame_bytes = w * h * 3 / 2;

    // two tile columns and one row: the tile counts in the frame headers, and the same pictures out
    let mut j = job(&src, &dir, "tiles", "av1", "webm");
    j.video.tiles = Some("2x1".into());
    let tiles = PathBuf::from(&j.output);
    run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("tiles: IronAV could not encode: {e}"));
    let cols = trace_values(&tiles, "tile_cols_log2");
    let rows = trace_values(&tiles, "tile_rows_log2");
    assert!(!cols.is_empty() && cols.iter().all(|&c| c == 1), "2x1 tiles: tile_cols_log2 is 1 in each frame, got {cols:?}");
    assert!(rows.iter().all(|&r| r == 0), "2x1 tiles: one tile row, got {rows:?}");
    decode_matches_ffmpeg(&dir, &tiles, "tiles", frames, frame_bytes);

    // quarter-sample motion: a different stream at the same quantiser, decoding the same way
    let plain = job(&src, &dir, "plain", "av1", "webm");
    run(&plain, &mut |_| {}).unwrap_or_else(|e| panic!("plain: IronAV could not encode: {e}"));
    let mut qpel = job(&src, &dir, "qpel", "av1", "webm");
    qpel.video.quarter_mv = true;
    let qpel_out = PathBuf::from(&qpel.output);
    run(&qpel, &mut |_| {}).unwrap_or_else(|e| panic!("quarter_mv: IronAV could not encode: {e}"));
    assert_ne!(std::fs::read(&qpel_out).unwrap(), std::fs::read(&plain.output).unwrap(), "quarter_mv changes the stream");
    decode_matches_ffmpeg(&dir, &qpel_out, "qpel", frames, frame_bytes);

    // hidden ALTREF frames: some frames are not shown, and show_existing_frame shows them later
    let mut alt = job(&src, &dir, "altref", "av1", "webm");
    alt.video.altref = Some(4);
    let alt_out = PathBuf::from(&alt.output);
    run(&alt, &mut |_| {}).unwrap_or_else(|e| panic!("altref: IronAV could not encode: {e}"));
    let shown = trace_values(&alt_out, "show_frame");
    let hidden = shown.iter().filter(|&&s| s == 0).count();
    assert!(hidden > 0, "altref 4: hidden frames are coded");
    let existing = trace_values(&alt_out, "show_existing_frame");
    assert_eq!(existing.iter().filter(|&&e| e == 1).count(), hidden, "each hidden frame is shown once, by show_existing_frame");
    decode_matches_ffmpeg(&dir, &alt_out, "altref", frames, frame_bytes);
}

#[test]
fn bad_encoder_options_are_refused_by_name() {
    let dir = scratch("refused");
    let src = write_source(&dir, 64, 48, 2);

    let mut j = job(&src, &dir, "a", "hevc", "mp4");
    j.video.bframes = Some(4);
    let e = refusal(&j);
    assert!(e.contains("bframes must be from 0 to 3"), "{e}");

    let mut j = job(&src, &dir, "b", "h264", "mp4");
    j.video.bframes = Some(2);
    let e = refusal(&j);
    assert!(e.contains("bframes applies to the hevc, mpeg4 and mpeg2 encoders"), "{e}");

    let mut j = job(&src, &dir, "c", "hevc", "mp4");
    j.video.bframes = Some(2);
    j.video.latency = Some(ironav_engine::Latency::Low);
    let e = refusal(&j);
    assert!(e.contains("bframes conflicts with latency low"), "{e}");

    for tiles in ["3x1", "0x1", "2x", "2x65", "x1", "2X1"] {
        let mut j = job(&src, &dir, "d", "av1", "webm");
        j.video.tiles = Some(tiles.into());
        let e = refusal(&j);
        assert!(e.contains("tiles must be COLSxROWS") && e.contains(tiles), "tiles {tiles}: {e}");
    }

    let mut j = job(&src, &dir, "e", "hevc", "mp4");
    j.video.tiles = Some("2x1".into());
    let e = refusal(&j);
    assert!(e.contains("tiles applies to the av1 encoder"), "{e}");

    let mut j = job(&src, &dir, "f", "hevc", "mp4");
    j.video.quarter_mv = true;
    let e = refusal(&j);
    assert!(e.contains("quarter_mv applies to the av1 encoder"), "{e}");

    for lag in [1usize, 17] {
        let mut j = job(&src, &dir, "g", "av1", "webm");
        j.video.altref = Some(lag as u32);
        let e = refusal(&j);
        assert!(e.contains("altref must be a lag from 2 to 16 frames"), "altref {lag}: {e}");
    }

    let mut j = job(&src, &dir, "h", "mpeg4", "mp4");
    j.video.altref = Some(4);
    let e = refusal(&j);
    assert!(e.contains("altref applies to the av1 encoder"), "{e}");

    let mut j = job(&src, &dir, "i", "av1", "webm");
    j.video.altref = Some(4);
    j.video.latency = Some(ironav_engine::Latency::Low);
    let e = refusal(&j);
    assert!(e.contains("altref conflicts with latency low"), "{e}");
}

#[test]
fn accepted_options_reach_the_plan() {
    let dir = scratch("plan");
    let src = write_source(&dir, 64, 48, 2);
    let mut j = job(&src, &dir, "p", "av1", "webm");
    j.video.tiles = Some("4x2".into());
    j.video.quarter_mv = true;
    j.video.altref = Some(8);
    let p = serde_json::to_value(plan(&j).unwrap()).unwrap();
    let step = &p["steps"][0];
    assert_eq!(step["tiles"], serde_json::json!([4, 2]));
    assert_eq!(step["quarter_mv"], serde_json::json!(true));
    assert_eq!(step["altref"], serde_json::json!(8));

    let mut j = job(&src, &dir, "q", "hevc", "mp4");
    j.video.bframes = Some(3);
    let p = serde_json::to_value(plan(&j).unwrap()).unwrap();
    assert_eq!(p["steps"][0]["bframes"], serde_json::json!(3));
}
