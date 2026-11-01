use ironav_core::ErrorCode;
use ironav_engine::{call_tool, ontology, plan, run, FilterCall, Job};
use serde_json::json;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-test-{}-{name}", std::process::id()));
    // a reused process id must not see an earlier run's files
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_wav(path: &std::path::Path, samples: &[i16], channels: u16, rate: u32) {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    f.extend_from_slice(&(channels * 2).to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

fn pcm(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap()[44..].to_vec()
}

#[test]
fn s16_to_f32_to_s16_is_lossless() {
    let d = scratch("roundtrip");
    let samples: Vec<i16> = (0..20_000).map(|i| ((i * 37) % 65536 - 32768) as i16).collect();
    let (a, b, c) = (d.join("a.wav"), d.join("b.wav"), d.join("c.wav"));
    write_wav(&a, &samples, 2, 44100);

    let mut j = job(&a, &b);
    j.audio.codec = Some("pcm_f32le".into());
    run(&j, &mut |_| {}).unwrap();
    let mut j = job(&b, &c);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();

    assert_eq!(pcm(&a), pcm(&c));
}

#[test]
fn volume_halves_amplitude_and_clips() {
    let d = scratch("volume");
    let (a, b) = (d.join("a.wav"), d.join("b.wav"));
    write_wav(&a, &[1000, -1000, 32767, -32768], 1, 8000);
    let mut j = job(&a, &b);
    j.filters.push(FilterCall::parse_shorthand("volume:gain=2").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out: Vec<i16> = pcm(&b).chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    assert_eq!(out, vec![2000, -2000, 32767, -32768]);
}

#[test]
fn copy_is_bit_exact_and_plan_says_copy() {
    let d = scratch("copy");
    let (a, b) = (d.join("a.wav"), d.join("b.wav"));
    write_wav(&a, &(0..9000).map(|i| i as i16).collect::<Vec<_>>(), 1, 16000);
    let p = plan(&job(&a, &b)).unwrap();
    assert_eq!(serde_json::to_value(p.steps[0].action).unwrap(), "copy");
    run(&job(&a, &b), &mut |_| {}).unwrap();
    assert_eq!(pcm(&a), pcm(&b));
}

#[test]
fn errors_carry_codes_and_recovery_info() {
    let d = scratch("errors");
    let (a, b) = (d.join("a.wav"), d.join("b.wav"));
    write_wav(&a, &[1, 2, 3, 4], 1, 8000);

    let mut j = job(&a, &b);
    j.audio.codec = Some("aac".into());
    let e = plan(&j).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
    assert!(e.alternatives.contains(&"pcm_s16le".to_string()));

    let mut j = job(&a, &b);
    j.audio.codec = Some("not-a-codec".into());
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::UnsupportedCodec);

    let mut j = job(&a, &b);
    j.hwaccel = "require".into();
    j.audio.codec = Some("pcm_f32le".into());
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::HardwareUnavailable);

    run(&job(&a, &b), &mut |_| {}).unwrap();
    assert_eq!(plan(&job(&a, &b)).unwrap_err().code, ErrorCode::OutputExists);

    let missing = d.join("nope.wav");
    assert_eq!(plan(&job(&missing, &b)).unwrap_err().code, ErrorCode::NotFound);

    let mut j = job(&a, &d.join("c.wav"));
    j.filters.push(FilterCall::parse_shorthand("gray").unwrap());
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::InvalidArgument);
}

