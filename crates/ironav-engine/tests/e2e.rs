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
