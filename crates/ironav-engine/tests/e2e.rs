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
