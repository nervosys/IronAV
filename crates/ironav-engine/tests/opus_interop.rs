//! Writes the Opus encoder's short-frame streams as Ogg files, for decoding by another decoder
//! (libopus through FFmpeg, say): `IRONAV_OPUS_OUT=dir cargo test -p ironav-engine --test
//! opus_interop -- --ignored`. Each file is one second of a speech-like signal, which is also
//! written as `input.wav` to compare against: CELT at four frame lengths, and SILK at two.

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
    for ms in [2.5, 5.0, 10.0, 20.0] {
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
    for ms in [10.0, 20.0] {
        let mut enc = OpusEncoder::new(48_000, 1, 16_000).unwrap().with_mode(OpusMode::Silk).unwrap().with_frame_ms(ms).unwrap();
        let n = enc.frame_samples();
        let info = StreamInfo { codec: CodecId::Opus, extradata: enc.head(), bits: None, ..pcm.clone() };
        let mut m = ironav_formats::create_muxer(&dir.join(format!("voice_silk_{ms}ms.opus")), "ogg").unwrap();
        m.write_header(&[info]).unwrap();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for (k, f) in input.chunks_exact(n).enumerate() {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            m.write_packet(&Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() }).unwrap();
        }
        m.finish().unwrap();
    }
}