#[test]
fn failed_run_leaves_no_partial_or_output() {
    let d = scratch("atomic");
    let (a, b) = (d.join("trunc.y4m"), d.join("out.y4m"));
    // Two frames declared, the second one cut short: the reader fails after the first succeeds.
    let mut f = b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420jpeg
FRAME
".to_vec();
    f.extend_from_slice(&[7u8; 12]);
    f.extend_from_slice(b"FRAME
");
    f.extend_from_slice(&[7u8; 5]);
    std::fs::write(&a, f).unwrap();
    let mut j = job(&a, &b);
    j.filters.push(FilterCall::parse_shorthand("hflip").unwrap());
    let e = run(&j, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInput);
    assert!(!b.exists(), "no output at the real path");
    assert!(!d.join("out.y4m.partial").exists(), "partial file cleaned up");
}

#[test]
fn wav_trailing_partial_frame_is_dropped_not_fatal() {
    let d = scratch("wavtrunc");
    let (a, b) = (d.join("a.wav"), d.join("b.wav"));
    write_wav(&a, &[1, 2, 3, 4], 1, 8000);
    let mut bytes = std::fs::read(&a).unwrap();
    bytes.pop();
    std::fs::write(&a, bytes).unwrap();
    run(&job(&a, &b), &mut |_| {}).unwrap();
    assert_eq!(pcm(&b).len(), 6); // 3 whole 16-bit frames
}

#[test]
fn y4m_hflip_and_gray() {
    let d = scratch("y4m");
    let (a, b, c) = (d.join("a.y4m"), d.join("b.y4m"), d.join("c.y4m"));
    let mut f = b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420jpeg\n".to_vec();
    f.extend_from_slice(b"FRAME\n");
    f.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // Y
    f.extend_from_slice(&[9, 10, 11, 12]); // U 2x1
    std::fs::write(&a, &f).unwrap();
    // 4x2 yuv420p has chroma 2x1 each => 8 + 2 + 2 bytes
    f.extend_from_slice(&[0u8; 0]);
    let mut full = b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420jpeg\nFRAME\n".to_vec();
    full.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    std::fs::write(&a, &full).unwrap();

    let mut j = job(&a, &b);
    j.filters.push(FilterCall::parse_shorthand("hflip").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out = std::fs::read(&b).unwrap();
    let body = &out[out.len() - 12..];
    assert_eq!(body, &[4, 3, 2, 1, 8, 7, 6, 5, 10, 9, 12, 11]);

    let mut j = job(&a, &c);
    j.filters.push(FilterCall::parse_shorthand("gray").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out = std::fs::read(&c).unwrap();
    assert!(String::from_utf8_lossy(&out[..40]).contains("Cmono"));
    assert_eq!(&out[out.len() - 8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn ten_bit_video_flips_scales_and_converts() {
    let d = scratch("y4m10");
    let (a, b, c, e) = (d.join("a.y4m"), d.join("b.y4m"), d.join("c.y4m"), d.join("e.y4m"));
    // 8x4 4:2:0, 10-bit: luma values 0, 100, ... (little-endian words), then flat chroma.
    let words = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    let luma: Vec<u16> = (0..32).map(|i| (i * 33) as u16).collect(); // 0..=1023
    let mut f = b"YUV4MPEG2 W8 H4 F30:1 Ip A1:1 C420p10
".to_vec();
    f.extend_from_slice(b"FRAME
");
    f.extend_from_slice(&words(&luma));
    f.extend_from_slice(&words(&[512; 8]));
    f.extend_from_slice(&words(&[512; 8]));
    std::fs::write(&a, &f).unwrap();

    // hflip keeps whole 16-bit samples together.
    let mut j = job(&a, &b);
    j.filters.push(FilterCall::parse_shorthand("hflip").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out = std::fs::read(&b).unwrap();
    assert!(String::from_utf8_lossy(&out[..40]).contains("C420p10"));
    let body = &out[out.len() - 96..];
    let got: Vec<u16> = body[..64].chunks(2).map(|w| u16::from_le_bytes([w[0], w[1]])).collect();
    assert_eq!(&got[..8], &[231, 198, 165, 132, 99, 66, 33, 0]);

    // scale keeps the format and halves the size.
    let mut j = job(&a, &c);
    j.filters.push(FilterCall::parse_shorthand("scale:width=4,height=2").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out = std::fs::read(&c).unwrap();
    assert!(String::from_utf8_lossy(&out[..50]).contains("W4 H2") && String::from_utf8_lossy(&out[..50]).contains("C420p10"));
    let header_end = out.iter().position(|&x| x == b'\n').unwrap() + 1;
    assert_eq!(out.len() - header_end, 6 + (4 * 2 + 2 * 2) * 2);

    // format to 8-bit: the header changes and full scale maps to 255.
    let mut j = job(&a, &e);
    j.filters.push(FilterCall::parse_shorthand("format:pix_fmt=yuv420p").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let out = std::fs::read(&e).unwrap();
    assert!(String::from_utf8_lossy(&out[..40]).contains("C420jpeg"));
    assert_eq!(out[out.len() - 17], 255); // last luma sample, 31 * 33 = 1023, with 16 chroma bytes after it
}

#[test]
fn ontology_is_consistent_with_registries() {
    let o = ontology();
    for c in ironav_codec::implemented() {
        let e = o.get(&format!("codec/{}", c.codec.name())).expect("implemented codec is in ontology");
        assert_eq!(serde_json::to_value(e.status).unwrap(), "implemented");
    }
    for c in ironav_core::CodecId::ALL {
        assert!(o.get(&format!("codec/{}", c.name())).is_some());
    }
    for f in ironav_formats::FORMATS {
        let e = o.get(&format!("container/{}", f.name)).expect("a registered container is in the ontology");
        assert_eq!(serde_json::to_value(e.status).unwrap(), "implemented", "{}", f.name);
    }
    for r in o.relations() {
        assert!(o.get(&r.subject).is_some(), "dangling subject {}", r.subject);
        assert!(o.get(&r.object).is_some(), "dangling object {}", r.object);
    }
    // Every tool is discoverable as an operation entity.
    for t in ironav_engine::tools() {
        assert!(o.get(&format!("op/{}", t.name)).is_some());
    }
    let ld = o.to_jsonld();
    assert!(ld["@graph"].as_array().unwrap().len() > o.entities().count());
}

#[test]
fn ontology_answers_conversion_questions() {
    let r = call_tool("ontology_explain_conversion", &json!({ "from": "pcm_s16le", "to": "pcm_f32le" })).unwrap();
    assert_eq!(r["feasible_in_this_build"], true);
    let r = call_tool("ontology_explain_conversion", &json!({ "from": "h264", "to": "av1" })).unwrap();
    // AV1 has a software encoder (key frames), so the only blocker that used to apply is gone
    assert_eq!(r["feasible_in_this_build"], true, "{r}");
    assert!(r["blockers"].as_array().unwrap().is_empty(), "{r}");
    let e = call_tool("ontology_describe", &json!({ "id": "h265x" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

// ---- FLAC ------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Raw bytes of the `data` chunk of a canonical WAV file.
fn wav_data(path: &std::path::Path) -> Vec<u8> {
    let b = std::fs::read(path).unwrap();
    let mut p = 12;
    while p + 8 <= b.len() {
        let len = u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
        if &b[p..p + 4] == b"data" {
            return b[p + 8..p + 8 + len].to_vec();
        }
        p += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

/// Files in `tests/fixtures` were written by the reference libFLAC (via libsndfile), so these
/// check the decoder against an independent implementation rather than against ourselves.
#[test]
fn decodes_reference_libflac_files_bit_exactly() {
    let d = scratch("flacref");
    for (flac, wav, codec) in [("ref_stereo16.flac", "ref_stereo16.wav", "pcm_s16le"), ("ref_mono24.flac", "ref_mono24.wav", "pcm_s24le")] {
        let out = d.join(wav);
        let mut j = job(&fixture(flac), &out);
        j.audio.codec = Some(codec.into());
        run(&j, &mut |_| {}).unwrap();
        assert_eq!(wav_data(&out), wav_data(&fixture(wav)), "{flac} decoded differently from libFLAC");
    }
}

#[test]
fn wav_to_flac_to_wav_is_lossless_at_16_and_24_bits() {
    let d = scratch("flacrt");
    let a = d.join("a.wav");
    // 16-bit: from the libFLAC reference WAV; back to WAV at the FLAC's depth without naming a codec
    for src in [fixture("ref_stereo16.wav"), fixture("ref_mono24.wav")] {
        let (f, back) = (d.join("x.flac"), d.join("back.wav"));
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(&back);
        let mut j = job(&src, &f);
        j.audio.codec = Some("flac".into());
        let r = run(&j, &mut |_| {}).unwrap();
        assert!(r.bytes_out < r.bytes_in, "FLAC should compress this signal");
        run(&job(&f, &back), &mut |_| {}).unwrap();
        assert_eq!(wav_data(&back), wav_data(&src));
    }
    let _ = a;
}

#[test]
fn flac_plan_picks_bit_depth_and_explains_wav_mismatch() {
    let d = scratch("flacplan");
    let p = plan(&{
        let mut j = job(&fixture("ref_mono24.wav"), &d.join("o.flac"));
        j.audio.codec = Some("flac".into());
        j
    })
    .unwrap();
    assert_eq!(p.output.streams[0].bits, Some(24));

    // FLAC into WAV is decoded to PCM at its depth, unless FLAC is asked for: then the error says
    // which codecs would work
    let p = plan(&job(&fixture("ref_stereo16.flac"), &d.join("o.wav"))).unwrap();
    assert_eq!(p.output.streams[0].codec, ironav_core::CodecId::PcmS16Le);
    let e = plan(&{
        let mut j = job(&fixture("ref_stereo16.flac"), &d.join("o.wav"));
        j.audio.codec = Some("flac".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
    assert!(e.alternatives.contains(&"pcm_s16le".to_string()));
}

#[test]
fn corrupt_flac_fails_cleanly_without_output() {
    let d = scratch("flaccorrupt");
    let bad = d.join("bad.flac");
    let mut bytes = std::fs::read(fixture("ref_stereo16.flac")).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x55;
    std::fs::write(&bad, bytes).unwrap();
    let out = d.join("out.wav");
    let mut j = job(&bad, &out);
    j.audio.codec = Some("pcm_s16le".into());
    let e = run(&j, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInput);
    assert!(!out.exists() && !d.join("out.wav.partial").exists());
}

fn write_y4m(path: &std::path::Path, w: usize, h: usize, frames: usize) {
    let mut f = format!("YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420jpeg
").into_bytes();
    for t in 0..frames {
        f.extend_from_slice(b"FRAME
");
        f.extend((0..w * h).map(|i| ((i % w) * 3 + (i / w) + t * 5) as u8));
        f.extend(std::iter::repeat_n(128u8, w * h / 2));
    }
    std::fs::write(path, f).unwrap();
}

/// H.264 is encoded by NVENC when a usable GPU is present and in software otherwise (or with
/// hardware off); either way the result is a real Annex B stream that decodes back to the source.
#[test]
fn h264_encodes_on_the_gpu_or_in_software() {
    let d = scratch("h264");
    let (src, out) = (d.join("in.y4m"), d.join("out.h264"));
    write_y4m(&src, 256, 144, 30);

    let mut off = job(&src, &d.join("soft.h264"));
    off.video.codec = Some("h264".into());
    off.video.qp = Some(28);
    off.hwaccel = "off".into();
    let report = run(&off, &mut |_| {}).unwrap();
    assert_eq!(report.steps[0].hardware_encoder, None);
    assert_eq!(report.steps[0].packets_out, 30);
    let bytes = std::fs::read(d.join("soft.h264")).unwrap();
    assert!(bytes.starts_with(&[0, 0, 0, 1, 0x67]), "Annex B, starting with the SPS");
    assert!(bytes.len() < 256 * 144 * 3 * 30 / 2 / 20, "compressed at least 20:1, got {} bytes", bytes.len());

    let mut bad = job(&src, &out);
    bad.video.codec = Some("h264".into());
    bad.video.qp = Some(20);
    bad.video.bitrate_kbps = Some(1000);
    assert_eq!(plan(&bad).unwrap_err().code, ErrorCode::InvalidArgument);
    let wav = d.join("in.wav");
    write_wav(&wav, &[0i16; 200], 1, 8000);
    let mut audio_opt = job(&wav, &d.join("o.wav"));
    audio_opt.audio.qp = Some(20);
    assert_eq!(plan(&audio_opt).unwrap_err().code, ErrorCode::InvalidArgument);

    // MP4 carries the software encoder's stream with its configuration in avcC.
    let mp4 = d.join("out.mp4");
    let mut m = job(&src, &mp4);
    m.video.codec = Some("h264".into());
    m.video.qp = Some(28);
    m.hwaccel = "off".into();
    run(&m, &mut |_| {}).unwrap();
    let b = std::fs::read(&mp4).unwrap();
    assert_eq!(&b[4..8], b"ftyp");
    assert!(b.windows(4).any(|w| w == b"moov") && b.windows(4).any(|w| w == b"avcC"));
    let back = d.join("soft.y4m");
    let mut dec = job(&mp4, &back);
    dec.video.codec = Some("rawvideo".into());
    dec.hwaccel = "off".into();
    run(&dec, &mut |_| {}).unwrap();
    let (a, b) = (y4m_frames(&src), y4m_frames(&back));
    assert_eq!(b.len(), 30);
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let mse = x.iter().zip(y).map(|(p, q)| (*p as f64 - *q as f64).powi(2)).sum::<f64>() / x.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        assert!(psnr > 34.0, "frame {i} only {psnr:.1} dB");
    }
    let mut keep = job(&src, &d.join("copy.mp4"));
    keep.video.codec = Some("copy".into());
    assert_eq!(plan(&keep).unwrap_err().code, ErrorCode::UnsupportedCodec, "rawvideo cannot go in an mp4");

    let mut j = job(&src, &out);
    j.video.codec = Some("h264".into());
    j.video.qp = Some(28);
    // with hardware allowed, a usable GPU is preferred
    let report = run(&j, &mut |_| {}).unwrap();
    assert!(matches!(report.steps[0].hardware_encoder, None | Some("cuda")));
    assert_eq!(report.steps[0].packets_out, 30);
    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes.starts_with(&[0, 0, 0, 1]), "Annex B start code");
    assert!(bytes.len() > 100 && bytes.len() < 256 * 144 * 3 * 30 / 4, "plausible compressed size, got {}", bytes.len());
}

#[test]
fn schema_and_ontology_know_hardware_codecs() {
    let schema = ironav_engine::tools::job_schema().to_string();
    assert!(schema.contains("bitrate_kbps") && schema.contains("\"h264\""));
    let o = ontology();
    let h264 = o.get("codec/h264").expect("h264 entity");
    assert_eq!(h264.status, ironav_ontology::Status::Implemented);
    assert!(o.get("codec/mpeg4").is_some_and(|e| e.status == ironav_ontology::Status::Implemented));
    // AV1 is decoded and encoded in software, so it is usable without a GPU.
    let av1 = o.get("codec/av1").expect("av1 entity");
    assert_eq!(av1.status, ironav_ontology::Status::Implemented);
    for k in ["software_decode", "software_encode"] {
        assert_eq!(av1.properties.get(k).and_then(|v| v.as_bool()), Some(true), "av1 {k}");
    }
}

fn tone(n: usize) -> Vec<i16> {
    (0..n).map(|i| (((i as f64) * 0.05).sin() * 12000.0) as i16).collect()
}

fn has(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn audio_only_mp4_holds_flac_and_nothing_else() {
    let d = scratch("mp4audio");
    let (wav, out) = (d.join("in.wav"), d.join("out.mp4"));
    write_wav(&wav, &tone(44_100), 1, 44_100);

    // PCM is not an MP4 codec; the error must say what is.
    let e = plan(&job(&wav, &out)).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
    assert!(e.alternatives.iter().any(|a| a == "flac"), "{:?}", e.alternatives);

    let mut j = job(&wav, &out);
    j.audio.codec = Some("flac".into());
    let report = run(&j, &mut |_| {}).unwrap();
    assert_eq!(report.format, "mp4");
    let b = std::fs::read(&out).unwrap();
    assert_eq!(&b[4..8], b"ftyp");
    assert!(has(&b, b"fLaC") && has(&b, b"dfLa") && has(&b, b"soun") && has(&b, b"moov"));
    assert!(!has(&b, b"vide"), "no video track");
}

#[test]
fn video_and_audio_files_combine_into_one_mp4() {
    let d = scratch("mp4av");
    let (src, wav, out) = (d.join("in.y4m"), d.join("in.wav"), d.join("out.mp4"));
    write_y4m(&src, 256, 144, 30);
    write_wav(&wav, &tone(44_100), 2, 44_100);
    let mut j = job(&src, &out);
    j.extra_inputs = vec![wav.display().to_string()];
    j.video.codec = Some("h264".into());
    j.video.qp = Some(28);
    j.audio.codec = Some("flac".into());

    let report = run(&j, &mut |_| {}).unwrap();
    assert_eq!(report.steps.len(), 2);
    assert_eq!(report.plan.output.streams.len(), 2);
    assert_eq!((report.steps[0].input_stream, report.steps[1].input_stream), (0, 0));
    assert_eq!(report.plan.extra_inputs.len(), 1);
    let b = std::fs::read(&out).unwrap();
    assert!(has(&b, b"avcC") && has(&b, b"fLaC") && has(&b, b"vide") && has(&b, b"soun"));
    let secs = report.media_seconds.unwrap();
    assert!((secs - 1.0).abs() < 0.05, "one second of media, got {secs}");
}

#[test]
fn a_second_stream_of_the_same_kind_is_dropped_with_a_warning() {
    let d = scratch("mp4two");
    let (a, b, out) = (d.join("a.wav"), d.join("b.wav"), d.join("out.mp4"));
    write_wav(&a, &tone(22_050), 1, 22_050);
    write_wav(&b, &tone(8_000), 1, 8_000);
    let mut j = job(&a, &out);
    j.extra_inputs = vec![b.display().to_string()];
    j.audio.codec = Some("flac".into());
    let p = plan(&j).unwrap();
    assert_eq!(p.steps.len(), 1);
    assert_eq!(p.steps[0].input_file, 0);
    assert!(p.warnings.iter().any(|w| w.contains("input 1 stream 0") && w.contains("dropped")), "{:?}", p.warnings);
    run(&j, &mut |_| {}).unwrap();
    // Only the first file's audio is in the output: its rate, not the second's.
    let bytes = std::fs::read(&out).unwrap();
    let at = bytes.windows(4).position(|w| w == b"dfLa").unwrap();
    let packed = u64::from_be_bytes(bytes[at + 12 + 10..at + 12 + 18].try_into().unwrap());
    assert_eq!((packed >> 44, packed & 0xF_FFFF_FFFF), (22_050, 22_050));

    // An extra input that is also the output is refused before anything is written.
    let mut same = job(&a, &b);
    same.extra_inputs = vec![b.display().to_string()];
    same.overwrite = true;
    assert_eq!(plan(&same).unwrap_err().code, ErrorCode::InvalidArgument);
}

fn y4m_frames(path: &std::path::Path) -> Vec<Vec<u8>> {
    let d = std::fs::read(path).unwrap();
    let hdr = d.iter().position(|&b| b == b'\n').unwrap() + 1;
    let head = String::from_utf8_lossy(&d[..hdr]).to_string();
    let get = |p: char| head.split_whitespace().find(|t| t.starts_with(p)).unwrap()[1..].parse::<usize>().unwrap();
    let size = get('W') * get('H') * 3 / 2;
    let mut at = hdr;
    let mut frames = Vec::new();
    while at < d.len() {
        let nl = at + d[at..].iter().position(|&b| b == b'\n').unwrap() + 1;
        frames.push(d[nl..nl + size].to_vec());
        at = nl + size;
    }
    frames
}

#[test]
fn mp4_decodes_on_the_gpu_or_fails_precisely() {
    let d = scratch("mp4dec");
    let (src, mp4, back) = (d.join("in.y4m"), d.join("mid.mp4"), d.join("back.y4m"));
    write_y4m(&src, 256, 144, 30);
    let mut enc = job(&src, &mp4);
    enc.video.codec = Some("h264".into());
    enc.video.qp = Some(10);
    if run(&enc, &mut |_| {}).unwrap().steps[0].hardware_encoder.is_none() {
        // no usable GPU: the software encoder wrote the file, and there is no GPU decoder to test
        return;
    }

    // With hardware off the software decoder is planned instead of the GPU's.
    let mut off = job(&mp4, &back);
    off.video.codec = Some("rawvideo".into());
    off.hwaccel = "off".into();
    let p = plan(&off).unwrap();
    assert!(p.steps[0].hardware.iter().all(|h| h.direction != ironav_hwaccel::Direction::Decode));

    let mut j = job(&mp4, &back);
    j.video.codec = Some("rawvideo".into());
    let report = run(&j, &mut |_| {}).unwrap();
    assert_eq!(report.steps[0].hardware_decoder, Some("cuda"));
    assert_eq!(report.steps[0].frames, 30);

    let (a, b) = (y4m_frames(&src), y4m_frames(&back));
    assert_eq!(b.len(), 30, "every frame comes back, in order");
    // QP 10 is close to lossless: the decoded frames must resemble the source closely.
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let mse = x.iter().zip(y).map(|(p, q)| (*p as f64 - *q as f64).powi(2)).sum::<f64>() / x.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        assert!(psnr > 38.0, "frame {i} only {psnr:.1} dB");
    }

    // Copying the stream needs no decoder, so it works with hardware off.
    let again = d.join("again.mp4");
    let mut copy = job(&mp4, &again);
    copy.hwaccel = "off".into();
    let r = run(&copy, &mut |_| {}).unwrap();
    assert_eq!(r.steps[0].packets_in, r.steps[0].packets_out);
    assert_eq!(r.steps[0].hardware_decoder, None);
}

#[test]
fn a_stream_can_be_left_out_with_codec_none() {
    let d = scratch("none");
    let (wav, out) = (d.join("in.wav"), d.join("out.mp4"));
    write_wav(&wav, &tone(8_000), 1, 8_000);
    let mut j = job(&wav, &out);
    j.audio.codec = Some("none".into());
    // Nothing is left to write, and the plan says why instead of producing an empty file.
    let e = plan(&j).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInput, "{e:?}");
}

#[test]
fn scale_changes_the_output_size_and_the_plan_says_so() {
    let d = scratch("scale");
    let (src, out) = (d.join("in.y4m"), d.join("out.y4m"));
    write_y4m(&src, 256, 144, 5);

    let mut j = job(&src, &out);
    j.filters.push(FilterCall::parse_shorthand("scale:width=128").unwrap());
    let p = plan(&j).unwrap();
    let dims = |p: &ironav_engine::Plan| match &p.output.streams[0].params {
        ironav_core::StreamParams::Video { width, height, .. } => (*width, *height),
        _ => panic!("video expected"),
    };
    assert_eq!(dims(&p), (128, 72), "height follows the aspect ratio");
    run(&j, &mut |_| {}).unwrap();
    let head = String::from_utf8_lossy(&std::fs::read(&out).unwrap()[..40]).to_string();
    assert!(head.contains("W128") && head.contains("H72"), "{head}");
    assert_eq!(y4m_frames(&out).len(), 5);

    for bad in ["scale", "scale:width=0", "scale:width=12.5", "scale:depth=3"] {
        let mut j = job(&src, &d.join("bad.y4m"));
        j.filters.push(FilterCall::parse_shorthand(bad).unwrap());
        assert_eq!(plan(&j).unwrap_err().code, ErrorCode::InvalidArgument, "{bad}");
    }
}

#[test]
fn format_filter_converts_pixel_formats_and_explains_mistakes() {
    let d = scratch("format");
    let (src, out) = (d.join("in.y4m"), d.join("out.y4m"));
    write_y4m(&src, 64, 48, 3);
    let mut j = job(&src, &out);
    j.filters.push(FilterCall::parse_shorthand("format:pix_fmt=yuv444p").unwrap());
    run(&j, &mut |_| {}).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert!(String::from_utf8_lossy(&bytes[..60]).contains("C444"));
    let first_frame_at = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
    assert!(bytes.len() - first_frame_at >= 3 * (64 * 48 * 3 + 6), "three 4:4:4 frames");

    let mut bad = job(&src, &d.join("bad.y4m"));
    bad.filters.push(FilterCall::parse_shorthand("format:pix_fmt=yuv999").unwrap());
    let e = plan(&bad).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.alternatives.iter().any(|a| a == "yuv420p"));
    let mut missing = job(&src, &d.join("bad.y4m"));
    missing.filters.push(FilterCall::parse_shorthand("format").unwrap());
    assert_eq!(plan(&missing).unwrap_err().code, ErrorCode::InvalidArgument);
}

/// 0.8 s of stereo AAC-LC from FFmpeg's encoder with short windows, TNS, PNS, intensity and mid/side
/// stereo all in use; the reference is what FFmpeg's decoder produces from the same file.
#[test]
fn aac_decodes_to_what_ffmpeg_decodes() {
    aac_matches_ffmpeg("aac", "aac_stereo_tools.m4a");
}

/// The same audio as raw ADTS (remuxed by FFmpeg) decodes to the same samples.
#[test]
fn aac_in_adts_decodes_to_what_ffmpeg_decodes() {
    aac_matches_ffmpeg("aac-adts", "aac_stereo_tools.aac");
}

/// The same audio remuxed into Matroska (by FFmpeg) decodes to the same samples.
#[test]
fn aac_in_matroska_decodes_to_what_ffmpeg_decodes() {
    aac_matches_ffmpeg("aac-mka", "aac_stereo_tools.mka");
}

/// Where an audio edit ends: FFmpeg's AAC in MP4 (an edit list, whose segment is shorter than the padded
/// frames) and the same frames in Matroska (a codec delay and discard padding) both decode to the 8000
/// samples FFmpeg gives, and MP3 in MP4 (an `mp4a` entry with an MPEG audio object type) to its 24000.
#[test]
fn audio_edits_and_codec_delays_end_where_ffmpeg_ends() {
    let dir = scratch("audio-edits");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (file, reference, worst_allowed) in [("aac_sine16k.m4a", "aac_sine16k.ffmpeg.s16", 2), ("aac_sine16k.mka", "aac_sine16k.ffmpeg.s16", 2), ("mp3_sine48k.mp4", "mp3_sine48k.ffmpeg.s16", 1)] {
        let out = dir.join(format!("{file}.wav"));
        let mut j = job(&data.join(file), &out);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let s16 = |b: Vec<u8>| -> Vec<i32> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
        let got = s16(pcm(&out));
        let want = s16(std::fs::read(data.join(reference)).unwrap());
        assert_eq!(got.len(), want.len(), "{file}: sample count against FFmpeg's decode");
        let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).max().unwrap();
        assert!(worst <= worst_allowed, "{file}: largest difference from FFmpeg's decode is {worst} LSB");
    }
}

/// Six and eight channels (5.1 in MP4 and 7.1 in ADTS, encoded by our encoder from one tone per channel
/// at 16 kHz): our decoder gives what FFmpeg's decodes, with the layouts in FFmpeg's order (front left,
/// front right, centre, LFE, back left, back right, then the side pair for 7.1).
#[test]
fn aac_multichannel_decodes_to_what_ffmpeg_decodes() {
    let dir = scratch("aac-multichannel");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (file, reference, channels) in [("aac_51.m4a", "aac_51.ffmpeg.s16", 6usize), ("aac_71.aac", "aac_71.ffmpeg.s16", 8)] {
        let out = dir.join(format!("{file}.wav"));
        let mut j = job(&data.join(file), &out);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let s16 = |b: Vec<u8>| -> Vec<i16> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]])).collect() };
        let got = s16(wav_data(&out));
        let want = s16(std::fs::read(data.join(reference)).unwrap());
        assert_eq!(got.len(), want.len(), "{file}: sample count against FFmpeg's decode");
        assert_eq!(got.len() % channels, 0, "{file}");
        let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 2, "{file}: largest difference from FFmpeg's decode is {worst} LSB");
    }
}

/// AAC encoding of six and eight channels into MP4, ADTS and Matroska: each channel's tone comes back on
/// that channel from our decoder. MP4 (edit list) and Matroska (codec delay and discard padding) remove the
/// encoder's priming and end padding, so they hold exactly the input's samples; ADTS keeps the priming
/// frame and every frame whole, as FFmpeg reads it.
#[test]
fn aac_encoder_multichannel_output_decodes_to_its_own_channels() {
    let dir = scratch("aac-enc-multi");
    let rate = 44_100u32;
    let n = rate as usize / 2;
    for ch in [6usize, 8] {
        let samples: Vec<i16> = (0..n * ch)
            .map(|i| {
                let (t, c) = ((i / ch) as f64 / rate as f64, (i % ch) as f64);
                (6000.0 * (2.0 * std::f64::consts::PI * (300.0 + 190.0 * c) * t).sin()) as i16
            })
            .collect();
        let wav = dir.join(format!("in{ch}.wav"));
        write_wav(&wav, &samples, ch as u16, rate);
        for (ext, lead) in [("m4a", 0usize), ("aac", 1024), ("mka", 0)] {
            let out = dir.join(format!("out{ch}.{ext}"));
            let mut j = job(&wav, &out);
            j.audio.codec = Some("aac".into());
            run(&j, &mut |_| {}).unwrap();
            let back = dir.join(format!("back{ch}.{ext}.wav"));
            let mut j = job(&out, &back);
            j.audio.codec = Some("pcm_s16le".into());
            run(&j, &mut |_| {}).unwrap();
            let got: Vec<f64> = wav_data(&back).as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as f64).collect();
            let decoded = if ext == "aac" { (n + 1024).div_ceil(1024) * 1024 } else { n };
            assert_eq!(got.len(), decoded * ch, "{ch} channels in {ext}: sample count");
            for c in 0..ch {
                let (mut sig, mut err) = (0.0, 0.0);
                for t in 0..n {
                    let (x, y) = (samples[t * ch + c] as f64, got[(t + lead) * ch + c]);
                    sig += x * x;
                    err += (x - y) * (x - y);
                }
                let db = 10.0 * (sig / err.max(1e-9)).log10();
                assert!(db > 18.0, "{ch} channels in {ext}, channel {c}: {db:.1} dB");
            }
        }
    }
}

fn aac_matches_ffmpeg(scratch_name: &str, file: &str) {
    let dir = scratch(scratch_name);
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let input = data.join(file);
    let out = dir.join("out.wav");
    let mut j = job(&input, &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let got: Vec<i16> = pcm(&out).as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let want: Vec<i16> = std::fs::read(data.join("aac_stereo_tools.ffmpeg.s16")).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    assert_eq!(got.len(), want.len());
    let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 2, "largest difference from FFmpeg's decode is {worst} LSB");
}

/// Decode an Opus file and compare it with a reference decode of the same packets (pre-skip removed
/// in both; ours keeps the encoder's trailing padding, which the reference trims). Returns the SNR
/// in dB and the worst difference in LSB. `reference` is the file in tests/data holding the
/// reference decode as 16-bit samples: libopus's (`ffmpeg -c:a libopus`) in every mode.
/// `late` is how many samples (interleaved) the reference runs behind ours.
fn opus_vs_reference(file: &str, ext: &str, late: usize, reference: &str) -> (f64, i32) {
    let dir = scratch(&format!("opus-{file}-{ext}"));
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join(format!("{file}.{ext}")), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let s16 = |b: Vec<u8>| -> Vec<i32> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
    let got = s16(pcm(&out));
    let want = s16(std::fs::read(data.join(reference)).unwrap());
    assert!(got.len() >= want.len() && got.len() < want.len() + 4 * 2000, "{} samples against {}", got.len(), want.len());
    let want = &want[late..];
    let (mut sig, mut err, mut worst) = (0f64, 0f64, 0);
    for (a, b) in got.iter().zip(want) {
        sig += (*b as f64).powi(2);
        err += ((a - b) as f64).powi(2);
        worst = worst.max((a - b).abs());
    }
    (10.0 * (sig / err.max(1e-9)).log10(), worst)
}

/// Decode an MP3 file and compare it with FFmpeg's `mp3float` output, which starts 1105 samples in
/// (576 of the encoder's delay and 529 more, as FFmpeg's MP3 reader trims a LAME stream) and is
/// trimmed at the end too. IronAV's reader does the same from the LAME tag; for a file without one
/// (the Matroska copy) the start is skipped here.
fn mp3_vs_ffmpeg(file: &str, ext: &str, channels: usize) -> (f64, i32) {
    let dir = scratch(&format!("mp3-{file}-{ext}"));
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join(format!("{file}.{ext}")), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let s16 = |b: Vec<u8>| -> Vec<i32> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
    let got = s16(pcm(&out));
    let want = s16(std::fs::read(data.join(format!("{file}.ffmpeg.s16"))).unwrap());
    // the .mp3 files carry a LAME tag, so the reader trims them itself; the Matroska copy has none
    let got = if ext == "mp3" { &got[..] } else { &got[1105 * channels..] };
    assert!(got.len() >= want.len(), "{} samples against {}", got.len(), want.len());
    if ext == "mp3" {
        assert_eq!(got.len(), want.len(), "the LAME tag should give FFmpeg's length");
    }
    let (mut sig, mut err, mut worst) = (0f64, 0f64, 0);
    for (a, b) in got.iter().zip(&want) {
        sig += (*b as f64).powi(2);
        err += ((a - b) as f64).powi(2);
        worst = worst.max((a - b).abs());
    }
    (10.0 * (sig / err.max(1e-9)).log10(), worst)
}

/// MPEG-1, MPEG-2 and MPEG-2.5 files from LAME: long, start, short and stop blocks, and joint
/// stereo with and without mid/side. The difference from FFmpeg is rounding to 16 bits.
#[test]
fn mp3_decodes_to_what_ffmpeg_decodes() {
    // the first file also as the same frames in Matroska
    for (file, ext, ch) in [("mp3_mpeg1_stereo", "mp3", 2), ("mp3_mpeg1_stereo", "mka", 2), ("mp3_mpeg2_mono", "mp3", 1), ("mp3_mpeg25_mono", "mp3", 1)] {
        let (snr, worst) = mp3_vs_ffmpeg(file, ext, ch);
        assert!(worst <= 1 && snr > 80.0, "{file}.{ext}: largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
    }
}

/// The same packets in an Ogg file decode to the same samples as in Matroska.
#[test]
fn ogg_opus_decodes_like_the_same_packets_in_matroska() {
    let (snr, worst) = opus_vs_reference("opus_celt_stereo", "opus", 0, "opus_celt_stereo.libopus.s16");
    assert!(worst <= 2, "largest difference from libopus is {worst} LSB (SNR {snr:.1} dB)");
    let (snr, _) = opus_vs_reference("opus_silk_mono", "opus", 0, "opus_silk_mono.libopus.s16");
    assert!(snr > 35.0, "SNR against libopus is only {snr:.1} dB");
}

/// CELT (stereo, with dual stereo, intensity and collapsed bands) decodes to what libopus decodes, to
/// 2 LSB: the reference is libopus's own decode of the file (`ffmpeg -c:a libopus`).
#[test]
fn celt_opus_decodes_to_what_libopus_decodes() {
    let (snr, worst) = opus_vs_reference("opus_celt_stereo", "mka", 0, "opus_celt_stereo.libopus.s16");
    assert!(worst <= 2, "largest difference from libopus is {worst} LSB (SNR {snr:.1} dB)");
}

/// SILK and hybrid frames decode to what libopus decodes, to a few LSB: the resampler is libopus's,
/// and the SILK synthesis differs from libopus's fixed-point arithmetic by rounding (see the README).
#[test]
fn silk_and_hybrid_opus_decode_close_to_what_libopus_decodes() {
    for file in ["opus_silk_mono", "opus_hybrid_stereo"] {
        let (snr, worst) = opus_vs_reference(file, "mka", 0, &format!("{file}.libopus.s16"));
        eprintln!("{file}: SNR {snr:.1} dB, worst {worst} LSB");
        assert!(snr > 35.0, "{file}: SNR against libopus is only {snr:.1} dB");
    }
}

/// SILK in each bandwidth and frame size, and hybrid, mono and stereo: FFmpeg's libopus encodes a
/// harmonic tone mix in each mode, and IronAV's decode of each file is compared with libopus's
/// decode of the same file. Narrow band is coded from 8 kHz input and medium band from 12 kHz, which
/// is how libopus picks the bandwidth. The output must be in step with libopus's (least mean
/// difference at lag 0) and above 35 dB SNR (25 dB for stereo, whose SILK is not isolated; see the
/// README). The differences on this tonal signal are larger
/// than on speech (SILK's own synthesis, see the README).
#[test]
fn opus_silk_and_hybrid_in_every_bandwidth_decode_like_libopus() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("opus-silk-modes");
    // (mode, input rate, application, bit rate per channel in kb/s)
    let modes = [("nb", 8000, "voip", 8), ("mb", 12000, "voip", 12), ("wb", 48000, "voip", 12), ("swb-hybrid", 48000, "audio", 24), ("fb-hybrid", 48000, "audio", 40)];
    let s16 = |b: &[u8]| -> Vec<i32> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
    for channels in [1usize, 2] {
        for (mode, rate, application, kbps) in modes {
            for ms in [10, 20, 40, 60] {
                let name = format!("{mode}-{channels}ch-{ms}ms");
                let enc = dir.join(format!("{name}.opus"));
                // a voice-like tone mix, a different pitch in each channel, with a slow swell
                let tone = |f: u32| format!("0.25*(sin(2*PI*{f}*t)+0.5*sin(4*PI*{f}*t)+0.25*sin(6*PI*{f}*t))*(0.6+0.4*sin(2*PI*2*t))");
                let expr = if channels == 1 { tone(150) } else { format!("{}|{}", tone(150), tone(170)) };
                let source = format!("aevalsrc={expr}:s={rate}:d=1.5");
                let bitrate = format!("{}k", kbps * channels as u32);
                let st = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-y", "-f", "lavfi", "-i", &source, "-c:a", "libopus", "-application", application, "-b:a", &bitrate, "-frame_duration", &ms.to_string()])
                    .arg(&enc)
                    .status()
                    .unwrap();
                assert!(st.success(), "ffmpeg could not encode {name}");
                let reference = std::process::Command::new("ffmpeg").args(["-v", "error", "-c:a", "libopus", "-i"]).arg(&enc).args(["-f", "s16le", "-"]).output().unwrap();
                assert!(reference.status.success(), "libopus could not decode {name}");
                let want = s16(&reference.stdout);
                let out = dir.join(format!("{name}.wav"));
                let mut j = job(&enc, &out);
                j.audio.codec = Some("pcm_s16le".into());
                run(&j, &mut |_| {}).unwrap();
                let got = s16(&pcm(&out));
                let n = got.len().min(want.len());
                assert!(n > 40_000 * channels, "{name}: {n} samples to compare");
                let (mut sig, mut err, mut worst) = (0f64, 0f64, 0);
                for (a, b) in got[..n].iter().zip(&want[..n]) {
                    sig += (*b as f64).powi(2);
                    err += ((a - b) as f64).powi(2);
                    worst = worst.max((a - b).abs());
                }
                let snr = 10.0 * (sig / err.max(1e-9)).log10();
                // the alignment: the mean difference is least at lag 0 (SNR alone barely notices a
                // sample's shift in a low tone)
                let lag_error = |lag: isize| -> f64 {
                    let range = 9600..n - 9600;
                    range.clone().map(|i| (got[(i as isize + lag) as usize] - want[i]).abs() as f64).sum::<f64>() / range.len() as f64
                };
                let best = (-3isize..=3).min_by(|a, b| lag_error(*a).total_cmp(&lag_error(*b))).unwrap();
                eprintln!("{name}: SNR {snr:.1} dB against libopus, worst {worst} LSB, best lag {best}");
                let min_snr = if channels == 2 { 25.0 } else { 35.0 };
                assert_eq!(best, 0, "{name}: the output is out of step with libopus's");
                assert!(snr > min_snr, "{name}: SNR against libopus is only {snr:.1} dB (worst {worst} LSB)");
            }
        }
    }
}

/// 20 frames of H.264 with B-frames and the AAC clip, muxed into Matroska by FFmpeg. Copying it into an
/// MP4 must keep every packet and its time, so reading the copy gives back what the Matroska reader gave.
#[test]
fn matroska_with_b_frames_copies_into_mp4_unchanged() {
    let dir = scratch("mkv-copy");
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes_aac.mkv");
    let out = dir.join("out.mp4");
    run(&job(&input, &out), &mut |_| {}).unwrap();
    let read = |p: &std::path::Path| {
        let mut d = ironav_formats::open_demuxer(p).unwrap();
        let streams = d.streams().to_vec();
        let mut packets = Vec::new();
        while let Some(pk) = d.read_packet().unwrap() {
            // The edit list of an MP4 skips `lead_in` at the start; what is shown starts after it.
            let secs = (pk.pts - streams[pk.stream].lead_in) as f64 * streams[pk.stream].time_base.as_f64();
            packets.push((pk.stream, secs, pk.data));
        }
        (d.format_name(), streams, packets)
    };
    let (from_name, from_streams, from) = read(&input);
    let (to_name, to_streams, to) = read(&out);
    assert_eq!((from_name, to_name), ("matroska", "mp4"));
    assert_eq!(from_streams.len(), 2);
    assert_eq!(to_streams.iter().map(|s| s.codec).collect::<Vec<_>>(), from_streams.iter().map(|s| s.codec).collect::<Vec<_>>());
    assert_eq!(from.len(), 20 + 17, "20 video frames and 17 AAC packets");
    for stream in 0..2 {
        let a: Vec<_> = from.iter().filter(|p| p.0 == stream).collect();
        let b: Vec<_> = to.iter().filter(|p| p.0 == stream).collect();
        assert_eq!(a.len(), b.len(), "stream {stream}");
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x.2, y.2, "stream {stream} packet {i}: payload");
            assert!((x.1 - y.1).abs() < 1e-3, "stream {stream} packet {i}: shown at {} s in the Matroska, {} s in the copy", x.1, y.1);
        }
    }
    let shown: Vec<f64> = from.iter().filter(|p| p.0 == 0).map(|p| p.1).collect();
    assert!(shown.windows(2).any(|w| w[1] < w[0]), "the fixture is meant to have reordered frames");
}

/// Decode a Vorbis file and compare it with FFmpeg's output, which is trimmed at the end by the Ogg
/// granule position where ours is not. `legacy` selects the older FFmpeg's reading of inverse channel
/// coupling (see `VorbisDecoder::legacy_zero_coupling`), which is the one the fixtures were made with.
fn vorbis_vs_ffmpeg(file: &str, ext: &str, legacy: bool) -> (f64, i32) {
    use ironav_core::{Decoder, Frame};
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut d = ironav_formats::open_demuxer(&data.join(format!("{file}.{ext}"))).unwrap();
    let info = d.streams()[0].clone();
    assert_eq!(info.codec, ironav_core::CodecId::Vorbis);
    let params = info.params.clone();
    let mut dec: Box<dyn Decoder> = if legacy {
        Box::new(ironav_codec::vorbis::VorbisDecoder::new(&info.extradata, &params).unwrap().legacy_zero_coupling(true))
    } else {
        ironav_codec::new_decoder_for(&info).unwrap()
    };
    let mut got = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        for f in dec.decode(&p).unwrap() {
            if let Frame::Audio(a) = f {
                got.extend(a.samples.iter().map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i32));
            }
        }
    }
    let want: Vec<i32> = std::fs::read(data.join(format!("{file}.ffmpeg.s16"))).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect();
    // The length is the last page's granule position (the FFmpeg-encoder fixture's granule is 1024 frames past
    // what its reference holds, so it is not held to it). The mono fixture's reference came out 192
    // samples longer (8192 against its granule of 8000), so it is checked against the granule.
    let channels = match &params {
        ironav_core::StreamParams::Audio { channels, .. } => *channels as usize,
        _ => 1,
    };
    if file == "vorbis_mono_8k" {
        assert_eq!(got.len() / channels, 8000);
    } else if ext == "ogg" && file != "vorbis_ffmpeg_encoder" {
        assert_eq!(got.len(), want.len(), "the granule position of the last page should give FFmpeg's length");
    } else {
        assert!(got.len() >= want.len(), "{} samples against {}", got.len(), want.len());
    }
    let (mut sig, mut err, mut worst) = (0f64, 0f64, 0);
    for (a, b) in got.iter().zip(&want) {
        sig += (*b as f64).powi(2);
        err += ((a - b) as f64).powi(2);
        worst = worst.max((a - b).abs());
    }
    (10.0 * (sig / err.max(1e-9)).log10(), worst)
}

/// libvorbis streams (long and short blocks and the windows between them, mono, stereo and 5.1 with
/// their channel coupling and residue types) decode to what FFmpeg decodes, to a rounding step.
#[test]
fn vorbis_decodes_to_what_ffmpeg_decodes() {
    for (file, ext) in [("vorbis_stereo", "ogg"), ("vorbis_stereo", "mka"), ("vorbis_mono_8k", "ogg"), ("vorbis_5_1", "ogg")] {
        let (snr, worst) = vorbis_vs_ffmpeg(file, ext, false);
        assert!(worst <= 1 && snr > 80.0, "{file}.{ext}: largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
    }
}

/// FFmpeg's own Vorbis encoder makes streams in which the magnitude channel is often exactly zero where
/// the angle channel is not. The FFmpeg the fixture was decoded with then took the angle's sign the
/// other way from the specification: matched with the legacy switch the output is identical, and
/// without it only those spectral lines differ.
#[test]
fn vorbis_follows_the_specification_where_old_ffmpeg_did_not() {
    let (snr, worst) = vorbis_vs_ffmpeg("vorbis_ffmpeg_encoder", "ogg", true);
    assert!(worst <= 1 && snr > 80.0, "largest difference from the older FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
    let (snr, _) = vorbis_vs_ffmpeg("vorbis_ffmpeg_encoder", "ogg", false);
    assert!(snr > 25.0 && snr < 60.0, "SNR against the older FFmpeg is {snr:.1} dB");
}

/// The whole path, through the engine: Ogg Vorbis to WAV.
#[test]
fn vorbis_converts_to_wav() {
    let dir = scratch("vorbis-wav");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join("vorbis_5_1.ogg"), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let bytes = pcm(&out);
    assert!(bytes.len() >= 35280 * 6 * 2, "only {} bytes of audio", bytes.len());
}

/// FLAC in Ogg (made by FFmpeg's Ogg muxer from 16-bit stereo PCM): lossless, so the decoded audio is
/// the PCM it was made from, sample for sample.
#[test]
fn flac_in_ogg_decodes_to_the_original_pcm() {
    let dir = scratch("flac-in-ogg");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join("flac_in_ogg.ogg"), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    assert!(pcm(&out) == std::fs::read(data.join("flac_in_ogg.expect.s16")).unwrap(), "decoded audio differs from the source PCM");
}

/// Copying audio into Matroska keeps every packet's bytes and the codec configuration, and the
/// FLAC stream in it decodes to the PCM it was made from. (FFmpeg opens these files too: that was
/// checked by hand with PyAV on AAC, Opus, Vorbis, FLAC and MP3, packet counts equal.)
#[test]
fn audio_copied_into_matroska_keeps_its_packets() {
    let dir = scratch("mka-copy");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for file in ["aac_stereo_tools.m4a", "opus_celt_stereo.opus", "vorbis_stereo.ogg", "flac_in_ogg.ogg", "mp3_mpeg1_stereo.mp3"] {
        let out = dir.join(format!("{file}.mka"));
        run(&job(&data.join(file), &out), &mut |_| {}).unwrap();
        let mut a = ironav_formats::open_demuxer(&data.join(file)).unwrap();
        let mut b = ironav_formats::open_demuxer(&out).unwrap();
        let (sa, sb) = (a.streams()[0].clone(), b.streams()[0].clone());
        assert_eq!((sa.codec, &sa.params), (sb.codec, &sb.params), "{file}");
        // (FLAC's configuration is rebuilt; an MP3's gapless trim has no place in Matroska and is dropped)
        if !matches!(sa.codec, ironav_core::CodecId::Flac | ironav_core::CodecId::Mp3) {
            assert_eq!(sa.extradata, sb.extradata, "{file}: codec configuration");
        }
        let mut n = 0;
        while let Some(pa) = a.read_packet().unwrap() {
            let pb = b.read_packet().unwrap().unwrap_or_else(|| panic!("{file}: the copy ends early at packet {n}"));
            assert_eq!(pa.data, pb.data, "{file}: packet {n}");
            let (ta, tb) = (pa.pts as f64 * sa.time_base.as_f64(), pb.pts as f64 * sb.time_base.as_f64());
            assert!((ta - tb).abs() < 0.0015, "{file}: packet {n} at {ta} against {tb}");
            n += 1;
        }
        assert!(b.read_packet().unwrap().is_none(), "{file}: the copy has extra packets");
    }

    let mut j = job(&dir.join("flac_in_ogg.ogg.mka"), &dir.join("flac.wav"));
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    assert!(pcm(&dir.join("flac.wav")) == std::fs::read(data.join("flac_in_ogg.expect.s16")).unwrap(), "FLAC through Matroska is not the source PCM");
}

/// WebM output: its audio is Opus or Vorbis, with the `webm` document type; other audio is refused
/// with the codecs that would fit.
#[test]
fn webm_audio_is_opus_or_vorbis() {
    let dir = scratch("webm");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for file in ["opus_celt_stereo.opus", "vorbis_stereo.ogg"] {
        let out = dir.join(format!("{file}.webm"));
        run(&job(&data.join(file), &out), &mut |_| {}).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes[..64].windows(4).any(|w| w == b"webm"), "{file}: document type");
        let mut d = ironav_formats::open_demuxer(&out).unwrap();
        let mut n = 0;
        while d.read_packet().unwrap().is_some() {
            n += 1;
        }
        assert!(n > 20, "{file}: {n} packets");
    }
    let e = run(&job(&data.join("aac_stereo_tools.m4a"), &dir.join("x.webm")), &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::UnsupportedCodec);
    assert!(e.alternatives.iter().any(|a| a == "opus"));
}

/// The signal-to-noise ratio in dB of `got` against `want` (16-bit little-endian PCM), over the
/// samples both have.
fn snr_db(want: &[u8], got: &[u8]) -> f64 {
    let s = |b: &[u8]| b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as f64).collect::<Vec<_>>();
    let (a, b) = (s(want), s(got));
    let (mut sig, mut err) = (0.0, 0.0);
    for (x, y) in a.iter().zip(&b) {
        sig += x * x;
        err += (x - y) * (x - y);
    }
    10.0 * (sig / err.max(1e-9)).log10()
}

/// The Opus encoder: 48 kHz mono and stereo go into Matroska and WebM, come back out of our own
/// decoder close to the source, and keep the source's length to within the final frame.
#[test]
fn opus_encoder_output_decodes_close_to_its_input() {
    let dir = scratch("opus-enc");
    for channels in [1u16, 2] {
        let n = 48_000 * 3;
        let mut samples = Vec::new();
        for i in 0..n {
            let t = i as f64 / 48_000.0;
            let v = 0.35 * (2.0 * std::f64::consts::PI * 330.0 * t).sin() + 0.2 * (2.0 * std::f64::consts::PI * 1870.0 * t + 2.0 * (t * 3.0).sin()).sin();
            samples.push((v * 20000.0) as i16);
            if channels == 2 {
                samples.push((0.8 * v * 20000.0 + 3000.0 * (2.0 * std::f64::consts::PI * 700.0 * t).sin()) as i16);
            }
        }
        let wav = dir.join(format!("in{channels}.wav"));
        write_wav(&wav, &samples, channels, 48_000);
        for ext in ["mka", "webm"] {
            let out = dir.join(format!("out{channels}.{ext}"));
            let mut j = job(&wav, &out);
            j.audio.codec = Some("opus".into());
            run(&j, &mut |_| {}).unwrap();
            let d = ironav_formats::open_demuxer(&out).unwrap();
            assert_eq!(d.streams()[0].codec, ironav_core::CodecId::Opus);
            assert_eq!(d.streams()[0].extradata.len(), 19, "an OpusHead");
            let back = dir.join(format!("back{channels}.{ext}.wav"));
            let mut j = job(&out, &back);
            j.audio.codec = Some("pcm_s16le".into());
            run(&j, &mut |_| {}).unwrap();
            let got = pcm(&back);
            let frames = got.len() / 2 / channels as usize;
            assert_eq!(frames, n, "{ext} {channels}ch: an exact length");
            let snr = snr_db(&pcm(&wav), &got);
            assert!(snr > 20.0, "{ext} {channels}ch: {snr:.1} dB");
        }
    }
}

/// Sample-rate conversion into the Opus encoder, and the audio bit rate: 44.1 kHz audio is
/// resampled by the `resample` filter (including the samples it holds back at the end), a lower
/// `bitrate_kbps` makes a smaller file, and a bit rate on a codec without one is refused.
#[test]
fn resample_feeds_opus_and_bitrate_sets_the_size() {
    let dir = scratch("opus-rate");
    let n = 44_100 * 2;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f64 / 44_100.0;
            (20000.0 * (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.3 * (2.0 * std::f64::consts::PI * 3100.0 * t).sin())) as i16
        })
        .collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 1, 44_100);
    let resample = FilterCall::parse_shorthand("resample:rate=48000").unwrap();

    // the reference: the same resampling written straight to PCM
    let mut j = job(&wav, &dir.join("ref.wav"));
    j.filters.push(resample.clone());
    run(&j, &mut |_| {}).unwrap();
    let want = pcm(&dir.join("ref.wav"));
    assert_eq!(want.len() / 2, 96_000, "two seconds at 48 kHz");

    let mut sizes = Vec::new();
    for kbps in [24u32, 96] {
        let out = dir.join(format!("out{kbps}.mka"));
        let mut j = job(&wav, &out);
        j.filters.push(resample.clone());
        j.audio.codec = Some("opus".into());
        j.audio.bitrate_kbps = Some(kbps);
        let report = run(&j, &mut |_| {}).unwrap();
        assert_eq!(report.plan.steps[0].bitrate_kbps, Some(kbps));
        sizes.push(std::fs::metadata(&out).unwrap().len());
        let back = dir.join(format!("back{kbps}.wav"));
        let mut j = job(&out, &back);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let got = pcm(&back);
        assert!((96_000..98_000).contains(&(got.len() / 2)), "{kbps} kbps: {} samples", got.len() / 2);
        let snr = snr_db(&want, &got);
        assert!(snr > if kbps == 24 { 12.0 } else { 22.0 }, "{kbps} kbps: {snr:.1} dB");
    }
    assert!(sizes[1] > 2 * sizes[0], "{sizes:?}");

    let mut j = job(&wav, &dir.join("x.wav"));
    j.audio.codec = Some("pcm_s16le".into());
    j.audio.bitrate_kbps = Some(64);
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::InvalidArgument);
    let mut j = job(&wav, &dir.join("x.mka"));
    j.filters.push(resample);
    j.audio.codec = Some("opus".into());
    j.audio.bitrate_kbps = Some(900);
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::InvalidArgument);
}

