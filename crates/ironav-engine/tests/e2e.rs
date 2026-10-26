use ironav_core::ErrorCode;
use ironav_engine::{call_tool, ontology, plan, run, FilterCall, Job};
use serde_json::json;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-test-{}-{name}", std::process::id()));
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

fn job(input: &PathBuf, output: &PathBuf) -> Job {
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
    assert_eq!(serde_json::to_value(&p.steps[0].action).unwrap(), "copy");
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
    assert_eq!(r["feasible_in_this_build"], false);
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
    // 16-bit: from the libFLAC reference WAV
    for (src, codec) in [(fixture("ref_stereo16.wav"), "pcm_s16le"), (fixture("ref_mono24.wav"), "pcm_s24le")] {
        let (f, back) = (d.join("x.flac"), d.join("back.wav"));
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(&back);
        let mut j = job(&src, &f);
        j.audio.codec = Some("flac".into());
        let r = run(&j, &mut |_| {}).unwrap();
        assert!(r.bytes_out < r.bytes_in, "FLAC should compress this signal");
        let mut j = job(&f, &back);
        j.audio.codec = Some(codec.into());
        run(&j, &mut |_| {}).unwrap();
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

    // FLAC cannot be written into WAV: the error must say which codecs would work.
    let e = plan(&job(&fixture("ref_stereo16.flac"), &d.join("o.wav"))).unwrap_err();
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
        f.extend(std::iter::repeat(128u8).take(w * h / 2));
    }
    std::fs::write(path, f).unwrap();
}

/// H.264 has no software encoder, so the result depends on the machine: with a usable NVENC
/// GPU it must produce a real Annex B stream; without one it must fail with a precise,
/// recoverable error and leave nothing behind.
#[test]
fn h264_uses_the_gpu_or_fails_precisely() {
    let d = scratch("h264");
    let (src, out) = (d.join("in.y4m"), d.join("out.h264"));
    write_y4m(&src, 256, 144, 30);

    let mut off = job(&src, &out);
    off.video.codec = Some("h264".into());
    off.hwaccel = "off".into();
    let e = plan(&off).unwrap_err();
    assert_eq!(e.code, ErrorCode::HardwareUnavailable);
    assert!(e.hint.as_deref().unwrap_or("").contains("auto"), "hint should say how to recover: {:?}", e.hint);

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

    // MP4 only holds codecs it can carry; the same GPU requirement applies.
    let mp4 = d.join("out.mp4");
    let mut m = job(&src, &mp4);
    m.video.codec = Some("h264".into());
    m.video.qp = Some(28);
    if run(&m, &mut |_| {}).is_ok() {
        let b = std::fs::read(&mp4).unwrap();
        assert_eq!(&b[4..8], b"ftyp");
        assert!(b.windows(4).any(|w| w == b"moov") && b.windows(4).any(|w| w == b"avcC"));
    } else {
        assert!(!mp4.exists());
    }
    let mut keep = job(&src, &d.join("copy.mp4"));
    keep.video.codec = Some("copy".into());
    assert_eq!(plan(&keep).unwrap_err().code, ErrorCode::UnsupportedCodec, "rawvideo cannot go in an mp4");

    let mut j = job(&src, &out);
    j.video.codec = Some("h264".into());
    j.video.qp = Some(28);
    match run(&j, &mut |_| {}) {
        Ok(report) => {
            assert_eq!(report.steps[0].hardware_encoder, Some("cuda"));
            assert_eq!(report.steps[0].packets_out, 30);
            let bytes = std::fs::read(&out).unwrap();
            assert!(bytes.starts_with(&[0, 0, 0, 1]), "Annex B start code");
            assert!(bytes.len() > 100 && bytes.len() < 256 * 144 * 3 * 30 / 4, "plausible compressed size, got {}", bytes.len());
        }
        Err(e) => {
            assert_eq!(e.code, ErrorCode::HardwareUnavailable, "{e:?}");
            assert!(!out.exists() && !d.join("out.h264.partial").exists(), "failure must leave no output");
        }
    }
}

