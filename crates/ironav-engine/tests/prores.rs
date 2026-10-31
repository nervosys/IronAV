//! ProRes in MOV, both ways against FFmpeg: `ffmpeg -c:v prores_ks` writes it and IronAV decodes it
//! bit-exactly, and IronAV writes it and FFmpeg decodes it bit-exactly. The FFmpeg tests skip when
//! `ffmpeg` is not on the PATH.

use ironav_codec::prores::{Profile, ProResEncoder};
use ironav_core::{CodecId, Encoder, Frame, Packet, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
use ironav_engine::{plan, run, Job, ProResProfile};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-prores-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
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

/// A test pattern of `frames` pictures: the FFmpeg `testsrc2` pattern in `pix_fmt`, as the Y4M file FFmpeg writes.
fn pattern(path: &Path, size: &str, frames: u32, pix_fmt: &str) {
    let (src, n) = (format!("testsrc2=size={size}:rate=25"), frames.to_string());
    ffmpeg(&["-f", "lavfi", "-i", &src, "-frames:v", &n, "-pix_fmt", pix_fmt, path.to_str().unwrap()]);
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn ffmpeg_prores_decodes_bit_exactly_in_ironav() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("ffmpeg-to-ironav");
    // profile, the pixel format FFmpeg encodes from, and the one it decodes to (4444 is 12-bit)
    let cases = [(0, "yuv422p10le", "yuv422p10le"), (1, "yuv422p10le", "yuv422p10le"), (2, "yuv422p10le", "yuv422p10le"), (3, "yuv422p10le", "yuv422p10le"), (4, "yuv444p10le", "yuv444p12le")];
    for (profile, enc, dec) in cases {
        let (mov, raw, y4m) = (d.join(format!("p{profile}.mov")), d.join(format!("p{profile}.raw")), d.join(format!("p{profile}.y4m")));
        let p = profile.to_string();
        ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=130x74:rate=25", "-frames:v", "4", "-c:v", "prores_ks", "-profile:v", &p, "-pix_fmt", enc, mov.to_str().unwrap()]);
        let b = read(&mov);
        assert!(b.windows(4).any(|w| Profile::from_fourcc(w).is_some()), "profile {profile}: no ProRes sample entry");
        ffmpeg(&["-i", mov.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", dec, raw.to_str().unwrap()]);
        let r = run(&job(&mov, &y4m), &mut |_| {}).unwrap();
        assert_eq!(r.format, "y4m", "profile {profile}");
        assert!(y4m_payload(&read(&y4m)) == read(&raw), "profile {profile}: IronAV decodes FFmpeg's ProRes differently");
    }
}

#[test]
fn ironav_prores_decodes_bit_exactly_in_ffmpeg_and_keeps_the_picture() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("ironav-to-ffmpeg");
    let (src, mov, raw, back) = (d.join("src.y4m"), d.join("ours.mov"), d.join("ff.raw"), d.join("ours.y4m"));
    pattern(&src, "130x74", 4, "yuv422p");
    let mut j = job(&src, &mov);
    j.video.codec = Some("prores".into());
    let r = run(&j, &mut |_| {}).unwrap();
    assert_eq!(r.format, "mov");
    ffmpeg(&["-i", mov.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv422p10le", raw.to_str().unwrap()]);
    run(&job(&mov, &back), &mut |_| {}).unwrap();
    let ours = y4m_payload(&read(&back));
    assert!(ours == read(&raw), "FFmpeg decodes IronAV's ProRes differently from IronAV");

    // the 10-bit decode against the 8-bit source (scaled by four): above 45 dB at HQ
    let src8 = y4m_payload(&read(&src));
    let (mut se, mut n) = (0f64, 0usize);
    for (i, s) in src8.iter().enumerate() {
        let v = u16::from_le_bytes([ours[2 * i], ours[2 * i + 1]]) as f64;
        se += (v - *s as f64 * 4.0).powi(2);
        n += 1;
    }
    let psnr = 10.0 * (1023.0f64 * 1023.0 * n as f64 / se).log10();
    eprintln!("HQ decode against the source: {psnr:.2} dB");
    assert!(psnr > 45.0, "HQ decodes at {psnr:.1} dB");
}

#[test]
fn ironav_prores_4444_decodes_bit_exactly_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("ironav-4444");
    let (w, h) = (130u32, 74u32);
    let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv444p10, frame_rate: Rational::new(25, 1) };
    let mut enc = ProResEncoder::new(&params, Profile::Fourfour).unwrap();
    // three pictures of a gradient with some texture, in 10-bit 4:4:4
    let frames: Vec<VideoFrame> = (0..3u32)
        .map(|f| VideoFrame {
            width: w,
            height: h,
            pix_fmt: PixelFormat::Yuv444p10,
            pts: f as i64,
            planes: (0..3u32)
                .map(|p| {
                    (0..w * h)
                        .flat_map(|i| {
                            let (x, y) = (i % w, i / w);
                            let v = (300 + 3 * x + 2 * y + 40 * p + f * 7 + (x * y) % 17) as u16;
                            v.to_le_bytes()
                        })
                        .collect()
                })
                .collect(),
        })
        .collect();
    let mov = d.join("four.mov");
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::ProRes,
        time_base: Rational::new(1, 25),
        duration: Some(1),
        bits: None,
        extradata: b"ap4h".to_vec(),
        lead_in: 0,
        params: params.clone(),
    };
    let mut out = ironav_formats::create_muxer(&mov, "mov").unwrap();
    out.write_header(std::slice::from_ref(&stream)).unwrap();
    for f in &frames {
        let pkts: Vec<Packet> = enc.encode(&Frame::Video(f.clone())).unwrap();
        for p in pkts {
            out.write_packet(&p).unwrap();
        }
    }
    out.finish().unwrap();

    let raw = d.join("ff.raw");
    ffmpeg(&["-i", mov.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv444p12le", raw.to_str().unwrap()]);
    let back = d.join("ours.y4m");
    run(&job(&mov, &back), &mut |_| {}).unwrap();
    assert!(y4m_payload(&read(&back)) == read(&raw), "FFmpeg decodes IronAV's 4444 ProRes differently from IronAV");
}

fn pix(p: &StreamParams) -> Option<PixelFormat> {
    match p {
        StreamParams::Video { pix_fmt, .. } => Some(*pix_fmt),
        _ => None,
    }
}

fn ffprobe_value(path: &Path, entries: &str) -> String {
    let out = Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries, "-of", "csv=p=0"]).arg(path).output().unwrap();
    assert!(out.status.success(), "ffprobe failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn every_prores_profile_is_written_and_ffmpeg_reads_it() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("profiles");
    // the profile, the name ffprobe gives it (4444 XQ is "XQ"), the source picture (8-bit, converted by IronAV), and the
    // format FFmpeg decodes the stream to
    let cases = [
        (ProResProfile::Proxy, "Proxy", "yuv422p", "yuv422p10le"),
        (ProResProfile::Lt, "LT", "yuv422p", "yuv422p10le"),
        (ProResProfile::Standard, "Standard", "yuv422p", "yuv422p10le"),
        (ProResProfile::Hq, "HQ", "yuv422p", "yuv422p10le"),
        (ProResProfile::Fourfour, "4444", "yuv444p", "yuv444p12le"),
        (ProResProfile::FourfourXq, "XQ", "yuv444p", "yuv444p12le"),
    ];
    for (profile, name, src_fmt, dec) in cases {
        let (src, mov, raw, back) = (d.join(format!("{name}.y4m")), d.join(format!("{name}.mov")), d.join(format!("{name}.raw")), d.join(format!("{name}-back.y4m")));
        pattern(&src, "130x74", 4, src_fmt);
        let mut j = job(&src, &mov);
        j.video.codec = Some("prores".into());
        j.video.prores_profile = Some(profile);
        run(&j, &mut |_| {}).unwrap();
        assert_eq!(ffprobe_value(&mov, "stream=profile"), name, "{name}: profile as FFmpeg reports it");
        ffmpeg(&["-i", mov.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", dec, raw.to_str().unwrap()]);
        run(&job(&mov, &back), &mut |_| {}).unwrap();
        assert!(y4m_payload(&read(&back)) == read(&raw), "{name}: IronAV decodes FFmpeg's stream differently");
    }
}

#[test]
fn prores_444_profiles_keep_the_chroma_and_422_profiles_convert_it() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("plan");
    let (src, mov) = (d.join("src.mov"), d.join("out.mov"));
    // a 12-bit 4:4:4 ProRes source, as FFmpeg writes it
    ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=130x74:rate=25", "-frames:v", "2", "-c:v", "prores_ks", "-profile:v", "4", "-pix_fmt", "yuv444p12le", src.to_str().unwrap()]);
    let mut j = job(&src, &mov);
    j.video.codec = Some("prores".into());
    j.video.prores_profile = Some(ProResProfile::FourfourXq);
    let p = plan(&j).unwrap();
    assert!(p.steps[0].filters.is_empty(), "4444 XQ from 12-bit 4:4:4 converts nothing: {:?}", p.steps[0].filters);
    assert_eq!(pix(&p.output.streams[0].params), Some(PixelFormat::Yuv444p12));
    j.video.prores_profile = Some(ProResProfile::Hq);
    let p = plan(&j).unwrap();
    assert_eq!(p.steps[0].filters.iter().map(|f| f.args["pix_fmt"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ["yuv422p10le"]);
    assert_eq!(pix(&p.output.streams[0].params), Some(PixelFormat::Yuv422p10));
    assert_eq!(p.output.streams[0].extradata, b"apch".to_vec());
}

#[test]
fn prores_profile_is_refused_for_other_codecs_and_audio() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("refuse");
    let (src, mov) = (d.join("src.y4m"), d.join("out.mov"));
    pattern(&src, "130x74", 2, "yuv422p");
    let mut j = job(&src, &mov);
    j.video.codec = Some("h264".into());
    j.video.prores_profile = Some(ProResProfile::Lt);
    let e = plan(&j).unwrap_err().to_string();
    assert!(e.contains("prores_profile applies to the prores encoder only"), "{e}");
    let wav = d.join("tone.wav");
    ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=440:duration=0.1", wav.to_str().unwrap()]);
    let mut a = job(&wav, &d.join("out.wav"));
    a.audio.prores_profile = Some(ProResProfile::Lt);
    let e = plan(&a).unwrap_err().to_string();
    assert!(e.contains("video streams only"), "{e}");
}