/// The Opus settings in a job reach the encoder and its head: SILK at 60 ms with FEC and DTX writes
/// packets whose TOC says SILK at 60 ms, and they decode; automatic mode with a voice signal
/// decodes; the settings are refused for another codec and for a bad frame length.
#[test]
fn opus_settings_reach_the_encoder() {
    use ironav_engine::{OpusJob, OpusMode, OpusSignal};
    let dir = scratch("opus-settings");
    let n = 48_000;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f64 / 48_000.0;
            let env = (2.0 * std::f64::consts::PI * 3.0 * t).sin().abs();
            (6000.0 * env * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
        })
        .collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 1, 48_000);

    let out = dir.join("silk60.opus");
    let mut j = job(&wav, &out);
    j.audio.codec = Some("opus".into());
    j.audio.bitrate_kbps = Some(24);
    j.audio.opus = Some(OpusJob { mode: Some(OpusMode::Silk), frame_ms: Some(60.0), fec_loss_percent: Some(10), dtx: true, ..OpusJob::default() });
    run(&j, &mut |_| {}).unwrap();
    let mut d = ironav_formats::open_demuxer(&out).unwrap();
    let mut packets = 0;
    while let Some(p) = d.read_packet().unwrap() {
        let config = p.data[0] >> 3;
        assert!(config < 12 && config % 4 == 3, "packet {packets}: TOC config {config}, want SILK at 60 ms");
        packets += 1;
    }
    assert!(packets > 0, "no packets");
    let back = dir.join("silk60.wav");
    let mut j = job(&out, &back);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let frames = pcm(&back).len() / 2;
    assert!(frames >= n && frames < n + 4_800, "silk60: {frames} samples for {n}");

    let out = dir.join("auto.opus");
    let mut j = job(&wav, &out);
    j.audio.codec = Some("opus".into());
    j.audio.opus = Some(OpusJob { mode: Some(OpusMode::Auto), signal: Some(OpusSignal::Voice), ..OpusJob::default() });
    run(&j, &mut |_| {}).unwrap();
    let back = dir.join("auto.wav");
    let mut j = job(&out, &back);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let frames = pcm(&back).len() / 2;
    assert!(frames >= n && frames < n + 4_800, "auto: {frames} samples for {n}");

    let mut j = job(&wav, &dir.join("aac.mka"));
    j.audio.codec = Some("aac".into());
    j.audio.opus = Some(OpusJob { dtx: true, ..OpusJob::default() });
    let err = plan(&j).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("opus") && err.message.contains("aac"), "{}", err.message);

    let mut j = job(&wav, &dir.join("bad.opus"));
    j.audio.codec = Some("opus".into());
    j.audio.opus = Some(OpusJob { frame_ms: Some(3.0), ..OpusJob::default() });
    let err = plan(&j).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("2.5, 5, 10, 20, 40 or 60"), "{}", err.message);

    j.audio.opus = Some(OpusJob { fec_loss_percent: Some(101), ..OpusJob::default() });
    assert_eq!(plan(&j).unwrap_err().code, ErrorCode::InvalidArgument);
}

