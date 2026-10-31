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
    assert!(o.get("codec/mpeg4").is_some_and(|e| e.status == ironav_ontology::Status::Planned));
    // AV1 has no software codec; it is usable only where the NVDEC provider is compiled in.
    let av1 = o.get("codec/av1").expect("av1 entity");
    let gpu = cfg!(any(windows, target_os = "linux"));
    assert_eq!(av1.status, if gpu { ironav_ontology::Status::Implemented } else { ironav_ontology::Status::Planned });
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

/// Decode an Opus file and compare it with FFmpeg's native decoder (pre-skip removed in both; ours
/// keeps the encoder's trailing padding, which FFmpeg trims). Returns the SNR in dB and the worst
/// difference in LSB.
/// `late` is how many samples (interleaved) FFmpeg's output runs behind ours: 3 for mono SILK,
/// where its native decoder keeps a 16 kHz sample more than libopus (and its own stereo path).
fn opus_vs_ffmpeg(file: &str, ext: &str, late: usize) -> (f64, i32) {
    let dir = scratch(&format!("opus-{file}-{ext}"));
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let out = dir.join("out.wav");
    let mut j = job(&data.join(format!("{file}.{ext}")), &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    let s16 = |b: Vec<u8>| -> Vec<i32> { b.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect() };
    let got = s16(pcm(&out));
    let want = s16(std::fs::read(data.join(format!("{file}.ffmpeg.s16"))).unwrap());
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
    let (snr, worst) = opus_vs_ffmpeg("opus_celt_stereo", "opus", 0);
    assert!(worst <= 3, "largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
    let (snr, _) = opus_vs_ffmpeg("opus_silk_mono", "opus", 3);
    assert!(snr > 35.0, "SNR against FFmpeg is only {snr:.1} dB");
}

#[test]
fn celt_opus_decodes_to_what_ffmpeg_decodes() {
    let (snr, worst) = opus_vs_ffmpeg("opus_celt_stereo", "mka", 0);
    assert!(worst <= 3, "largest difference from FFmpeg is {worst} LSB (SNR {snr:.1} dB)");
}

/// SILK and hybrid frames are upsampled by our own filter, so they agree with FFmpeg's resampler to
/// about 40 dB rather than to the bit.
#[test]
fn silk_and_hybrid_opus_decode_close_to_what_ffmpeg_decodes() {
    for (file, late) in [("opus_silk_mono", 3), ("opus_hybrid_stereo", 0)] {
        let (snr, _) = opus_vs_ffmpeg(file, "mka", late);
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

    // three or more channels are refused with a reason
    let six = dir.join("six.wav");
    write_wav(&six, &vec![0i16; 6 * 4410], 6, 44_100);
    let e = plan(&{
        let mut j = job(&six, &dir.join("six.ogg"));
        j.audio.codec = Some("vorbis".into());
        j
    })
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedCodec, "{e:?}");
}

/// AAC encoding into MP4, ADTS and Matroska: the bit rate steers the size, our decoder gets back
/// something close to the input, and the encoder's priming frame is skipped where the container says so
/// (the MP4 edit list) and left where it does not (ADTS, Matroska).
#[test]
fn aac_encoder_output_decodes_close_to_its_input() {
    let dir = scratch("aac-enc");
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
    for (ext, kbps, lead) in [("m4a", 48u32, 0usize), ("m4a", 160, 0), ("aac", 160, 1024), ("mka", 160, 1024)] {
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
        assert!(frames >= n + lead && frames < n + lead + 2048, "{ext} {kbps}: {frames} frames for {n}");
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

/// Encodes `src` to `out` with the VP9 encoder (key frames only) at `qp` and returns the size of the file.
fn vp9_encode(src: &Path, out: &Path, qp: u8) -> u64 {
    let mut j = job(src, out);
    j.hwaccel = "off".into();
    j.video.codec = Some("vp9".into());
    j.video.qp = Some(qp);
    run(&j, &mut |_| {}).unwrap();
    std::fs::metadata(out).unwrap().len()
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
    let (w, h, n) = (177, 99, 6);
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

/// AV1 is decoded only on the GPU. Without one (or with hardware off) the job must refuse
/// precisely; with one, the pictures must equal libdav1d's (AV1 decoding is exact).
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
        assert_eq!(plan(&off).unwrap_err().code, ErrorCode::HardwareUnavailable, "{name}: no software AV1 decoder");

        let report = match run(&j, &mut |_| {}) {
            Ok(r) => r,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::HardwareUnavailable, "{name}: {}", e.message);
                assert!(!out.exists(), "{name}: nothing is written when decoding is refused");
                continue;
            }
        };
        assert_eq!(report.steps[0].hardware_decoder, Some("cuda"));
        let got: Vec<u8> = y4m_frames_any(&out).concat();
        let want = std::fs::read(data.join(format!("av1_{name}.dav1d.yuv"))).unwrap();
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
