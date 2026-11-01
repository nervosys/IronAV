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
    /// The key frame interval and libaom's speed.
    gop: &'a str,
    cpu: &'a str,
}

/// Encodes the case, decodes it with IronAV (software) and FFmpeg's libaom, and requires equal
/// pictures. Returns the number of bytes compared.
fn check(c: &Case) -> usize {
    let d = scratch(c.name);
    let src = d.join(format!("src.{}", c.container));
    let raw = d.join("ref.raw");
    let y4m = d.join("out.y4m");
    let n = c.frames.to_string();
    let mut enc = vec!["-f", "lavfi", "-i", c.src, "-frames:v", &n, "-c:v", "libaom-av1", "-g", c.gop, "-cpu-used", c.cpu, "-pix_fmt", c.pix_fmt, "-aom-params", c.params];
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
    Case { name, container: "webm", src, pix_fmt: "yuv420p", params, args, frames: 2, gop: "1", cpu: "6" }
}

/// An inter-coded case: one key frame, then `frames - 1` inter frames.
fn inter<'a>(name: &'a str, src: &'a str, params: &'a str, args: &'a [&'a str], frames: u32, cpu: &'a str) -> Case<'a> {
    Case { frames, gop: "1000", cpu, ..case(name, src, params, args) }
}

/// The inter tools libaom can turn off, off: single and plain compound prediction only.
const NO_TOOLS: &str = "enable-cdef=0:enable-restoration=0:enable-obmc=0:enable-warped-motion=0:enable-global-motion=0:enable-masked-comp=0:enable-interintra-comp=0:enable-dist-wtd-comp=0:enable-diff-wtd-comp=0:enable-dual-filter=0:enable-ref-frame-mvs=0";

/// The deblocking filter on, CDEF and loop restoration off.
const DEBLOCK: &str = "enable-cdef=0:enable-restoration=0";

/// Every inter tool libaom picks by itself; intra block copy off (cpu-used 2 and below use it).
const TOOLS: &str = "enable-cdef=0:enable-restoration=0:enable-intrabc=0";

/// Every inter tool forced on, for cpu-used 1.
const ALL_TOOLS: &str = "enable-cdef=0:enable-restoration=0:enable-intrabc=0:enable-interintra-comp=1:enable-smooth-interintra=1:enable-interintra-wedge=1:enable-interinter-wedge=1:enable-dist-wtd-comp=1:enable-dual-filter=1";

const P_ONLY: &[&str] = &["-crf", "30", "-lag-in-frames", "0"];
const ALTREF: &[&str] = &["-crf", "30"];

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

/// Loop restoration on, with deblocking and CDEF. libaom leaves restoration off at `-cpu-used` 6
/// (the speed of the other tests), so these encode at 4; its choice of Wiener, self-guided and
/// switchable filters per unit depends on the content and the quantiser.
const RESTORATION: &str = "enable-cdef=1:enable-restoration=1";