/// Ogg Opus output: packets copied from a Matroska or Ogg file arrive unchanged and end where the
/// source ended, an encode to `.opus` decodes close to its input, and other codecs are refused.
#[test]
fn ogg_opus_output_copies_and_encodes() {
    let dir = scratch("ogg-out");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for file in ["opus_celt_stereo.opus", "opus_celt_stereo.opus.mka"] {
        let src = if file.ends_with(".mka") {
            let mka = dir.join(file);
            run(&job(&data.join("opus_celt_stereo.opus"), &mka), &mut |_| {}).unwrap();
            mka
        } else {
            data.join(file)
        };
        let out = dir.join(format!("{file}.copy.opus"));
        run(&job(&src, &out), &mut |_| {}).unwrap();
        let mut a = ironav_formats::open_demuxer(&src).unwrap();
        let mut b = ironav_formats::open_demuxer(&out).unwrap();
        assert_eq!(b.format_name(), "ogg");
        assert_eq!(a.streams()[0].extradata[..19], b.streams()[0].extradata[..19], "{file}: OpusHead");
        let (mut n, mut total_a, mut total_b) = (0, 0i64, 0i64);
        while let Some(pa) = a.read_packet().unwrap() {
            let pb = b.read_packet().unwrap().unwrap_or_else(|| panic!("{file}: the copy ends early at packet {n}"));
            assert_eq!(pa.data, pb.data, "{file}: packet {n}");
            total_a += pa.duration;
            total_b += pb.duration;
            n += 1;
        }
        assert!(b.read_packet().unwrap().is_none(), "{file}: the copy has extra packets");
        assert_eq!(total_a, total_b, "{file}: stream length");
    }

    // an encode to .opus: the length is exact, because the last page says where the audio ends
    let n = 48_000 * 2 + 777;
    let samples: Vec<i16> = (0..n).map(|i| (14000.0 * (2.0 * std::f64::consts::PI * 523.0 * i as f64 / 48_000.0).sin()) as i16).collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 1, 48_000);
    let out = dir.join("enc.opus");
    let mut j = job(&wav, &out);
    j.audio.codec = Some("opus".into());
    run(&j, &mut |_| {}).unwrap();
    assert!(std::fs::read(&out).unwrap().starts_with(b"OggS"));
    let back = dir.join("enc.wav");
    let mut j = job(&out, &back);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let got = pcm(&back);
    assert_eq!(got.len() / 2, n, "an exact length");
    assert!(snr_db(&pcm(&wav), &got) > 20.0);

    let e = plan(&{
        let mut j = job(&data.join("mp3_mpeg1_stereo.mp3"), &dir.join("v.opus"));
        j.audio.codec = Some("copy".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
}

/// Ogg FLAC output: a WAV encoded into Ogg comes back bit-exact, its STREAMINFO (patched into the
/// first page when the stream ended) says how long it is, and FLAC frames copy across unchanged.
#[test]
fn ogg_flac_output_is_lossless_and_copies() {
    let dir = scratch("ogg-flac");
    let n = 44_100 * 3 + 123;
    let samples: Vec<i16> = (0..n * 2).map(|i| ((i as f64 * 0.013).sin() * 9000.0 + ((i * 7919) % 211) as f64) as i16).collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 2, 44_100);
    let oga = dir.join("enc.oga");
    let mut j = job(&wav, &oga);
    j.audio.codec = Some("flac".into());
    run(&j, &mut |_| {}).unwrap();
    let bytes = std::fs::read(&oga).unwrap();
    assert!(bytes.starts_with(b"OggS") && bytes[28..].starts_with(b"\x7fFLAC\x01\x00\x00\x01fLaC"));
    let d = ironav_formats::open_demuxer(&oga).unwrap();
    assert_eq!(d.format_name(), "ogg");
    assert_eq!(d.streams()[0].duration, Some(n as i64));
    drop(d);
    let back = dir.join("back.wav");
    let mut j = job(&oga, &back);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    assert_eq!(pcm(&back), pcm(&wav), "lossless through Ogg");

    // Ogg to native FLAC and back is a copy of the same frames
    let flac = dir.join("copy.flac");
    run(&job(&oga, &flac), &mut |_| {}).unwrap();
    let oga2 = dir.join("copy2.oga");
    run(&job(&flac, &oga2), &mut |_| {}).unwrap();
    let (mut a, mut b) = (ironav_formats::open_demuxer(&oga).unwrap(), ironav_formats::open_demuxer(&oga2).unwrap());
    while let Some(pa) = a.read_packet().unwrap() {
        assert_eq!(pa.data, b.read_packet().unwrap().unwrap().data);
    }
    assert!(b.read_packet().unwrap().is_none());
}

/// Vorbis can be copied into Ogg from Ogg or Matroska: the packets and the stream length survive.
#[test]
fn ogg_vorbis_output_copies_packets_and_length() {
    let dir = scratch("ogg-vorbis");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for file in ["vorbis_stereo.mka", "vorbis_5_1.ogg", "vorbis_mono_8k.ogg"] {
        let src = data.join(file);
        let out = dir.join(format!("{file}.copy.ogg"));
        run(&job(&src, &out), &mut |_| {}).unwrap();
        let (mut a, mut b) = (ironav_formats::open_demuxer(&src).unwrap(), ironav_formats::open_demuxer(&out).unwrap());
        assert_eq!(b.format_name(), "ogg");
        assert_eq!(a.streams()[0].extradata, b.streams()[0].extradata, "{file}: headers");
        let (mut n, mut ta, mut tb) = (0, 0i64, 0i64);
        while let Some(pa) = a.read_packet().unwrap() {
            let pb = b.read_packet().unwrap().unwrap_or_else(|| panic!("{file}: ends early at {n}"));
            assert_eq!(pa.data, pb.data, "{file}: packet {n}");
            ta += pa.duration;
            tb += pb.duration;
            n += 1;
        }
        assert!(b.read_packet().unwrap().is_none());
        if file.ends_with(".ogg") {
            assert_eq!(ta, tb, "{file}: an Ogg source is copied to the sample");
        }
        let secs = |t: i64, d: &dyn ironav_core::Demuxer| t as f64 * d.streams()[0].time_base.as_f64();
        assert!((secs(ta, &*a) - secs(tb, &*b)).abs() < 0.03, "{file}: stream length {ta} vs {tb}");
        let (wa, wb) = (dir.join(format!("{file}.a.wav")), dir.join(format!("{file}.b.wav")));
        for (i, o) in [(&src, &wa), (&out, &wb)] {
            let mut j = job(i, o);
            j.audio.codec = Some("pcm_s16le".into());
            run(&j, &mut |_| {}).unwrap();
        }
        assert_eq!(pcm(&wa), pcm(&wb), "{file}: decoded audio");
    }
}

/// Opus from a 44.1 kHz source needs no resample filter from the caller: the plan adds one, says so,
/// and the output is the same as with the filter given by hand.
#[test]
fn opus_from_44k_resamples_by_itself() {
    let dir = scratch("opus-auto");
    let n = 44_100 * 2;
    let samples: Vec<i16> = (0..n).map(|i| (12000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44_100.0).sin()) as i16).collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 1, 44_100);

    let mut auto = job(&wav, &dir.join("auto.opus"));
    auto.audio.codec = Some("opus".into());
    let p = plan(&auto).unwrap();
    assert_eq!(p.steps[0].filters.len(), 1);
    assert_eq!(p.steps[0].filters[0].name, "resample");
    assert!(p.warnings.iter().any(|w| w.contains("44100") && w.contains("48000")), "{:?}", p.warnings);
    run(&auto, &mut |_| {}).unwrap();

    let mut by_hand = job(&wav, &dir.join("hand.opus"));
    by_hand.audio.codec = Some("opus".into());
    by_hand.filters.push(FilterCall::parse_shorthand("resample:rate=48000").unwrap());
    let p = plan(&by_hand).unwrap();
    assert_eq!(p.steps[0].filters.len(), 1, "no second resample is added");
    assert!(p.warnings.is_empty());
    run(&by_hand, &mut |_| {}).unwrap();
    assert_eq!(std::fs::read(dir.join("auto.opus")).unwrap(), std::fs::read(dir.join("hand.opus")).unwrap());
}

