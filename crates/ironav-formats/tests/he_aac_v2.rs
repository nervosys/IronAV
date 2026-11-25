//! HE-AAC v2 (AAC-LC with SBR and parametric stereo) in its containers. Two small synthetic streams
//! (a mono core, an SBR extension with `ps_data` in every frame, made by the codec's test-only writer;
//! no FFmpeg encoder writes parametric stereo): in an MP4 whose AudioSpecificConfig says so (object
//! type 29), and as ADTS, where nothing in the header does and the reader finds SBR and parametric
//! stereo in the first frames. Both report 44.1 kHz and two channels before any frame is decoded, and
//! decode to the same samples; the MP4 also decodes to what FFmpeg makes of it when `ffmpeg` is on PATH.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamInfo, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const FRAMES: usize = 24;

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// The stream's info and its decode: rate, channels and f32 samples.
fn demux_and_decode(file: &Path) -> (StreamInfo, Vec<f32>) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    assert_eq!(d.streams().len(), 1);
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, CodecId::Aac);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("an AAC decoder");
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        for f in dec.decode(&Packet { stream: 0, ..p }).expect("a decoded frame") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (sample_rate, channels), "frames agree with the stream's parameters");
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    }
    (stream, out)
}

#[test]
fn explicit_signalling_in_mp4_reports_two_channels_up_front() {
    let (stream, samples) = demux_and_decode(&data("he-aac-v2-explicit.mp4"));
    assert_eq!(stream.params, StreamParams::Audio { sample_rate: 44100, channels: 2 });
    assert_eq!(ironav_codec::aac::output_params(&stream.extradata), Some((44100, 2)));
    assert!(ironav_codec::aac::parametric_stereo(&stream.extradata));
    assert_eq!(samples.len(), FRAMES * 2048 * 2);
}

#[test]
fn adts_finds_the_implicit_signalling_in_the_first_frames() {
    let (adts, from_adts) = demux_and_decode(&data("he-aac-v2-implicit.aac"));
    assert_eq!(adts.params, StreamParams::Audio { sample_rate: 44100, channels: 2 });
    assert!(ironav_codec::aac::parametric_stereo(&adts.extradata));
    let (_, from_mp4) = demux_and_decode(&data("he-aac-v2-explicit.mp4"));
    assert_eq!(from_adts, from_mp4, "the same samples either way");
}

#[test]
fn a_damaged_adts_file_does_not_panic() {
    let bytes = std::fs::read(data("he-aac-v2-implicit.aac")).unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("heaac-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (i, cut) in [bytes.len() - 5, bytes.len() / 2, 300, 40].into_iter().enumerate() {
        let mut b = bytes[..cut].to_vec();
        if i == 1 {
            // and some of the extension data of a middle frame overwritten
            for x in b.iter_mut().skip(cut / 2).take(60) {
                *x ^= 0x5A;
            }
        }
        let f = dir.join(format!("cut{i}.aac"));
        std::fs::write(&f, &b).unwrap();
        // opening may fail; decoding may fail; neither may panic
        if let Ok(mut d) = open_demuxer(&f) {
            let stream = d.streams()[0].clone();
            if let Ok(mut dec) = ironav_codec::new_decoder_for(&stream) {
                while let Ok(Some(p)) = d.read_packet() {
                    let _ = dec.decode(&Packet { stream: 0, ..p });
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mp4_decodes_like_ffmpeg() {
    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("heaac-ff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("v2.f32");
    let ok = Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(data("he-aac-v2-explicit.mp4")).args(["-f", "f32le", "-acodec", "pcm_f32le"]).arg(&out).status().is_ok_and(|s| s.success());
    assert!(ok, "FFmpeg decodes the file");
    let want: Vec<f32> = std::fs::read(&out).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let _ = std::fs::remove_dir_all(&dir);
    let (_, got) = demux_and_decode(&data("he-aac-v2-explicit.mp4"));
    assert_eq!(got.len(), want.len(), "sample count");
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(&want) {
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    let snr = 10.0 * (sig / noise.max(1e-300)).log10();
    eprintln!("HE-AAC v2 MP4 against FFmpeg: SNR {snr:.1} dB over {} samples", got.len());
    assert!(snr > 55.0, "SNR {snr} dB against FFmpeg");
}