#[test]
fn loop_restoration_decodes_bit_exactly() {
    need_ffmpeg!();
    let big = "mandelbrot=size=1280x720:rate=25";
    check(&case("lr-wiener", big, RESTORATION, &["-crf", "20", "-cpu-used", "4"]));
    check(&case("lr-switchable", big, RESTORATION, &["-crf", "35", "-cpu-used", "4"]));
    check(&case("lr-sgrproj", big, RESTORATION, &["-crf", "50", "-cpu-used", "4"]));
    check(&case("lr-1080p", "testsrc2=size=1920x1080:rate=25", RESTORATION, &["-crf", "25", "-cpu-used", "4"]));
    check(&case("lr-odd", "testsrc2=size=130x74:rate=25", RESTORATION, &["-crf", "40", "-cpu-used", "4"]));
    let mandel = "mandelbrot=size=640x360:rate=25";
    for (name, pix_fmt) in [("lr-10bit", "yuv420p10le"), ("lr-12bit", "yuv420p12le"), ("lr-444", "yuv444p"), ("lr-422", "yuv422p"), ("lr-gray", "gray"), ("lr-444-10bit", "yuv444p10le")] {
        check(&Case { pix_fmt, ..case(name, mandel, RESTORATION, &["-crf", "30", "-cpu-used", "4"]) });
    }
    check(&Case { pix_fmt: "yuv444p10le", ..case("lr-444-10bit-odd", "testsrc2=size=130x74:rate=25", RESTORATION, &["-crf", "40", "-cpu-used", "4"]) });
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
fn film_grain_decodes_bit_exactly() {
    need_ffmpeg!();
    for test in [1, 3, 8, 16] {
        check(&case(&format!("grain{test}"), "mandelbrot=size=320x180:rate=25", &format!("{DEBLOCK}:film-grain-test={test}"), &["-crf", "30"]));
    }
    check(&case("grain-odd", "testsrc2=size=131x75:rate=25", &format!("{DEBLOCK}:film-grain-test=5"), &["-crf", "35"]));
    check(&Case { pix_fmt: "yuv420p10le", ..case("grain-10bit", "mandelbrot=size=320x180:rate=25", &format!("{DEBLOCK}:film-grain-test=2"), &["-crf", "30"]) });
    check(&Case { pix_fmt: "yuv444p", ..case("grain-444", TESTSRC, &format!("{DEBLOCK}:film-grain-test=7"), &["-crf", "25"]) });
    check(&Case { pix_fmt: "gray", ..case("grain-gray", TESTSRC, &format!("{DEBLOCK}:film-grain-test=9"), &["-crf", "25"]) });
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
    // intra block copy: libaom uses it for screen content at cpu-used 2
    let cases = [("intrabc.webm", "tune-content=screen:enable-intrabc=1", "testsrc=size=320x180:rate=25", "2", "intra block copy")];
    for (file, params, lavfi, cpu, want) in cases {
        let src = d.join(file);
        let mut enc = vec!["-f", "lavfi", "-i", lavfi, "-frames:v", "1", "-c:v", "libaom-av1", "-cpu-used", cpu, "-crf", "30", "-aom-params", params];
        enc.push(src.to_str().unwrap());
        assert!(ffmpeg(&enc));
        let j = Job { input: src.display().to_string(), output: d.join("out.y4m").display().to_string(), hwaccel: "off".into(), ..Job::default() };
        let e = run(&j, &mut |_| {}).err().unwrap_or_else(|| panic!("{file} decoded"));
        assert!(e.to_string().contains(want), "{file}: {e}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn p_frames_decode_bit_exactly() {
    need_ffmpeg!();
    check(&inter("p-plain", TESTSRC, NO_TOOLS, P_ONLY, 8, "6"));
    check(&inter("p-tools", TESTSRC, TOOLS, P_ONLY, 8, "6"));
}

#[test]
fn altref_and_hidden_frames_decode_bit_exactly() {
    need_ffmpeg!();
    // altrefs, overlays, show_existing_frame, compound and skip mode
    check(&inter("altref-plain", TESTSRC, NO_TOOLS, ALTREF, 10, "6"));
    check(&inter("altref", TESTSRC, TOOLS, ALTREF, 10, "6"));
}

#[test]
fn encoder_speeds_decode_bit_exactly() {
    need_ffmpeg!();
    let src = "testsrc2=size=160x90:rate=25";
    for cpu in ["8", "4", "2"] {
        check(&inter(&format!("cpu{cpu}"), src, TOOLS, ALTREF, 8, cpu));
        check(&inter(&format!("cpu{cpu}-p"), src, TOOLS, P_ONLY, 6, cpu));
    }
}

#[test]
fn motion_tools_decode_bit_exactly() {
    need_ffmpeg!();
    // a zoom: global and local warp, OBMC, wedges, interintra, switchable filters
    let zoom = "mandelbrot=size=192x108:rate=25:start_scale=3:end_scale=0.3";
    check(&inter("zoom", zoom, ALL_TOOLS, ALTREF, 10, "1"));
    check(&inter("life", "life=size=160x90:rate=25:mold=10:ratio=0.5", TOOLS, ALTREF, 10, "2"));
}

#[test]
fn inter_formats_decode_bit_exactly() {
    need_ffmpeg!();
    for (name, pix_fmt) in [("inter-10bit", "yuv420p10le"), ("inter-12bit", "yuv420p12le"), ("inter-444", "yuv444p"), ("inter-422", "yuv422p"), ("inter-gray", "gray")] {
        check(&Case { pix_fmt, ..inter(name, TESTSRC, TOOLS, ALTREF, 6, "6") });
    }
    assert_eq!(check(&inter("inter-odd", "testsrc2=size=130x74:rate=25", TOOLS, ALTREF, 6, "4")), 6 * (130 * 74 + 2 * 65 * 37));
    for container in ["mkv", "mp4"] {
        check(&Case { container, ..inter(container, TESTSRC, TOOLS, ALTREF, 6, "6") });
    }
}

#[test]
fn post_filters_on_inter_frames_decode_bit_exactly() {
    need_ffmpeg!();
    let src = "testsrc2=size=160x90:rate=25";
    check(&inter("inter-cdef-lr", src, "enable-cdef=1:enable-restoration=1:enable-intrabc=0", ALTREF, 8, "4"));
    check(&inter("inter-qm", src, "enable-intrabc=0:enable-qm=1:qm-min=4:qm-max=10", ALTREF, 6, "6"));
    // grain on inter, hidden and shown-existing frames (libaom always resends the parameters)
    let zoom = "mandelbrot=size=160x90:rate=25:start_scale=3:end_scale=0.3";
    check(&inter("inter-grain", zoom, &format!("{TOOLS}:film-grain-test=1"), ALTREF, 8, "6"));
    check(&inter("inter-grain-p", zoom, &format!("{TOOLS}:film-grain-test=3"), P_ONLY, 6, "6"));
}

#[test]
fn inter_tiles_and_long_streams_decode_bit_exactly() {
    need_ffmpeg!();
    check(&inter("inter-tiles", "testsrc2=size=1280x720:rate=25", TOOLS, &["-crf", "35", "-tile-columns", "2", "-tile-rows", "1"], 4, "6"));
    assert_eq!(check(&inter("long", TESTSRC, TOOLS, ALTREF, 32, "6")), 32 * 128 * 72 * 3 / 2);
    // segmentation, delta q and error resilience
    check(&inter("aq", TESTSRC, &format!("{TOOLS}:aq-mode=1:deltaq-mode=1"), ALTREF, 8, "6"));
    check(&inter("resilient", TESTSRC, &format!("{TOOLS}:error-resilient=1"), P_ONLY, 6, "6"));
}