/// Vorbis encoding: a WAV becomes Ogg or Matroska Vorbis of exactly its length (Ogg), the bit rate
/// steers the size, and our own decoder gets back something close to the input.
#[test]
fn vorbis_encoder_output_decodes_close_to_its_input() {
    let dir = scratch("vorbis-enc");
    let n = 44_100 * 3 + 321;
    let samples: Vec<i16> = (0..n * 2)
        .map(|i| {
            let (t, c) = ((i / 2) as f64 / 44_100.0, (i % 2) as f64);
            (9000.0 * (2.0 * std::f64::consts::PI * (440.0 + 110.0 * c) * t).sin() + 3000.0 * (2.0 * std::f64::consts::PI * 2500.0 * t).sin()) as i16
        })
        .collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 2, 44_100);
    let mut sizes = Vec::new();
    for (ext, kbps) in [("ogg", 64u32), ("ogg", 192), ("mka", 192)] {
        let out = dir.join(format!("out{kbps}.{ext}"));
        let mut j = job(&wav, &out);
        j.audio.codec = Some("vorbis".into());
        j.audio.bitrate_kbps = Some(kbps);
        run(&j, &mut |_| {}).unwrap();
        let back = dir.join(format!("back{kbps}.{ext}.wav"));
        let mut j = job(&out, &back);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let got = pcm(&back);
        if ext == "ogg" {
            assert_eq!(got.len() / 4, n, "{ext} {kbps}: an exact length");
        } else {
            assert!(got.len() / 4 >= n && got.len() / 4 < n + 2048, "{ext} {kbps}: {} frames", got.len() / 4);
        }
        let snr = snr_db(&pcm(&wav), &got[..pcm(&wav).len()]);
        assert!(snr > if kbps == 64 { 12.0 } else { 20.0 }, "{ext} {kbps}: {snr:.1} dB");
        if ext == "ogg" {
            sizes.push(std::fs::metadata(&out).unwrap().len());
        }
    }
    assert!(sizes[0] < sizes[1] * 2 / 3, "a lower bit rate makes a smaller file: {sizes:?}");

    // more than eight channels are refused with a reason
    let nine = dir.join("nine.wav");
    write_wav(&nine, &vec![0i16; 9 * 4410], 9, 44_100);
    let e = plan(&{
        let mut j = job(&nine, &dir.join("nine.ogg"));
        j.audio.codec = Some("vorbis".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec, "{e:?}");
}

/// The SNR of channel `got_c` of interleaved 16-bit audio against channel `want_c` of a reference
/// with `channels` channels.
fn channel_snr_db(want: &[u8], got: &[u8], channels: usize, want_c: usize, got_c: usize) -> f64 {
    let s = |b: &[u8], c: usize| -> Vec<f64> { b.as_chunks::<2>().0.iter().skip(c).step_by(channels).map(|v| i16::from_le_bytes([v[0], v[1]]) as f64).collect() };
    let (a, b) = (s(want, want_c), s(got, got_c));
    let (sig, err) = a.iter().zip(&b).fold((0.0, 0.0), |(sg, er), (x, y)| (sg + x * x, er + (x - y) * (x - y)));
    10.0 * (sig / err.max(1e-9)).log10()
}

/// Vorbis with 3 to 8 channels: channel `c` carries a tone of its own, and every channel comes back
/// in its own place, in our decoder's output and in FFmpeg's (which takes the WAV channel order:
/// front left, front right, front centre, LFE, ...), in Ogg and in Matroska. Each channel is
/// checked against the others, so a swapped pair fails.
#[test]
fn vorbis_multichannel_keeps_each_tone_in_its_channel() {
    let dir = scratch("vorbis-multi");
    let n = 44_100 * 2 + 123;
    let with_ffmpeg = ffmpeg_available();
    for (channels, ext) in [(3, "ogg"), (4, "ogg"), (5, "webm"), (6, "ogg"), (7, "mka"), (8, "ogg")] {
        let samples: Vec<i16> = (0..n * channels)
            .map(|i| {
                let (t, c) = ((i / channels) as f64 / 44_100.0, (i % channels) as f64);
                (6000.0 * (2.0 * std::f64::consts::PI * (300.0 + 130.0 * c) * t).sin()) as i16
            })
            .collect();
        let wav = dir.join(format!("in{channels}.wav"));
        write_wav(&wav, &samples, channels as u16, 44_100);
        let want = pcm(&wav);
        let out = dir.join(format!("out{channels}.{ext}"));
        let mut j = job(&wav, &out);
        j.audio.codec = Some("vorbis".into());
        run(&j, &mut |_| {}).unwrap();

        let back = dir.join(format!("back{channels}.wav"));
        let mut j = job(&out, &back);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let got = pcm(&back);
        // Ogg keeps the exact length; Matroska and WebM keep the encoder's padding at the end, as the stereo test allows
        let frames = got.len() / (2 * channels);
        if ext == "ogg" {
            assert_eq!(got.len(), want.len(), "{channels} channels: the same length");
        } else {
            assert!(frames >= n && frames < n + 2048, "{channels} channels in {ext}: {frames} frames");
        }
        let mut decoders = vec![("ironav", got)];
        if with_ffmpeg {
            let raw = dir.join(format!("ff{channels}.raw"));
            let st = std::process::Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(&out).args(["-f", "s16le"]).arg(&raw).status().unwrap();
            assert!(st.success(), "ffmpeg could not decode {channels} channels");
            decoders.push(("ffmpeg", std::fs::read(&raw).unwrap()));
        }
        for (who, pcm_out) in &decoders {
            for c in 0..channels {
                // output channel c should be input channel c, and no other input channel
                let own = channel_snr_db(&want, pcm_out, channels, c, c);
                let other = (0..channels).filter(|&d| d != c).map(|d| channel_snr_db(&want, pcm_out, channels, d, c)).fold(f64::MIN, f64::max);
                assert!(own > 12.0, "{who} {channels} channels, channel {c}: {own:.1} dB");
                assert!(own > other + 20.0, "{who} {channels} channels, channel {c}: {own:.1} dB against {other:.1} dB for another input channel");
            }
        }
    }
}

/// AAC encoding into MP4, ADTS and Matroska: the bit rate steers the size, our decoder gets back
/// something close to the input, and the encoder's priming frame is skipped where the container says so
/// (the MP4 edit list, the Matroska codec delay) and left where it does not (ADTS). The lengths are exact.
#[test]
fn aac_encoder_output_decodes_close_to_its_input() {
    let dir = scratch("aac-enc");
    let n: usize = 44_100 * 3 + 321;
    let samples: Vec<i16> = (0..n * 2)
        .map(|i| {
            let (t, c) = ((i / 2) as f64 / 44_100.0, (i % 2) as f64);
            (9000.0 * (2.0 * std::f64::consts::PI * (440.0 + 110.0 * c) * t).sin() + 3000.0 * (2.0 * std::f64::consts::PI * 2500.0 * t).sin()) as i16
        })
        .collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 2, 44_100);
    let want = pcm(&wav);
    let mut sizes = Vec::new();
    for (ext, kbps, lead) in [("m4a", 48u32, 0usize), ("m4a", 160, 0), ("aac", 160, 1024), ("mka", 160, 0)] {
        let out = dir.join(format!("out{kbps}.{ext}"));
        let mut j = job(&wav, &out);
        j.audio.codec = Some("aac".into());
        j.audio.bitrate_kbps = Some(kbps);
        run(&j, &mut |_| {}).unwrap();
        let back = dir.join(format!("back{kbps}.{ext}.wav"));
        let mut j = job(&out, &back);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let got = pcm(&back);
        let frames = got.len() / 4;
        let decoded = if ext == "aac" { (n + 1024).div_ceil(1024) * 1024 } else { n };
        assert_eq!(frames, decoded, "{ext} {kbps}: frames decoded for {n}");
        let snr = snr_db(&want, &got[lead * 4..lead * 4 + want.len()]);
        assert!(snr > if kbps == 48 { 10.0 } else { 18.0 }, "{ext} {kbps}: {snr:.1} dB");
        if ext == "m4a" {
            sizes.push(std::fs::metadata(&out).unwrap().len());
        }
    }
    assert!(sizes[0] < sizes[1] / 2, "a lower bit rate makes a smaller file: {sizes:?}");

    // a rate with no AAC index is refused with the way out
    let odd = dir.join("odd.wav");
    write_wav(&odd, &vec![0i16; 4000], 1, 44_000);
    let e = plan(&{
        let mut j = job(&odd, &dir.join("odd.m4a"));
        j.audio.codec = Some("aac".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec, "{e:?}");
}

/// MP3 output: the file starts with an Info frame whose tag trims the encoder's delay, so decoding it gives back
/// exactly the input's length and something close to the input; rates MPEG-1 lacks are resampled.
#[test]
fn mp3_encoder_output_decodes_close_to_its_input() {
    let dir = scratch("mp3-enc");
    let n = 44_100 * 3 + 321;
    let samples: Vec<i16> = (0..n * 2)
        .map(|i| {
            let (t, c) = ((i / 2) as f64 / 44_100.0, (i % 2) as f64);
            (9000.0 * (2.0 * std::f64::consts::PI * (440.0 + 110.0 * c) * t).sin() + 3000.0 * (2.0 * std::f64::consts::PI * 2500.0 * t).sin()) as i16
        })
        .collect();
    let wav = dir.join("in.wav");
    write_wav(&wav, &samples, 2, 44_100);
    let want = pcm(&wav);
    let mut sizes = Vec::new();
    for (ext, kbps) in [("mp3", 64u32), ("mp3", 192), ("mka", 192)] {
        let out = dir.join(format!("out{kbps}.{ext}"));
        let mut j = job(&wav, &out);
        j.audio.codec = Some("mp3".into());
        j.audio.bitrate_kbps = Some(kbps);
        run(&j, &mut |_| {}).unwrap();
        let back = dir.join(format!("back{kbps}.{ext}.wav"));
        let mut j = job(&out, &back);
        j.audio.codec = Some("pcm_s16le".into());
        run(&j, &mut |_| {}).unwrap();
        let got = pcm(&back);
        // the .mp3 is trimmed by its tag; Matroska keeps the encoder's delay
        let lead = if ext == "mp3" { 0 } else { 1057 };
        let frames = got.len() / 4;
        if ext == "mp3" {
            assert_eq!(frames, n, "{kbps}: {frames} frames for {n}");
        } else {
            assert!(frames >= n + lead && frames < n + lead + 1152, "{ext} {kbps}: {frames} frames for {n}");
        }
        let snr = snr_db(&want, &got[lead * 4..lead * 4 + want.len()]);
        assert!(snr > if kbps == 64 { 10.0 } else { 18.0 }, "{ext} {kbps}: {snr:.1} dB");
        if ext == "mp3" {
            sizes.push(std::fs::metadata(&out).unwrap().len());
        }
    }
    assert!(sizes[0] < sizes[1] / 2, "a lower bit rate makes a smaller file: {sizes:?}");

    // 22.05 kHz is not an MPEG-1 rate: it is resampled, with a warning
    let odd = dir.join("odd.wav");
    write_wav(&odd, &vec![0i16; 8000], 1, 22_050);
    let mut j = job(&odd, &dir.join("odd.mp3"));
    j.audio.codec = Some("mp3".into());
    let p = plan(&j).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("resampled from 22050 Hz to 32000 Hz")), "{:?}", p.warnings);
}

