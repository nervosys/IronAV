//! Writes the Opus encoder's short-frame streams as Ogg files, for decoding by another decoder
//! (libopus through FFmpeg, say): `IRONAV_OPUS_OUT=dir cargo test -p ironav-engine --test
//! opus_interop -- --ignored`. Each file is one second of a speech-like signal, which is also
//! written as `input.wav` to compare against: CELT at six frame lengths, SILK and hybrid at four (packets of 40
//! and 60 ms hold two or three frames), each mode with DTX across a pause, against `pause.wav`, and each mode
//! in stereo, against `stereo.wav`; SILK and hybrid also with in-band FEC (`_fec`).

use ironav_codec::opus::{opus_head, OpusEncoder, OpusMode};
use ironav_core::{CodecId, Packet, Rational, StreamInfo, StreamParams};
use std::path::PathBuf;

fn voice(n: usize) -> Vec<f32> {
    let mut phase = 0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / 48_000.0;
            let f0 = 140.0 + 40.0 * (2.0 * std::f64::consts::PI * 1.5 * t).sin();
            phase += 2.0 * std::f64::consts::PI * f0 / 48_000.0;
            let env = (2.0 * std::f64::consts::PI * 4.0 * t).sin().abs();
            (env * (0.3 * phase.sin() + 0.12 * (2.0 * phase).sin() + 0.06 * (3.0 * phase).sin())) as f32
        })
        .collect()
}

