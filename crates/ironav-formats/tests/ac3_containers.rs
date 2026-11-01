//! AC-3 in its containers: a stream made with FFmpeg's AC-3 encoder, read with `open_demuxer`, decodes
//! with the engine's decoder for the stream to what FFmpeg's decoder makes of the same file. Skipped
//! without `ffmpeg` on PATH.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIGNAL: &str = "aevalsrc=exprs='0.4*sin(2*PI*440*t)+0.3*(random(0)-0.5)*(lt(mod(t,0.25),0.02))|0.3*sin(2*PI*660*t)':s=48000:d=2";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ac3-container-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// FFmpeg's decode of `file` as f32 samples.
fn reference(dir: &Path, name: &str, file: &Path) -> Vec<f32> {
    let out = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    std::fs::read(&out).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Demux `file` with the formats registry, decode its AC-3 packets, and return the samples with the
/// stream's rate and channel count.
fn demux_and_decode(file: &Path) -> (Vec<f32>, u32, u16) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, CodecId::Ac3);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    // the engine's decoder for the stream, which keeps the samples the container's edit keeps
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("an AC-3 decoder");
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        let p = Packet { stream: 0, ..p };
        for f in dec.decode(&p).expect("a decoded frame") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (sample_rate, channels));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    }
    (out, sample_rate, channels)
}

/// Compare the demuxed and decoded samples with FFmpeg's, and check the worst difference.
fn check(name: &str, got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let worst = got.iter().zip(want).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
    eprintln!("{name:>16}: max abs diff {worst:.3e} over {} samples", got.len());
    // The bound is the decoder's float error on this signal (the codec's own test measures it on
    // others, at up to 2.5e-5); a container that dropped or reordered frames would miss by far more.
    assert!(worst < 5e-5, "{name}: worst difference {worst}");
}

/// Encode a stereo AC-3 stream into `name` (any container FFmpeg writes it to) and compare what our
/// demuxer and decoder make of it with FFmpeg's decode.
fn encode_and_compare(dir: &Path, name: &str, format: &str, channels: &str) -> PathBuf {
    let file = dir.join(name);
    assert!(
        ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", channels, "-c:a", "ac3", "-b:a", "192k", "-f", format, file.to_str().unwrap()]),
        "encoding {name}"
    );
    let want = reference(dir, name, &file);
    let (got, _, _) = demux_and_decode(&file);
    check(name, &got, &want);
    file
}

#[test]
fn matroska_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    encode_and_compare(&dir, "stereo_192.mkv", "matroska", "2");
    encode_and_compare(&dir, "surround_384.mkv", "matroska", "6");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A raw AC-3 file copied into Matroska with our writer reads back to the same samples.
#[test]
fn raw_ac3_copied_to_matroska_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let raw = dir.join("copy_source.ac3");
    assert!(ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", "2", "-c:a", "ac3", "-b:a", "192k", "-f", "ac3", raw.to_str().unwrap()]), "encoding the source");
    let want = reference(&dir, "copy_source", &raw);
    let mut input = open_demuxer(&raw).expect("the raw file opens");
    let streams = input.streams().to_vec();
    let out = dir.join("copied.mkv");
    let mut muxer = ironav_formats::create_muxer(&out, "mkv").expect("a Matroska writer");
    muxer.write_header(&streams).expect("the header");
    while let Some(p) = input.read_packet().expect("a packet") {
        muxer.write_packet(&p).expect("the packet");
    }
    muxer.finish().expect("the trailer");
    let (got, _, _) = demux_and_decode(&out);
    check("copied.mkv", &got, &want);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MPEG CRC-32 (polynomial 0x04C11DB7, no reflection), as PSI sections carry it.
fn mpeg_crc(data: &[u8]) -> u32 {
    data.iter().fold(0xFFFF_FFFFu32, |crc, &b| {
        let mut c = crc ^ ((b as u32) << 24);
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04C1_1DB7 } else { c << 1 };
        }
        c
    })
}