/// A bit rate asked of the codec the stream already has means re-encoding, with a warning saying so.
#[test]
fn a_bitrate_on_the_same_lossy_codec_reencodes() {
    let dir = scratch("reencode");
    let wav = dir.join("in.wav");
    write_wav(&wav, &vec![0i16; 44_100 * 2], 2, 44_100);
    let mp3 = dir.join("a.mp3");
    let mut j = job(&wav, &mp3);
    j.audio.codec = Some("mp3".into());
    run(&j, &mut |_| {}).unwrap();
    let mut j = job(&mp3, &dir.join("b.mp3"));
    j.audio.bitrate_kbps = Some(96);
    let p = plan(&j).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("re-encoded instead of copied")), "{:?}", p.warnings);
    run(&j, &mut |_| {}).unwrap();
    assert!(std::fs::metadata(dir.join("b.mp3")).unwrap().len() < std::fs::metadata(&mp3).unwrap().len());
}

// ---- VP8 -------------------------------------------------------------------------------

fn fnv64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
}

/// Decodes a libvpx-encoded WebM file and compares every frame, bit for bit, with FFmpeg's decode
/// (stored as one FNV-1a hash per frame).
fn vp8_matches_ffmpeg(name: &str) {
    let dir = scratch(name);
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.y4m");
    let mut j = job(&data.join(format!("{name}.webm")), &out);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let want: Vec<u64> = std::fs::read_to_string(data.join(format!("{name}.ffmpeg.fnv"))).unwrap().lines().map(|l| u64::from_str_radix(l, 16).unwrap()).collect();
    let got: Vec<u64> = y4m_frames(&out).iter().map(|f| fnv64(f)).collect();
    assert_eq!(got.len(), want.len(), "frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "frame {i} differs from FFmpeg's decode");
    }
}

/// Intra, inter, golden/altref references, split motion vectors and the six-tap filter.
#[test]
fn vp8_inter_frames_decode_to_what_ffmpeg_decodes() {
    vp8_matches_ffmpeg("vp8_inter");
}

/// Segment-based quantisers and filter levels, and probabilities that are not carried forward.
#[test]
fn vp8_segments_decode_to_what_ffmpeg_decodes() {
    vp8_matches_ffmpeg("vp8_segments");
}

/// Profile 3: bilinear full-pixel chroma, the simple loop filter and several token partitions.
#[test]
fn vp8_bilinear_profile_decodes_to_what_ffmpeg_decodes() {
    vp8_matches_ffmpeg("vp8_bilinear");
}

/// VP8 copied into WebM and Matroska keeps every packet: the remuxed files decode to FFmpeg's frames.
#[test]
fn vp8_copied_into_webm_and_matroska_still_decodes_identically() {
    let dir = scratch("vp8-remux");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let want: Vec<u64> = std::fs::read_to_string(data.join("vp8_inter.ffmpeg.fnv")).unwrap().lines().map(|l| u64::from_str_radix(l, 16).unwrap()).collect();
    for ext in ["webm", "mkv"] {
        let remux = dir.join(format!("copy.{ext}"));
        let mut j = job(&data.join("vp8_inter.webm"), &remux);
        j.hwaccel = "off".into();
        j.video.codec = Some("copy".into());
        run(&j, &mut |_| {}).unwrap();
        let out = dir.join(format!("{ext}.y4m"));
        let mut d = job(&remux, &out);
        d.hwaccel = "off".into();
        d.video.codec = Some("rawvideo".into());
        run(&d, &mut |_| {}).unwrap();
        let got: Vec<u64> = y4m_frames(&out).iter().map(|f| fnv64(f)).collect();
        assert_eq!(got, want, "{ext}");
    }
}

/// Whether FFmpeg is on PATH. The FFmpeg checks below skip (and say so) when it is not.
fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Planar 4:2:0 frames of a moving pattern: a gradient, a bar sliding right and a dark block sliding
/// left, with a little noise.
fn moving_pattern(w: usize, h: usize, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|t| {
            let mut seed = 7u32 + t as u32 * 1000;
            let mut noise = move || {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((seed >> 16) % 7) as i32 - 3
            };
            let mut f = Vec::new();
            for r in 0..h {
                for c in 0..w {
                    let mut v = (c * 160 / w + r * 60 / h) as i32 + 20;
                    if (c + 3 * t) % w < w / 5 && r > h / 5 && r < h / 2 {
                        v = 220 - ((c + r) % 7) as i32 * 10;
                    }
                    let dark = (c + w - (2 * t) % w) % w;
                    if dark > w / 2 && dark < w / 2 + w / 6 && r > h / 2 {
                        v = 30 + ((r * 5) % 40) as i32;
                    }
                    f.push((v + noise()).clamp(0, 255) as u8);
                }
            }
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            f.extend((0..cw * ch).map(|i| (100 + (i % cw) * 50 / cw + t) as u8));
            f.extend((0..cw * ch).map(|i| (150 - (i / cw) * 40 / ch) as u8));
            f
        })
        .collect()
}

fn write_y4m_frames(path: &Path, w: usize, h: usize, frames: &[Vec<u8>]) {
    let mut f = format!("YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420jpeg\n").into_bytes();
    for fr in frames {
        f.extend_from_slice(b"FRAME\n");
        f.extend_from_slice(fr);
    }
    std::fs::write(path, f).unwrap();
}

/// PSNR of the luma planes of two frames of `w` x `h` (the first `w * h` bytes of each).
fn luma_psnr(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    let mse = a[..w * h].iter().zip(&b[..w * h]).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum::<f64>() / (w * h) as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

/// Encodes `src` to `out` with the VP9 encoder at `qp` (key frames, then inter frames) and returns the size of the file.
fn vp9_encode(src: &Path, out: &Path, qp: u8) -> u64 {
    let mut j = job(src, out);
    j.hwaccel = "off".into();
    j.video.codec = Some("vp9".into());
    j.video.qp = Some(qp);
    run(&j, &mut |_| {}).unwrap();
    std::fs::metadata(out).unwrap().len()
}

/// Inter frames make the VP9 stream smaller than the same frames coded alone (all key frames).
#[test]
fn vp9_inter_frames_make_the_stream_smaller_than_key_frames_alone() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("vp9-inter-size");
    let (w, h, n) = (176, 144, 10);
    let frames = moving_pattern(w, h, n);
    let all = dir.join("all.y4m");
    write_y4m_frames(&all, w, h, &frames);
    let together = vp9_encode(&all, &dir.join("all.webm"), 30);
    let mut alone = 0;
    for (t, fr) in frames.iter().enumerate() {
        let one = dir.join(format!("one{t}.y4m"));
        write_y4m_frames(&one, w, h, std::slice::from_ref(fr));
        alone += vp9_encode(&one, &dir.join(format!("one{t}.webm")), 30);
    }
    assert!(together * 10 < alone * 8, "{together} bytes with inter frames, {alone} as key frames only");
}

/// VP9 written by IronAV, in WebM, Matroska and MP4, decodes to the same pictures in IronAV and in
/// FFmpeg, and those pictures are close to the source (odd dimensions included).
#[test]
fn vp9_encoded_files_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("vp9-enc");
    let (w, h, n) = (177, 99, 12);
    let frames = moving_pattern(w, h, n);
    let src = dir.join("src.y4m");
    write_y4m_frames(&src, w, h, &frames);
    for ext in ["webm", "mkv", "mp4"] {
        let enc = dir.join(format!("out.{ext}"));
        vp9_encode(&src, &enc, 28);
        let ours = dir.join(format!("ours-{ext}.y4m"));
        let mut d = job(&enc, &ours);
        d.hwaccel = "off".into();
        d.video.codec = Some("rawvideo".into());
        run(&d, &mut |_| {}).unwrap();
        let theirs = dir.join(format!("ffmpeg-{ext}.yuv"));
        let st = std::process::Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(&enc).args(["-f", "rawvideo", "-pix_fmt", "yuv420p"]).arg(&theirs).status().unwrap();
        assert!(st.success(), "ffmpeg could not decode {ext}");
        let got = y4m_frames_any(&ours);
        assert_eq!(got.len(), n, "{ext}: frame count");
        assert!(got.concat() == std::fs::read(&theirs).unwrap(), "{ext}: IronAV's decode differs from FFmpeg's");
        for (t, (g, s)) in got.iter().zip(&frames).enumerate() {
            let p = luma_psnr(g, s, w, h);
            assert!(p > 30.0, "{ext} frame {t}: PSNR {p:.1} dB");
        }
    }
}

/// Encodes `src` to `out` with the VP8 encoder at `qp` and returns the size of the file.
fn vp8_encode(src: &Path, out: &Path, qp: u8) -> u64 {
    let mut j = job(src, out);
    j.hwaccel = "off".into();
    j.video.codec = Some("vp8".into());
    j.video.qp = Some(qp);
    run(&j, &mut |_| {}).unwrap();
    std::fs::metadata(out).unwrap().len()
}

/// VP8 written by IronAV, in WebM and Matroska, decodes to the same pictures in IronAV and in FFmpeg,
/// and those pictures are close to the source (odd dimensions included).
#[test]
fn vp8_encoded_files_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("vp8-enc");
    let (w, h, n) = (177, 99, 12);
    let frames = moving_pattern(w, h, n);
    let src = dir.join("src.y4m");
    write_y4m_frames(&src, w, h, &frames);
    for ext in ["webm", "mkv"] {
        let enc = dir.join(format!("out.{ext}"));
        vp8_encode(&src, &enc, 30);
        let ours = dir.join(format!("ours-{ext}.y4m"));
        let mut d = job(&enc, &ours);
        d.hwaccel = "off".into();
        d.video.codec = Some("rawvideo".into());
        run(&d, &mut |_| {}).unwrap();
        let theirs = dir.join(format!("ffmpeg-{ext}.yuv"));
        let st = std::process::Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(&enc).args(["-f", "rawvideo", "-pix_fmt", "yuv420p"]).arg(&theirs).status().unwrap();
        assert!(st.success(), "ffmpeg could not decode {ext}");
        let got = y4m_frames_any(&ours);
        assert_eq!(got.len(), n, "{ext}: frame count");
        assert!(got.concat() == std::fs::read(&theirs).unwrap(), "{ext}: IronAV's decode differs from FFmpeg's");
        for (t, (g, s)) in got.iter().zip(&frames).enumerate() {
            let p = luma_psnr(g, s, w, h);
            assert!(p > 30.0, "{ext} frame {t}: PSNR {p:.1} dB");
        }
    }
}

/// Inter frames are what make the stream small: the same frames each coded alone (all key frames)
/// take more bytes than the stream does.
#[test]
fn vp8_inter_frames_make_the_stream_smaller_than_key_frames_alone() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("vp8-inter-size");
    let (w, h, n) = (176, 144, 10);
    let frames = moving_pattern(w, h, n);
    let all = dir.join("all.y4m");
    write_y4m_frames(&all, w, h, &frames);
    let together = vp8_encode(&all, &dir.join("all.webm"), 30);
    let mut alone = 0;
    for (t, fr) in frames.iter().enumerate() {
        let one = dir.join(format!("one{t}.y4m"));
        write_y4m_frames(&one, w, h, std::slice::from_ref(fr));
        alone += vp8_encode(&one, &dir.join(format!("one{t}.webm")), 30);
    }
    assert!(together * 10 < alone * 8, "{together} bytes with inter frames, {alone} as key frames only");
}

#[test]
fn qp_sets_the_jpeg_quality() {
    let dir = scratch("jpeg-qp");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut sizes = Vec::new();
    for qp in [2u8, 23, 45] {
        let out = dir.join(format!("q{qp}.jpg"));
        let mut j = job(&data.join("jpeg_420.jpg"), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("mjpeg".into());
        j.video.qp = Some(qp);
        run(&j, &mut |_| {}).unwrap();
        sizes.push(std::fs::metadata(&out).unwrap().len());
    }
    assert!(sizes[0] > sizes[1] && sizes[1] > sizes[2], "{sizes:?}");
}

#[test]
fn mjpeg_in_matroska_roundtrips() {
    let dir = scratch("mjpeg-mkv");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let src = dir.join("src.y4m");
    let mut j = job(&data.join("vp8_inter.webm"), &src);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let mkv = dir.join("clip.mkv");
    let mut e = job(&src, &mkv);
    e.hwaccel = "off".into();
    e.video.codec = Some("mjpeg".into());
    run(&e, &mut |_| {}).unwrap();
    let back = dir.join("back.y4m");
    let mut d = job(&mkv, &back);
    d.hwaccel = "off".into();
    d.video.codec = Some("rawvideo".into());
    run(&d, &mut |_| {}).unwrap();
    let (a, b) = (y4m_frames_any(&src), y4m_frames_any(&back));
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), 12);
    for (x, y) in a.iter().zip(&b) {
        let mse = x.iter().zip(y).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum::<f64>() / x.len() as f64;
        assert!(10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10() > 32.0, "frame too far from the source");
    }
}

// ---- H.264 -------------------------------------------------------------------------------