#[test]
#[ignore]
fn short_frame_streams_for_another_decoder() {
    let Ok(dir) = std::env::var("IRONAV_OPUS_OUT") else { return };
    let dir = PathBuf::from(dir);
    let input = voice(48_000);
    let mut wav = ironav_formats::create_muxer(&dir.join("input.wav"), "wav").unwrap();
    let pcm = StreamInfo { index: 0, codec: CodecId::PcmS16Le, time_base: Rational::new(1, 48_000), duration: None, bits: Some(16), extradata: Vec::new(), lead_in: 0, params: StreamParams::Audio { sample_rate: 48_000, channels: 1 } };
    wav.write_header(std::slice::from_ref(&pcm)).unwrap();
    let data = input.iter().flat_map(|v| ((v * 32767.0).round() as i16).to_le_bytes()).collect();
    wav.write_packet(&Packet { stream: 0, pts: 0, duration: input.len() as i64, data }).unwrap();
    wav.finish().unwrap();
    for ms in [2.5, 5.0, 10.0, 20.0, 40.0, 60.0] {
        let mut enc = OpusEncoder::new(48_000, 1, 32_000).unwrap().with_frame_ms(ms).unwrap();
        let n = enc.frame_samples();
        let info = StreamInfo { codec: CodecId::Opus, extradata: opus_head(1), bits: None, ..pcm.clone() };
        let mut m = ironav_formats::create_muxer(&dir.join(format!("voice_{ms}ms.opus")), "ogg").unwrap();
        m.write_header(&[info]).unwrap();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for (k, f) in input.chunks_exact(n).enumerate() {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            m.write_packet(&Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() }).unwrap();
        }
        m.finish().unwrap();
    }
    let modes = [(OpusMode::Silk, "silk", 16_000, 0), (OpusMode::Hybrid, "hybrid", 24_000, 0), (OpusMode::Silk, "silk_fec", 24_000, 10), (OpusMode::Hybrid, "hybrid_fec", 32_000, 10)];
    for (mode, name, rate, fec, ms) in modes.into_iter().flat_map(|(m, n, r, f)| [10.0, 20.0, 40.0, 60.0].map(|ms| (m, n, r, f, ms))) {
        let mut enc = OpusEncoder::new(48_000, 1, rate).unwrap().with_mode(mode).unwrap().with_frame_ms(ms).unwrap().with_fec(fec);
        let n = enc.frame_samples();
        let info = StreamInfo { codec: CodecId::Opus, extradata: enc.head(), bits: None, ..pcm.clone() };
        let mut m = ironav_formats::create_muxer(&dir.join(format!("voice_{name}_{ms}ms.opus")), "ogg").unwrap();
        m.write_header(&[info]).unwrap();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for (k, f) in input.chunks_exact(n).enumerate() {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            m.write_packet(&Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() }).unwrap();
        }
        m.finish().unwrap();
    }
    // with DTX: the signal, a second of silence and the signal again, as `pause.wav`
    let paused: Vec<f32> = input.iter().copied().chain(std::iter::repeat_n(0.0, 48_000)).chain(input.iter().copied()).collect();
    let mut wav = ironav_formats::create_muxer(&dir.join("pause.wav"), "wav").unwrap();
    wav.write_header(std::slice::from_ref(&pcm)).unwrap();
    let data = paused.iter().flat_map(|v| ((v * 32767.0).round() as i16).to_le_bytes()).collect();
    wav.write_packet(&Packet { stream: 0, pts: 0, duration: paused.len() as i64, data }).unwrap();
    wav.finish().unwrap();
    for (mode, name, rate) in [(OpusMode::Celt, "celt", 32_000), (OpusMode::Silk, "silk", 16_000), (OpusMode::Hybrid, "hybrid", 24_000)] {
        let mut enc = OpusEncoder::new(48_000, 1, rate).unwrap().with_mode(mode).unwrap().with_dtx(true);
        let n = enc.frame_samples();
        let info = StreamInfo { codec: CodecId::Opus, extradata: enc.head(), bits: None, ..pcm.clone() };
        let mut m = ironav_formats::create_muxer(&dir.join(format!("pause_{name}_dtx.opus")), "ogg").unwrap();
        m.write_header(&[info]).unwrap();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for (k, f) in paused.chunks_exact(n).enumerate() {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            m.write_packet(&Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() }).unwrap();
        }
        m.finish().unwrap();
        assert!(enc.skipped > 30, "{name}: {} skipped", enc.skipped);
    }
    // stereo, as `stereo.wav`: the signal on the left, and on the right a quieter copy 2 ms later
    // with a tone
    let stereo: Vec<f32> = (0..input.len())
        .flat_map(|i| {
            let tone = 0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
            [input[i], 0.6 * input[i.saturating_sub(96)] + tone]
        })
        .collect();
    let pcm2 = StreamInfo { params: StreamParams::Audio { sample_rate: 48_000, channels: 2 }, ..pcm.clone() };
    let mut wav = ironav_formats::create_muxer(&dir.join("stereo.wav"), "wav").unwrap();
    wav.write_header(std::slice::from_ref(&pcm2)).unwrap();
    let data = stereo.iter().flat_map(|v| ((v * 32767.0).round() as i16).to_le_bytes()).collect();
    wav.write_packet(&Packet { stream: 0, pts: 0, duration: input.len() as i64, data }).unwrap();
    wav.finish().unwrap();
    for (mode, name, rate, fec) in [(OpusMode::Celt, "celt", 64_000, 0), (OpusMode::Silk, "silk", 32_000, 0), (OpusMode::Hybrid, "hybrid", 40_000, 0), (OpusMode::Silk, "silk_fec", 32_000, 10)] {
        let mut enc = OpusEncoder::new(48_000, 2, rate).unwrap().with_mode(mode).unwrap().with_fec(fec);
        let n = enc.frame_samples();
        let info = StreamInfo { codec: CodecId::Opus, extradata: enc.head(), bits: None, ..pcm2.clone() };
        let mut m = ironav_formats::create_muxer(&dir.join(format!("stereo_{name}.opus")), "ogg").unwrap();
        m.write_header(&[info]).unwrap();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for (k, f) in stereo.chunks_exact(2 * n).enumerate() {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            m.write_packet(&Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() }).unwrap();
        }
        m.finish().unwrap();
    }
}