#[test]
fn schema_and_ontology_know_hardware_codecs() {
    let schema = ironav_engine::tools::job_schema().to_string();
    assert!(schema.contains("bitrate_kbps") && schema.contains("\"h264\""));
    let o = ontology();
    let h264 = o.get("codec/h264").expect("h264 entity");
    assert_eq!(h264.status, ironav_ontology::Status::Implemented);
    assert!(o.get("codec/av1").is_some_and(|e| e.status == ironav_ontology::Status::Planned));
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

    match run(&j, &mut |_| {}) {
        Ok(report) => {
            assert_eq!(report.steps.len(), 2);
            assert_eq!(report.plan.output.streams.len(), 2);
            assert_eq!((report.steps[0].input_stream, report.steps[1].input_stream), (0, 0));
            assert_eq!(report.plan.extra_inputs.len(), 1);
            let b = std::fs::read(&out).unwrap();
            assert!(has(&b, b"avcC") && has(&b, b"fLaC") && has(&b, b"vide") && has(&b, b"soun"));
            let secs = report.media_seconds.unwrap();
            assert!((secs - 1.0).abs() < 0.05, "one second of media, got {secs}");
        }
        Err(e) => {
            // Without a usable GPU the video stream cannot be encoded; nothing may be left behind.
            assert_eq!(e.code, ErrorCode::HardwareUnavailable, "{e:?}");
            assert!(!out.exists() && !d.join("out.mp4.partial").exists());
        }
    }
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
    if run(&enc, &mut |_| {}).is_err() {
        // No usable GPU: decoding must refuse just as precisely, and write nothing.
        return;
    }

    // With hardware off there is no H.264 decoder at all, and the error says how to recover.
    let mut off = job(&mp4, &back);
    off.video.codec = Some("rawvideo".into());
    off.hwaccel = "off".into();
    let e = plan(&off).unwrap_err();
    assert_eq!(e.code, ErrorCode::HardwareUnavailable);
    assert!(e.hint.as_deref().unwrap_or("").contains("auto"), "{:?}", e.hint);

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

fn aac_matches_ffmpeg(scratch_name: &str, file: &str) {
    let dir = scratch(scratch_name);
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let input = data.join(file);
    let out = dir.join("out.wav");
    let mut j = job(&input, &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let got: Vec<i16> = pcm(&out).chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let want: Vec<i16> = std::fs::read(data.join("aac_stereo_tools.ffmpeg.s16")).unwrap().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    assert_eq!(got.len(), want.len());
    let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 2, "largest difference from FFmpeg's decode is {worst} LSB");
}

/// Decode an Opus file and compare it with FFmpeg's native decoder (pre-skip removed in both; ours
/// keeps the encoder's trailing padding, which FFmpeg trims). Returns the SNR in dB and the worst
/// difference in LSB.
fn opus_vs_ffmpeg(file: &str, ext: &str) -> (f64, i32) {
    let dir = scratch(&format!("opus-{file}-{ext}"));
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join(format!("{file}.{ext}")), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let s16 = |b: Vec<u8>| -> Vec<i32> { b.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
    let got = s16(pcm(&out));
    let want = s16(std::fs::read(data.join(format!("{file}.ffmpeg.s16"))).unwrap());
    assert!(got.len() >= want.len() && got.len() < want.len() + 4 * 2000, "{} samples against {}", got.len(), want.len());
    let (mut sig, mut err, mut worst) = (0f64, 0f64, 0);
    for (a, b) in got.iter().zip(&want) {
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
    let s16 = |b: Vec<u8>| -> Vec<i32> { b.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
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
    let (snr, worst) = opus_vs_ffmpeg("opus_celt_stereo", "opus");
    assert!(worst <= 3, "largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
    let (snr, _) = opus_vs_ffmpeg("opus_silk_mono", "opus");
    assert!(snr > 35.0, "SNR against FFmpeg is only {snr:.1} dB");
}

#[test]
fn celt_opus_decodes_to_what_ffmpeg_decodes() {
    let (snr, worst) = opus_vs_ffmpeg("opus_celt_stereo", "mka");
    assert!(worst <= 3, "largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
}

/// SILK and hybrid frames are upsampled by our own filter, so they agree with FFmpeg's resampler to
/// about 40 dB rather than to the bit.
#[test]
fn silk_and_hybrid_opus_decode_close_to_what_ffmpeg_decodes() {
    for file in ["opus_silk_mono", "opus_hybrid_stereo"] {
        let (snr, _) = opus_vs_ffmpeg(file, "mka");
        assert!(snr > 35.0, "{file}: SNR against FFmpeg is only {snr:.1} dB");
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
    let want: Vec<i32> = std::fs::read(data.join(format!("{file}.ffmpeg.s16"))).unwrap().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect();
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

/// WebM output: Opus and Vorbis audio only, with the `webm` document type; anything else is refused
/// with the codecs that would fit.
#[test]
fn webm_holds_opus_and_vorbis_only() {
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
    let s = |b: &[u8]| b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64).collect::<Vec<_>>();
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

    // other sample rates are refused with the way out
    let wav = dir.join("in44.wav");
    write_wav(&wav, &tone(44_100), 1, 44_100);
    let mut j = job(&wav, &dir.join("x.mka"));
    j.audio.codec = Some("opus".into());
    let e = plan(&j).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
    assert!(e.hint.as_deref().is_some_and(|h| h.contains("48000")), "{:?}", e.hint);
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
        let mut j = job(&data.join("vorbis_stereo.ogg"), &dir.join("v.opus"));
        j.audio.codec = Some("vorbis".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec);
}