/// Software H.264 decoding of intra pictures is bit-exact with FFmpeg's decoder (libx264 streams
/// with CAVLC and CABAC, 8x8 transform, slices, scaling matrices, cropping, and hand-built I_PCM).
#[test]
fn h264_intra_decodes_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["i_cavlc", "i_cabac", "i_high8", "i_high8_cavlc", "i_slices", "i_cqm", "i_crop", "pcm_cavlc", "pcm_cabac"] {
        let dir = scratch(&format!("h264-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("h264_{name}.mp4")), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("h264_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// CAVLC (p_*) and CABAC (c_*) P slices and B slices (b_*: spatial and temporal direct, pyramids, implicit weights) (every partition size, several reference frames, weighted prediction, slices,
/// the 8x8 transform, odd sizes) are bit-exact with FFmpeg's decoder.
#[test]
fn h264_p_slices_decode_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["p_ref4", "p_parts", "p_8x8dct", "p_weight2", "p_slices", "p_odd", "p_dbp", "c_ref4", "c_parts", "c_8x8dct", "c_weight", "c_slices", "c_odd", "b_sp_cavlc", "b_sp_cabac", "b_tm_cavlc", "b_tm_cabac", "b_pyr", "b_wimp", "b_odd", "b_slices", "ci_pc", "ci_b", "ci_bn"] {
        let dir = scratch(&format!("h264-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("h264_{name}.mp4")), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("h264_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// High 10 (10-bit 4:2:0) streams, intra, P and B, CAVLC and CABAC, negative QPs and weighted
/// prediction, are bit-exact with FFmpeg's decoder. The GPU cannot decode them, so even with
/// hwaccel "auto" the software decoder takes over.
#[test]
fn h264_high10_decodes_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (name, hw) in [("h10_i_cabac", "off"), ("h10_i_cavlc", "off"), ("h10_p", "off"), ("h10_b", "off"), ("h10_neg", "off"), ("h10_wp", "off"), ("h10_noise", "off"), ("h10_b", "auto")] {
        let dir = scratch(&format!("h264-{name}-{hw}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("h264_{name}.mp4")), &out);
        j.hwaccel = hw.into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("h264_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// 8-bit 4:4:4, 4:2:2 and monochrome H.264, including lossless (transform bypass) streams (I, P and B slices, CAVLC and CABAC) are bit-exact with
/// FFmpeg's decoder; monochrome comes out as gray, and neither can go to the GPU.
#[test]
fn h264_444_422_and_monochrome_decode_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (name, hw) in [("y422_i", "off"), ("y422_p", "off"), ("y422_b", "off"), ("y422_cavlc", "off"), ("y422_b", "auto"), ("y400_i", "off"), ("y400_p", "off"), ("y400_b", "off"), ("y400_b", "auto"), ("y444_i_cavlc", "off"), ("y444_i_cabac", "off"), ("y444_p", "off"), ("y444_b", "off"), ("y444_b_cavlc", "off"), ("y444_b", "auto"), ("lossless_444_i", "off"), ("lossless_444_b", "off"), ("lossless_420_i", "off"), ("lossless_422_p", "off"), ("y422p10_i_cavlc", "off"), ("y422p10_p", "off"), ("y422p10_b", "off"), ("y422p10_b", "auto"), ("y444p10_i", "off"), ("y444p10_b", "off"), ("lossless_444p10_b", "off")] {
        let dir = scratch(&format!("h264-{name}-{hw}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("h264_{name}.mp4")), &out);
        j.hwaccel = hw.into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("h264_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// Interlaced H.264 coded as MBAFF frames (field and frame macroblock pairs) from libx264: CABAC with
/// spatial direct and explicit weights over a fade, CAVLC with temporal direct and the bottom field
/// first, and three slices without the 8x8 transform, are bit-exact with FFmpeg's decoder. Larger
/// streams and more tools are checked in ironav-codec's h264_mbaff tests.
#[test]
fn h264_mbaff_decodes_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["mbaff_b_cabac", "mbaff_tm_cavlc", "mbaff_slices"] {
        let dir = scratch(&format!("h264-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("h264_{name}.mp4")), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("h264_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// HEVC Main, Main 10 and range extension streams from x265 (intra, P and B pictures, weighted prediction,
/// wavefronts, multiple slices, conformance-window cropping, monochrome, 4:2:2 and 4:4:4 at 8 and 10 bits,
/// and 12-bit 4:2:0 and 4:2:2)
/// are bit-exact with FFmpeg's decoder.
#[test]
fn hevc_streams_decode_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["intra", "p", "b", "wpp_slices", "p10", "gray", "crop", "422", "444", "422_10", "444_10", "420_12", "422_12"] {
        let dir = scratch(&format!("hevc-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("hevc_{name}.mp4")), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("hevc_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// An open-GOP stream joined at a CRA picture, as a live viewer joins it: the RASL pictures that lean on
/// pictures before the join are dropped, and the rest are bit-exact with FFmpeg's decoder (which, like
/// NVDEC, gives the same pictures the whole stream decodes to from there on).
#[test]
fn hevc_joined_at_a_cra_picture_drops_its_rasl_pictures() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let dir = scratch("hevc-cra-start");
    let out = dir.join("out.y4m");
    let mut j = job(&data.join("hevc_cra_start.mkv"), &out);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let got: Vec<u8> = y4m_frames_any(&out).concat();
    let want = std::fs::read(data.join("hevc_cra_start.ffmpeg.yuv")).unwrap();
    assert_eq!(got.len(), want.len(), "16 pictures of the 19 after the join");
    assert!(got == want, "pixels differ from FFmpeg's");
}

/// SAO on 16x16 coding tree blocks with 4:2:0 chroma, after deblocking, is bit-exact with a hardware
/// decoder (NVDEC). FFmpeg 9.0.1's software decoder differs here by one chroma sample: with 8x8 chroma
/// blocks it applies SAO before the neighbouring block's chroma edges are deblocked (fixed in its
/// development tree for some edge classes), so its output is not the reference for this stream.
#[test]
fn hevc_sao_on_16x16_blocks_decodes_to_what_hardware_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let dir = scratch("hevc-ctu16-sao");
    let out = dir.join("out.y4m");
    let mut j = job(&data.join("hevc_ctu16_sao.mp4"), &out);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let got: Vec<u8> = y4m_frames_any(&out).concat();
    let want = std::fs::read(data.join("hevc_ctu16_sao.nvdec.yuv")).unwrap();
    assert_eq!(got.len(), want.len(), "size");
    assert!(got == want, "pixels differ from NVDEC's");
}

/// VP9 streams from libvpx (key and inter frames with compound prediction, segmentation, tiles,
/// 4:4:4, lossless, odd sizes) are bit-exact with FFmpeg's decoder.
#[test]
fn vp9_streams_decode_to_what_ffmpeg_decodes() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["p", "aq", "tiles", "c444", "lossless", "odd"] {
        let dir = scratch(&format!("vp9-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("vp9_{name}.webm")), &out);
        j.hwaccel = "off".into();
        j.video.codec = Some("rawvideo".into());
        run(&j, &mut |_| {}).unwrap();
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("vp9_{name}.ffmpeg.yuv"))).unwrap();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from FFmpeg's");
    }
}

/// The packets of a file's only video stream: the time each is shown, in seconds, and its payload.
fn video_packets(path: &std::path::Path) -> (ironav_core::StreamInfo, Vec<(f64, Vec<u8>)>) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let s = d.streams()[0].clone();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        packets.push(((p.pts - s.lead_in) as f64 * s.time_base.as_f64(), p.data));
    }
    (s, packets)
}

/// VP9 and AV1 copied from WebM into MP4 and from there into Matroska or WebM keep every packet and
/// its time, and the VP9 copies in MP4 still decode to what FFmpeg decodes.
#[test]
fn vp9_and_av1_copy_between_webm_matroska_and_mp4() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let dir = scratch("vpx-copy");
    let files = ["vp9_p.webm", "vp9_aq.webm", "vp9_tiles.webm", "vp9_c444.webm", "vp9_lossless.webm", "vp9_odd.webm", "av1_p.webm", "av1_odd.webm", "av1_h10.mp4"];
    for file in files {
        let src = data.join(file);
        let (mid, last) = if file.ends_with(".mp4") { (dir.join(format!("{file}.webm")), dir.join(format!("{file}.mp4"))) } else { (dir.join(format!("{file}.mp4")), dir.join(format!("{file}.mkv"))) };
        run(&job(&src, &mid), &mut |_| {}).unwrap();
        run(&job(&mid, &last), &mut |_| {}).unwrap();
        let (s0, p0) = video_packets(&src);
        for out in [&mid, &last] {
            let (s1, p1) = video_packets(out);
            assert_eq!((s1.codec, &s1.params), (s0.codec, &s0.params), "{}", out.display());
            if s0.codec == ironav_core::CodecId::Av1 {
                assert_eq!(s1.extradata, s0.extradata, "{}: av1C", out.display());
            }
            assert_eq!(p1.len(), p0.len(), "{}: packets", out.display());
            for (i, (a, b)) in p0.iter().zip(&p1).enumerate() {
                assert!(a.1 == b.1, "{}: packet {i} payload", out.display());
                assert!((a.0 - b.0).abs() < 0.0015, "{}: packet {i} at {} s, not {} s", out.display(), b.0, a.0);
            }
        }
        if let Some(name) = file.strip_prefix("vp9_").and_then(|f| f.strip_suffix(".webm")) {
            let mp4 = std::fs::read(&mid).unwrap();
            assert!(has(&mp4, b"vp09") && has(&mp4, b"vpcC"), "{file}: VP9 sample entry");
            let out = dir.join(format!("{name}.y4m"));
            let mut j = job(&mid, &out);
            j.hwaccel = "off".into();
            j.video.codec = Some("rawvideo".into());
            run(&j, &mut |_| {}).unwrap();
            let want = std::fs::read(data.join(format!("vp9_{name}.ffmpeg.yuv"))).unwrap();
            assert!(y4m_frames_any(&out).concat() == want, "{file}: the copy in MP4 decodes differently");
        }
    }
    // H.264 has no place in WebM
    let e = run(&job(&data.join("bframes_aac.mkv"), &dir.join("x.webm")), &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
    assert!(e.alternatives.iter().any(|a| a == "vp9") && e.alternatives.iter().any(|a| a == "av1"));
}

// ---- AV1 -------------------------------------------------------------------------------

/// AV1 is decoded on the GPU when there is one, else in software (tests/av1.rs), which decodes
/// key, intra-only and inter frames, intra block copy and superres included.
/// Either way the pictures
/// must equal libdav1d's (AV1 decoding is exact).
#[test]
fn av1_decodes_on_the_gpu_to_what_dav1d_decodes_or_refuses_precisely() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (name, ext) in [("p", "webm"), ("h10", "mp4"), ("odd", "webm")] {
        let dir = scratch(&format!("av1-{name}"));
        let out = dir.join("out.y4m");
        let mut j = job(&data.join(format!("av1_{name}.{ext}")), &out);
        j.video.codec = Some("rawvideo".into());

        let mut off = j.clone();
        off.hwaccel = "off".into();
        assert!(plan(&off).is_ok(), "{name}: AV1 is planned for the software decoder");
        let want = std::fs::read(data.join(format!("av1_{name}.dav1d.yuv"))).unwrap();
        match run(&off, &mut |_| {}) {
            Ok(_) => {
                assert!(y4m_frames_any(&out).concat() == want, "{name}: software pixels differ from libdav1d's");
                std::fs::remove_file(&out).unwrap();
            }
            Err(e) => {
                assert_eq!(e.code, ErrorCode::UnsupportedFormat, "{name}: {}", e.message);
                assert!(e.message.contains("not decoded yet") && !e.message.contains("inter frames"), "{name}: {}", e.message);
            }
        }

        let report = match run(&j, &mut |_| {}) {
            Ok(r) => r,
            Err(e) => {
                assert!(e.message.contains("not decoded yet"), "{name}: {}", e.message);
                continue;
            }
        };
        if report.steps[0].hardware_decoder != Some("cuda") {
            continue; // no GPU: decoded in software, as above
        }
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        assert_eq!(got.len(), want.len(), "{name}: size");
        assert!(got == want, "{name}: pixels differ from libdav1d's");
    }
}

// ---- JPEG ------------------------------------------------------------------------------

/// Raw planes of the first frame of a y4m file.
fn y4m_first_frame(path: &std::path::Path) -> Vec<u8> {
    y4m_frames_any(path).remove(0)
}

fn y4m_frames_any(path: &std::path::Path) -> Vec<Vec<u8>> {
    let d = std::fs::read(path).unwrap();
    let hdr = d.iter().position(|&b| b == b'\n').unwrap() + 1;
    let head = String::from_utf8_lossy(&d[..hdr]).to_string();
    let get = |p: char| head.split_whitespace().find(|t| t.starts_with(p)).unwrap()[1..].parse::<usize>().unwrap();
    let (w, h) = (get('W'), get('H'));
    let size = if head.contains("C420p10") || head.contains("C420p12") {
        2 * (w * h + 2 * w.div_ceil(2) * h.div_ceil(2))
    } else if head.contains("C422p10") || head.contains("C422p12") {
        2 * (w * h + 2 * w.div_ceil(2) * h)
    } else if head.contains("C444p10") || head.contains("C444p12") {
        6 * w * h
    } else if head.contains("C422") {
        w * h + 2 * w.div_ceil(2) * h
    } else if head.contains("C444") {
        3 * w * h
    } else if head.contains("Cmono") {
        w * h
    } else {
        w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
    };
    let mut frames = Vec::new();
    let mut pos = hdr;
    while pos < d.len() {
        let nl = pos + d[pos..].iter().position(|&b| b == b'\n').unwrap() + 1;
        frames.push(d[nl..nl + size].to_vec());
        pos = nl + size;
    }
    frames
}

/// A baseline JPEG decodes to what FFmpeg's decoder gives, to within one level (the two use
/// different integer inverse DCTs).
fn jpeg_matches_ffmpeg(name: &str) {
    let dir = scratch(name);
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.y4m");
    let mut j = job(&data.join(format!("{name}.jpg")), &out);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let got = y4m_first_frame(&out);
    let want = std::fs::read(data.join(format!("{name}.ffmpeg.yuv"))).unwrap();
    assert_eq!(got.len(), want.len());
    let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert!(worst <= 1, "largest difference from FFmpeg is {worst}");
    assert!(differing * 20 < got.len(), "{differing} of {} samples differ", got.len());
}

#[test]
fn jpeg_420_decodes_to_what_ffmpeg_decodes() {
    jpeg_matches_ffmpeg("jpeg_420");
}

#[test]
fn jpeg_422_and_444_decode_to_what_ffmpeg_decodes() {
    jpeg_matches_ffmpeg("jpeg_422");
    jpeg_matches_ffmpeg("jpeg_444");
}

/// Progressive files (spectral selection and successive approximation, written by libjpeg).
#[test]
fn progressive_jpeg_decodes_to_what_ffmpeg_decodes() {
    for n in ["jpeg_prog_p420", "jpeg_prog_p422", "jpeg_prog_p444", "jpeg_prog_pgray"] {
        jpeg_matches_ffmpeg(n);
    }
}

/// Decoding to raw frames and encoding again gives a JPEG that decodes close to the original.
#[test]
fn jpeg_reencode_stays_close() {
    let dir = scratch("jpeg-enc");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let raw = dir.join("raw.y4m");
    let mut j = job(&data.join("jpeg_420.jpg"), &raw);
    j.hwaccel = "off".into();
    j.video.codec = Some("rawvideo".into());
    run(&j, &mut |_| {}).unwrap();
    let again = dir.join("again.jpg");
    run(&job(&raw, &again), &mut |_| {}).unwrap();
    let back = dir.join("back.y4m");
    let mut k = job(&again, &back);
    k.video.codec = Some("rawvideo".into());
    run(&k, &mut |_| {}).unwrap();
    let (a, b) = (y4m_first_frame(&raw), y4m_first_frame(&back));
    let mse = a.iter().zip(&b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
    assert!(psnr > 35.0, "PSNR {psnr}");
}

/// The first picture of a JPEG, read and decoded the way the engine reads it.
fn jpeg_frame(path: &std::path::Path) -> ironav_core::VideoFrame {
    use ironav_core::Frame;
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let info = d.streams()[0].clone();
    let mut dec = ironav_codec::new_decoder_for(&info).unwrap();
    let p = d.read_packet().unwrap().unwrap();
    match dec.decode(&p).unwrap().remove(0) {
        Frame::Video(v) => v,
        _ => panic!("the JPEG did not decode to a picture"),
    }
}

/// A lossless JPEG decodes to exactly what FFmpeg decodes: a lossless file has no tolerance.
/// RGB references are FFmpeg's rgb24 output (`bgr24` files are decorrelated; FFmpeg's own decode
/// of them is the source picture).
fn lossless_matches_ffmpeg(name: &str, rgb: bool) {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let got = jpeg_frame(&data.join(format!("{name}.jpg"))).planes.concat();
    let want = std::fs::read(data.join(format!("{name}.ffmpeg.{}", if rgb { "rgb" } else { "yuv" }))).unwrap();
    assert_eq!(got.len(), want.len(), "{name}");
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "{name}: {differing} samples differ from FFmpeg's");
}

/// Lossless JPEGs (SOF3) from FFmpeg's encoder: predictors 1 to 3 on 4:2:0, 4:2:2 and 4:4:4, and
/// RGB (9-bit, with FFmpeg's colour transform).
#[test]
fn lossless_jpeg_decodes_to_what_ffmpeg_decodes() {
    for name in ["jpeg_lossless_420_p1", "jpeg_lossless_420_p3", "jpeg_lossless_422_p2", "jpeg_lossless_444_p2"] {
        lossless_matches_ffmpeg(name, false);
    }
    for name in ["jpeg_lossless_rgb_p1", "jpeg_lossless_rgb_p3"] {
        lossless_matches_ffmpeg(name, true);
    }
}

/// A lossless JPEG written by a small test encoder that follows T.81 H.1.2 on its own. FFmpeg's
/// encoder writes predictors 1 to 3 only, so this covers 4 to 7 and the point transform. The
/// samples of `planes[c]` cover whole MCUs (`mcux * h` samples a row, `mcuy * v` rows).
fn lossless_jpeg(w: usize, h: usize, sampling: &[(usize, usize)], planes: &[Vec<u8>], mcux: usize, mcuy: usize, pred: u8, pt: u32) -> Vec<u8> {
    struct BitOut {
        bytes: Vec<u8>,
        acc: u32,
        n: u32,
    }
    impl BitOut {
        fn put(&mut self, v: u32, len: u32) {
            for i in (0..len).rev() {
                self.acc = (self.acc << 1) | ((v >> i) & 1);
                self.n += 1;
                if self.n == 8 {
                    let b = self.acc as u8;
                    self.bytes.push(b);
                    if b == 0xFF {
                        self.bytes.push(0);
                    }
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
    }
    let n = sampling.len();
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xC3];
    out.extend(((8 + 3 * n) as u16).to_be_bytes());
    out.push(8);
    out.extend((h as u16).to_be_bytes());
    out.extend((w as u16).to_be_bytes());
    out.push(n as u8);
    for (c, &(hs, vs)) in sampling.iter().enumerate() {
        out.extend([c as u8 + 1, ((hs << 4) | vs) as u8, 0]);
    }
    // one DC table: nine codes of length four, for the categories 0 to 8
    let mut counts = [0u8; 16];
    counts[3] = 9;
    out.extend([0xFF, 0xC4]);
    out.extend((28u16).to_be_bytes());
    out.push(0x00);
    out.extend(counts);
    out.extend(0..9u8);
    out.extend([0xFF, 0xDA]);
    out.extend((6 + 2 * n as u16).to_be_bytes());
    out.push(n as u8);
    for c in 0..n {
        out.extend([c as u8 + 1, 0]);
    }
    out.extend([pred, 0, pt as u8]);

    let mut bw = BitOut { bytes: Vec::new(), acc: 0, n: 0 };
    let stride: Vec<usize> = sampling.iter().map(|&(hs, _)| mcux * hs).collect();
    let mut recon: Vec<Vec<i32>> = planes.iter().map(|p| vec![0; p.len()]).collect();
    for my in 0..mcuy {
        for mx in 0..mcux {
            for (c, &(hs, vs)) in sampling.iter().enumerate() {
                for v in 0..vs {
                    for hh in 0..hs {
                        let (x, y) = (mx * hs + hh, my * vs + v);
                        let (s, i) = (stride[c], y * stride[c] + x);
                        let rec = &mut recon[c];
                        let p = if x == 0 && y == 0 {
                            128
                        } else if y == 0 {
                            rec[i - 1]
                        } else if x == 0 {
                            rec[i - s]
                        } else {
                            let (ra, rb, rc) = (rec[i - 1], rec[i - s], rec[i - s - 1]);
                            match pred {
                                1 => ra,
                                2 => rb,
                                3 => rc,
                                4 => ra + rb - rc,
                                5 => ra + ((rb - rc) >> 1),
                                6 => rb + ((ra - rc) >> 1),
                                _ => (ra + rb) >> 1,
                            }
                        };
                        // the prediction is taken on the reduced samples, so its low `pt` bits are dropped
                        let p = p & !((1 << pt) - 1);
                        let xrec = ((planes[c][i] as i32) >> pt) << pt;
                        let d = (xrec - p).rem_euclid(256);
                        let d = if d >= 128 { d - 256 } else { d };
                        let coded = d >> pt;
                        let t = 32 - coded.unsigned_abs().leading_zeros();
                        bw.put(t, 4);
                        if t > 0 {
                            bw.put((if coded < 0 { coded + (1 << t) - 1 } else { coded }) as u32, t);
                        }
                        rec[i] = xrec;
                    }
                }
            }
        }
    }
    while bw.n != 0 {
        bw.put(1, 1);
    }
    out.extend(bw.bytes);
    out.extend([0xFF, 0xD9]);
    out
}

/// Deterministic test samples: a gradient with noise, so that the differences use every category.
fn lossless_sample(c: usize, x: usize, y: usize) -> u8 {
    let mut h = (c as u32).wrapping_mul(0x9E37_79B1) ^ (x as u32).wrapping_mul(0x85EB_CA77) ^ (y as u32).wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    ((3 * x + 2 * y + 40 * c) as u8).wrapping_add((h >> 24) as u8)
}

/// Every predictor and point transform, on gray, RGB and 4:2:0 pictures of sizes that leave
/// padding in the last MCUs, decodes to the samples the test encoder started from.
#[test]
fn lossless_jpeg_every_predictor_and_point_transform_is_exact() {
    let dir = scratch("jpeg-lossless-pred");
    let formats: [(&str, usize, usize, &[(usize, usize)]); 3] = [("gray", 33, 17, &[(1, 1)]), ("rgb", 17, 9, &[(1, 1), (1, 1), (1, 1)]), ("yuv420", 37, 21, &[(2, 2), (1, 1), (1, 1)])];
    for (name, w, h, sampling) in formats {
        let hmax = sampling.iter().map(|s| s.0).max().unwrap();
        let vmax = sampling.iter().map(|s| s.1).max().unwrap();
        let (mcux, mcuy) = (w.div_ceil(hmax), h.div_ceil(vmax));
        let planes: Vec<Vec<u8>> = sampling
            .iter()
            .enumerate()
            .map(|(c, &(hs, vs))| {
                let pw = mcux * hs;
                (0..pw * mcuy * vs).map(|i| lossless_sample(c, i % pw, i / pw)).collect()
            })
            .collect();
        for pred in 1..=7u8 {
            for pt in [0u32, 2] {
                let path = dir.join(format!("{name}_p{pred}_t{pt}.jpg"));
                std::fs::write(&path, lossless_jpeg(w, h, sampling, &planes, mcux, mcuy, pred, pt)).unwrap();
                let got = jpeg_frame(&path);
                assert_eq!((got.width as usize, got.height as usize), (w, h), "{name}");
                // the point transform drops the low bits of every sample; the rest is exact
                let mask = !((1u8 << pt) - 1);
                let flat: Vec<Vec<u8>> = sampling
                    .iter()
                    .enumerate()
                    .map(|(c, &(hs, vs))| {
                        let (cw, ch, pw) = ((w * hs).div_ceil(hmax), (h * vs).div_ceil(vmax), mcux * hs);
                        (0..ch).flat_map(|y| (0..cw).map(move |x| (y, x))).map(|(y, x)| planes[c][y * pw + x] & mask).collect()
                    })
                    .collect();
                let want: Vec<u8> = if name == "rgb" { (0..w * h).flat_map(|i| flat.iter().map(move |p| p[i])).collect() } else { flat.concat() };
                let differing = got.planes.concat().iter().zip(&want).filter(|(a, b)| a != b).count();
                assert_eq!(differing, 0, "{name}, predictor {pred}, point transform {pt}: samples differ");
            }
        }
    }
}

/// Arithmetic-coded (SOF9) and 12-bit JPEGs are refused by name, not read as garbage.
#[test]
fn jpeg_arithmetic_and_12_bit_are_refused_by_name() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let base = std::fs::read(data.join("jpeg_420.jpg")).unwrap();
    let sof = base.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    let dir = scratch("jpeg-refused");
    let mut arith = base.clone();
    arith[sof + 1] = 0xC9;
    std::fs::write(dir.join("arith.jpg"), &arith).unwrap();
    let mut twelve = base.clone();
    twelve[sof + 4] = 12;
    std::fs::write(dir.join("twelve.jpg"), &twelve).unwrap();
    for (file, words) in [("arith.jpg", "arithmetic"), ("twelve.jpg", "precision")] {
        let err = ironav_formats::open_demuxer(&dir.join(file)).and_then(|mut d| {
            let info = d.streams()[0].clone();
            let mut dec = ironav_codec::new_decoder_for(&info)?;
            let p = d.read_packet()?.unwrap();
            dec.decode(&p)
        });
        let err = err.unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedFormat, "{file}");
        assert!(err.to_string().contains(words), "{file}: {err}");
    }
}

#[test]
fn low_latency_re_encodes_without_reordering() {
    let dir = scratch("low-latency");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ironav-formats/tests/data/h264_aac.ts");
    let out = dir.join("live.mp4");
    let mut j = job(&src, &out);
    j.hwaccel = "off".into();
    // the source is H.264 with B pictures; naming h264 with low latency re-encodes it anyway
    j.video.codec = Some("h264".into());
    j.video.latency = Some(ironav_engine::Latency::Low);
    let p = plan(&j).unwrap();
    let v = serde_json::to_value(&p).unwrap();
    assert!(v["steps"].as_array().unwrap().iter().any(|s| s["low_latency"] == true && s["action"] == "transcode"), "{v}");
    run(&j, &mut |_| {}).unwrap();
    let mut d = ironav_formats::open_demuxer(&out).unwrap();
    let video = d.streams().iter().position(|s| s.params.media_type() == ironav_core::MediaType::Video).unwrap();
    let mut pts = Vec::new();
    while let Some(pk) = d.read_packet().unwrap() {
        if pk.stream == video {
            pts.push(pk.pts);
        }
    }
    assert!(pts.len() > 10);
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "decode order is display order: {pts:?}");

    // audio has nothing to hold back, and a copied stream has no encoder to set up
    let mut a = job(&src, &dir.join("a.mp4"));
    a.audio.latency = Some(ironav_engine::Latency::Low);
    assert_eq!(plan(&a).unwrap_err().code, ErrorCode::InvalidArgument);
    let mut c = job(&src, &dir.join("c.mp4"));
    c.video.latency = Some(ironav_engine::Latency::Low);
    assert_eq!(plan(&c).unwrap_err().code, ErrorCode::InvalidArgument);
}

// QuickTime MOV: ffmpeg-made fixtures under tests/data, and files IronAV writes that ffmpeg reads back.

use ironav_core::{CodecId, Packet, StreamInfo};

/// Demuxes a file: its format name, its streams, and every packet in order.
fn demux_all(path: &Path) -> (&'static str, Vec<StreamInfo>, Vec<Packet>) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let streams = d.streams().to_vec();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        packets.push(p);
    }
    (d.format_name(), streams, packets)
}

/// The payloads of one stream's packets.
fn stream_data(packets: &[Packet], stream: usize) -> Vec<&[u8]> {
    packets.iter().filter(|p| p.stream == stream).map(|p| p.data.as_slice()).collect()
}

/// FFmpeg's (hash, size) for each packet of one stream, copied without re-encoding. Timestamps are
/// left out: they are in each container's own time base.
fn ffmpeg_packets(path: &Path, map: &str) -> Vec<(String, String)> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", map, "-c", "copy", "-f", "framemd5", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg could not read {}", path.display());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            (f[4].to_string(), f[5].to_string())
        })
        .collect()
}

/// The raw sample bytes of a WAV file.
fn wav_pcm(path: &Path) -> Vec<u8> {
    let (_, _, packets) = demux_all(path);
    packets.iter().flat_map(|p| p.data.iter().copied()).collect()
}

#[test]
fn quicktime_video_and_aac_are_the_packets_of_the_mp4_copy() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (mov_name, mov_streams, mov) = demux_all(&data.join("quicktime_h264_aac.mov"));
    let (mp4_name, _, mp4) = demux_all(&data.join("quicktime_h264_aac.mp4"));
    assert_eq!((mov_name, mp4_name), ("mov", "mp4"));
    assert_eq!(mov_streams.iter().map(|s| s.codec).collect::<Vec<_>>(), vec![CodecId::H264, CodecId::Aac]);
    for stream in 0..2 {
        let a: Vec<(i64, &[u8])> = mov.iter().filter(|p| p.stream == stream).map(|p| (p.pts, p.data.as_slice())).collect();
        let b: Vec<(i64, &[u8])> = mp4.iter().filter(|p| p.stream == stream).map(|p| (p.pts, p.data.as_slice())).collect();
        assert!(!a.is_empty());
        assert!(a == b, "stream {stream} differs from the MP4 copy");
    }
}

#[test]
fn quicktime_pcm_is_bit_exact_with_the_wav_it_was_made_from() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (want16, want24) = (wav_pcm(&data.join("quicktime_src_s16.wav")), wav_pcm(&data.join("quicktime_src_s24.wav")));
    // big-endian 'twos', little-endian 'sowt' (16 bits) and 'in24' with its 'enda' atom (24 bits)
    for (file, want, codec, bits) in [
        ("quicktime_h264_s16le.mov", &want16, CodecId::PcmS16Le, 16),
        ("quicktime_h264_s16be.mov", &want16, CodecId::PcmS16Le, 16),
        ("quicktime_h264_s24le.mov", &want24, CodecId::PcmS24Le, 24),
    ] {
        let (name, streams, packets) = demux_all(&data.join(file));
        assert_eq!(name, "mov");
        assert_eq!((streams[1].codec, streams[1].bits), (codec, Some(bits)), "{file}");
        let got: Vec<u8> = stream_data(&packets, 1).concat();
        assert!(got == *want, "{file}: {} bytes, the WAV has {}", got.len(), want.len());
    }
}

#[test]
fn quicktime_motion_jpeg_frames_are_the_matroska_copy_frames() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (name, streams, mov) = demux_all(&data.join("quicktime_mjpeg_s16le.mov"));
    let (_, _, mkv) = demux_all(&data.join("quicktime_mjpeg_s16le.mkv"));
    assert_eq!(name, "mov");
    assert_eq!((streams[0].codec, streams[1].codec), (CodecId::Mjpeg, CodecId::PcmS16Le));
    assert_eq!(stream_data(&mov, 0), stream_data(&mkv, 0));
}

#[test]
fn mp4_copies_into_mov_and_ffmpeg_reads_the_same_packets() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("mp4-to-mov");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (src, out) = (data.join("quicktime_h264_aac.mp4"), dir.join("out.mov"));
    let report = run(&job(&src, &out), &mut |_| {}).unwrap();
    assert_eq!(report.format, "mov");
    for map in ["0:v", "0:a"] {
        assert!(!ffmpeg_packets(&src, map).is_empty(), "{map}");
        assert_eq!(ffmpeg_packets(&out, map), ffmpeg_packets(&src, map), "{map}");
    }
    let (name, _, packets) = demux_all(&out);
    let (_, _, src_packets) = demux_all(&src);
    assert_eq!(name, "mov");
    assert_eq!(stream_data(&packets, 0), stream_data(&src_packets, 0));
}

#[test]
fn pcm_written_to_mov_decodes_bit_exact_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("pcm-to-mov");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for (wav, fmt, codec) in [("quicktime_src_s16.wav", "s16le", CodecId::PcmS16Le), ("quicktime_src_s24.wav", "s24le", CodecId::PcmS24Le)] {
        let (src, out) = (data.join(wav), dir.join(format!("{fmt}.mov")));
        run(&job(&src, &out), &mut |_| {}).unwrap();
        let want = wav_pcm(&src);
        assert!(!want.is_empty());
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&out)
            .args(["-map", "0:a", "-f", fmt, "-"])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        assert!(decoded.stdout == want, "{wav}: ffmpeg's decode differs from the source");
        let (name, streams, packets) = demux_all(&out);
        assert_eq!((name, streams[0].codec), ("mov", codec));
        assert!(stream_data(&packets, 0).concat() == want, "{wav}: IronAV reads its own MOV differently");
    }
}

#[test]
fn motion_jpeg_written_to_mov_reads_back_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("mjpeg-to-mov");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (src, out) = (data.join("quicktime_mjpeg_s16le.mov"), dir.join("out.mov"));
    run(&job(&src, &out), &mut |_| {}).unwrap();
    assert!(!ffmpeg_packets(&src, "0:v").is_empty());
    assert_eq!(ffmpeg_packets(&out, "0:v"), ffmpeg_packets(&src, "0:v"));
}

#[test]
fn quicktime_converts_back_to_mp4_with_the_same_packets() {
    let dir = scratch("mov-to-mp4");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (src, out) = (data.join("quicktime_h264_aac.mov"), dir.join("out.mp4"));
    let report = run(&job(&src, &out), &mut |_| {}).unwrap();
    assert_eq!(report.format, "mp4");
    let (name, _, packets) = demux_all(&out);
    let (_, _, src_packets) = demux_all(&src);
    assert_eq!(name, "mp4");
    for stream in 0..2 {
        assert_eq!(stream_data(&packets, stream), stream_data(&src_packets, stream), "stream {stream}");
    }
}