/// The same transport stream with its AC-3 elementary stream (type 0x81) declared the DVB way: stream
/// type 0x06 with the AC-3 descriptor (tag 0x6A). Rewrites the PMT, which must sit in a packet without
/// an adaptation field.
fn dvb_style_pmt(ts: &[u8]) -> Vec<u8> {
    let mut out = ts.to_vec();
    for p in out.chunks_exact_mut(188) {
        let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
        if p[1] & 0x40 == 0 || p[3] >> 4 != 1 {
            continue;
        }
        let ptr = p[4] as usize;
        let sec = 5 + ptr;
        if pid == 0 || p[sec] != 0x02 {
            continue; // not a PMT (the PAT is on PID 0)
        }
        let len = (((p[sec + 1] & 0x0F) as usize) << 8) | p[sec + 2] as usize;
        let body = &p[sec + 3..sec + 3 + len - 4]; // without the CRC
        // program_number, version, two section numbers, PCR PID, then program_info_length
        let info_len = (((body[7] & 0x0F) as usize) << 8) | body[8] as usize;
        let mut sec_new = body[..9 + info_len].to_vec();
        let mut i = 9 + info_len;
        while i + 5 <= body.len() {
            let es_len = (((body[i + 3] & 0x0F) as usize) << 8) | body[i + 4] as usize;
            let (ty, rest) = (body[i], &body[i..i + 5 + es_len]);
            if ty == 0x81 {
                // stream type 0x06, the ES info gets the AC-3 descriptor (tag 0x6A, one byte of flags)
                let info = [&rest[5..], &[0x6A, 1, 0][..]].concat();
                sec_new.extend_from_slice(&[0x06, rest[1], rest[2], 0xF0 | (info.len() >> 8) as u8, info.len() as u8]);
                sec_new.extend_from_slice(&info);
            } else {
                sec_new.extend_from_slice(rest);
            }
            i += 5 + es_len;
        }
        // the section: table id, length (after this field, CRC included), body, CRC
        let section_len = sec_new.len() + 4; // the length field counts everything after itself, CRC included
        let mut section = vec![0x02, 0xB0 | ((section_len >> 8) as u8 & 0x0F), section_len as u8];
        section.extend_from_slice(&sec_new);
        let crc = mpeg_crc(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        let mut packet = p[..4].to_vec();
        packet.push(ptr as u8);
        packet.extend(vec![0xFF; ptr]);
        packet.extend_from_slice(&section);
        assert!(packet.len() <= 188, "the rewritten PMT fits its packet");
        packet.resize(188, 0xFF);
        p.copy_from_slice(&packet);
    }
    out
}

#[test]
fn mpegts_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let file = encode_and_compare(&dir, "stereo_192.ts", "mpegts", "2");
    // the same stream with its PMT declaring AC-3 the DVB way
    let dvb = dir.join("dvb_stereo_192.ts");
    let original = std::fs::read(&file).unwrap();
    let rewritten = dvb_style_pmt(&original);
    // the rewrite must have changed the PMT: otherwise the 0x81 stream would be read and the test vacuous
    assert!(rewritten.windows(3).any(|w| w == [0x6A, 1, 0]), "the PMT carries the AC-3 descriptor");
    assert_ne!(rewritten, original, "the PMT was rewritten");
    std::fs::write(&dvb, rewritten).unwrap();
    let want = reference(&dir, "stereo_192.ts", &file);
    let (got, _, _) = demux_and_decode(&dvb);
    check("dvb_stereo_192.ts", &got, &want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mp4_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    encode_and_compare(&dir, "stereo_192.mp4", "mp4", "2");
    encode_and_compare(&dir, "surround_384.mp4", "mp4", "6");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC-3 is read but not written: the MP4 writer refuses it, and the Matroska and raw paths keep working.
#[test]
fn ac3_is_not_written_to_mp4() {
    let dir = scratch();
    let out = dir.join("refused.mp4");
    let stream = ironav_core::StreamInfo {
        index: 0,
        codec: CodecId::Ac3,
        time_base: ironav_core::Rational::new(1, 48000),
        duration: None,
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: 48000, channels: 2 },
    };
    let mut muxer = ironav_formats::create_muxer(&out, "mp4").expect("an MP4 writer");
    let err = muxer.write_header(&[stream]).expect_err("AC-3 is refused");
    assert_eq!(err.code, ironav_core::ErrorCode::UnsupportedCodec);
    let _ = std::fs::remove_dir_all(&dir);
}

/// E-AC-3 (bit stream id 16) is refused with a clear error, not decoded as AC-3.
#[test]
fn eac3_is_refused() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let file = dir.join("refused.eac3");
    assert!(ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", "2", "-c:a", "eac3", "-f", "eac3", file.to_str().unwrap()]), "encoding E-AC-3");
    let err = open_demuxer(&file).err().expect("E-AC-3 is refused");
    assert_eq!(err.code, ironav_core::ErrorCode::UnsupportedCodec, "{}", err.message);
    assert!(err.message.contains("E-AC-3"), "{}", err.message);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn raw_ac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    for (name, channels, kbps) in [("raw_stereo_192", "2", "192k"), ("raw_surround_384", "6", "384k")] {
        let file = dir.join(format!("{name}.ac3"));
        assert!(
            ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", channels, "-c:a", "ac3", "-b:a", kbps, "-f", "ac3", file.to_str().unwrap()]),
            "encoding {name}"
        );
        let want = reference(&dir, name, &file);
        let (got, _, _) = demux_and_decode(&file);
        check(name, &got, &want);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
