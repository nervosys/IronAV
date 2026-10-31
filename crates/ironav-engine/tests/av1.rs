//! AV1 key frames, decoded by IronAV's software decoder and compared with libaom: `ffmpeg -c:v
//! libaom-av1` encodes intra-only streams into WebM, Matroska or MP4, and IronAV's decode of each
//! must equal libaom's (`ffmpeg -c:v libaom-av1 -i`) byte for byte. The tests skip when `ffmpeg`
//! is not on the PATH.

use ironav_engine::{run, Job};
use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-av1-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap().success()
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// The picture data of a Y4M file: the frames without the stream header or the FRAME lines.
fn y4m_payload(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.extend_from_slice(&rest[6..next]);
        rest = &rest[next..];
    }
    out
}

/// The post filters libaom leaves on: none unless a test turns deblocking on.
const NO_FILTERS: &str = "enable-cdef=0:enable-restoration=0:loopfilter-control=0";

struct Case<'a> {
    name: &'a str,
    container: &'a str,
    /// The lavfi source.
    src: &'a str,
    pix_fmt: &'a str,
    /// libaom's options after `-aom-params`, and any further encoder arguments.
    params: &'a str,
    args: &'a [&'a str],
    frames: u32,
}

/// Encodes the case, decodes it with IronAV (software) and FFmpeg's libaom, and requires equal
/// pictures. Returns the number of bytes compared.
fn check(c: &Case) -> usize {
    let d = scratch(c.name);
    let src = d.join(format!("src.{}", c.container));
    let raw = d.join("ref.raw");
    let y4m = d.join("out.y4m");
    let n = c.frames.to_string();
    let mut enc = vec!["-f", "lavfi", "-i", c.src, "-frames:v", &n, "-c:v", "libaom-av1", "-g", "1", "-cpu-used", "6", "-pix_fmt", c.pix_fmt, "-aom-params", c.params];
    enc.extend_from_slice(c.args);
    enc.push(src.to_str().unwrap());
    assert!(ffmpeg(&enc), "{}: ffmpeg could not encode the stream", c.name);
    assert!(ffmpeg(&["-c:v", "libaom-av1", "-i", src.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", c.pix_fmt, raw.to_str().unwrap()]), "{}: ffmpeg could not decode", c.name);
    let j = Job { input: src.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    let r = run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{}: {e}", c.name));
    assert_eq!(r.format, "y4m", "{}", c.name);
    let (got, want) = (y4m_payload(&std::fs::read(&y4m).unwrap()), std::fs::read(&raw).unwrap());
    if got != want {
        let i = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
        panic!("{}: IronAV decodes libaom's AV1 differently ({} bytes, libaom {}; first difference at byte {i})", c.name, got.len(), want.len());
    }
    let _ = std::fs::remove_dir_all(&d);
    want.len()
}

fn case<'a>(name: &'a str, src: &'a str, params: &'a str, args: &'a [&'a str]) -> Case<'a> {
    Case { name, container: "webm", src, pix_fmt: "yuv420p", params, args, frames: 2 }
}

/// The deblocking filter on, CDEF and loop restoration off.
const DEBLOCK: &str = "enable-cdef=0:enable-restoration=0";

const TESTSRC: &str = "testsrc2=size=128x72:rate=25";

macro_rules! need_ffmpeg {
    () => {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
    };
}

#[test]
fn key_frames_decode_bit_exactly() {
    need_ffmpeg!();
    assert_eq!(check(&case("small", TESTSRC, NO_FILTERS, &["-crf", "30"])), 2 * 128 * 72 * 3 / 2);
    assert_eq!(check(&case("deblocked", TESTSRC, DEBLOCK, &["-crf", "30"])), 2 * 128 * 72 * 3 / 2);
    assert_eq!(check(&case("odd", "testsrc2=size=130x74:rate=25", DEBLOCK, &["-crf", "30"])), 2 * (130 * 74 + 2 * 65 * 37));
    check(&case("mandelbrot", "mandelbrot=size=640x360:rate=25", DEBLOCK, &["-crf", "20"]));
}

#[test]
fn the_quantiser_range_decodes_bit_exactly() {
    need_ffmpeg!();
    for crf in ["0", "10", "40", "55", "63"] {
        check(&case(&format!("crf{crf}"), TESTSRC, DEBLOCK, &["-crf", crf]));
    }
}

#[test]
fn other_bit_depths_and_samplings_decode_bit_exactly() {
    need_ffmpeg!();
    for (name, pix_fmt) in [("10bit", "yuv420p10le"), ("444", "yuv444p"), ("444-10bit", "yuv444p10le"), ("422", "yuv422p"), ("12bit", "yuv420p12le"), ("gray", "gray")] {
        check(&Case { pix_fmt, ..case(name, TESTSRC, DEBLOCK, &["-crf", "30"]) });
    }
}

/// CDEF on (libaom's default) with deblocking, loop restoration off.
const CDEF: &str = "enable-restoration=0";

#[test]
fn cdef_decodes_bit_exactly() {
    need_ffmpeg!();
    check(&case("cdef", TESTSRC, CDEF, &["-crf", "30"]));
    check(&case("cdef-strong", "mandelbrot=size=320x180:rate=25", CDEF, &["-crf", "50"]));
    check(&case("cdef-odd", "testsrc2=size=130x74:rate=25", CDEF, &["-crf", "45"]));
    for (name, pix_fmt) in [("cdef-10bit", "yuv420p10le"), ("cdef-12bit", "yuv420p12le"), ("cdef-444", "yuv444p"), ("cdef-422", "yuv422p10le"), ("cdef-gray", "gray")] {
        check(&Case { pix_fmt, ..case(name, "mandelbrot=size=320x180:rate=25", CDEF, &["-crf", "40"]) });
    }
}

#[test]
fn quantiser_matrices_decode_bit_exactly() {
    need_ffmpeg!();
    let qm = "enable-qm=1:qm-min=0:qm-max=8";
    check(&case("qm", TESTSRC, &format!("{DEBLOCK}:{qm}"), &["-crf", "30"]));
    check(&case("qm-all", "mandelbrot=size=320x180:rate=25", &format!("{CDEF}:enable-qm=1:qm-min=0:qm-max=14"), &["-crf", "20"]));
    check(&Case { pix_fmt: "yuv420p10le", ..case("qm-10bit", "mandelbrot=size=320x180:rate=25", &format!("{CDEF}:{qm}"), &["-crf", "40"]) });
    check(&Case { pix_fmt: "yuv444p", ..case("qm-444", TESTSRC, &format!("{CDEF}:{qm}"), &["-crf", "25"]) });
}

#[test]
fn tiles_decode_bit_exactly() {
    need_ffmpeg!();
    let src = "testsrc2=size=1280x720:rate=25";
    // 4x2 tiles, whose CDFs the frame keeps from tile 6 (context_update_tile_id)
    check(&case("tiles", src, DEBLOCK, &["-crf", "35", "-tile-columns", "2", "-tile-rows", "1"]));
    check(&Case { pix_fmt: "yuv420p10le", ..case("tiles-10bit", src, DEBLOCK, &["-crf", "35", "-tile-columns", "3"]) });
}

#[test]
fn screen_content_and_lossless_decode_bit_exactly() {
    need_ffmpeg!();
    let screen = format!("{DEBLOCK}:tune-content=screen:enable-palette=1");
    check(&case("palette", "testsrc=size=320x180:rate=25", &screen, &["-crf", "30"]));
    check(&case("palette-bars", "smptehdbars=size=320x180:rate=25", &screen, &["-crf", "20"]));
    check(&case("lossless", TESTSRC, "lossless=1", &[]));
    check(&case("lossless-screen", "testsrc=size=320x180:rate=25", "lossless=1:tune-content=screen", &[]));
}

#[test]
fn other_containers_decode_bit_exactly() {
    need_ffmpeg!();
    for container in ["mkv", "mp4"] {
        check(&Case { container, ..case(container, TESTSRC, DEBLOCK, &["-crf", "30"]) });
    }
}

#[test]
fn unsupported_tools_are_refused() {
    need_ffmpeg!();
    let d = scratch("refuse");
    let cases = [("inter.webm", DEBLOCK, vec!["-g", "10"], "inter frames")];
    for (file, params, g, want) in cases {
        let src = d.join(file);
        let mut enc = vec!["-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25", "-frames:v", "3", "-c:v", "libaom-av1", "-cpu-used", "6", "-crf", "30", "-aom-params", params];
        enc.extend(g);
        enc.push(src.to_str().unwrap());
        assert!(ffmpeg(&enc));
        let j = Job { input: src.display().to_string(), output: d.join("out.y4m").display().to_string(), hwaccel: "off".into(), ..Job::default() };
        let e = run(&j, &mut |_| {}).err().unwrap_or_else(|| panic!("{file} decoded"));
        assert!(e.to_string().contains(want), "{file}: {e}");
    }
    let _ = std::fs::remove_dir_all(&d);
}
