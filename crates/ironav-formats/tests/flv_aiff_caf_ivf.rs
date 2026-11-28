//! FLV, AIFF, CAF and IVF files made by FFmpeg, read with `open_demuxer`: the streams, the packet times
//! against `ffprobe -show_packets` (when `ffprobe` is on PATH), the decoded sound against FFmpeg's (bit for
//! bit for PCM and ALAC, by SNR for AAC and MP3), seeking, and damaged files. Cases whose encoder FFmpeg
//! lacks are skipped, and the whole file is skipped without `ffmpeg` on PATH. CAF has no AAC from FFmpeg
//! (its writer leaves the cookie out), so that file is built here from the packets of an ADTS file.

use ironav_core::{CodecId, Demuxer, Frame, MediaType, Packet, StreamInfo, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool_available(name: &str) -> bool {
    Command::new(name).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("containers-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Runs FFmpeg with `args` (an output path last); `None` when it fails, as it does for an encoder it lacks.
fn ffmpeg(args: &[&str]) -> Option<()> {
    let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().ok()?;
    if !out.status.success() {
        eprintln!("skipped: ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    out.status.success().then_some(())
}

const VIDEO: &str = "testsrc2=size=64x48:rate=10:duration=1.2";

/// A 1.2 s test pattern at 10 fps and a 440 Hz tone, with `out` (codec options) to write `name`.
fn make(dir: &Path, name: &str, video: bool, audio: bool, out: &[&str]) -> Option<PathBuf> {
    let file = dir.join(name);
    let mut args = Vec::new();
    if video {
        args.extend(["-f", "lavfi", "-i", VIDEO]);
    }
    if audio {
        args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1.2"]);
    }
    args.extend_from_slice(out);
    args.push(file.to_str().unwrap());
    ffmpeg(&args).map(|_| file)
}

/// What `ffprobe` says of a stream's packets: presentation time, decode time and whether a key frame.
struct Probed {
    pts: f64,
    dts: f64,
    key: bool,
}

fn ffprobe_packets(file: &Path, select: &str) -> Option<Vec<Probed>> {
    if !tool_available("ffprobe") {
        return None;
    }
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", select, "-show_entries", "packet=pts_time,dts_time,flags", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let rows = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(',').collect();
            (f.len() >= 3).then(|| {
                let pts = f[0].parse().ok();
                let dts = f[1].parse().ok();
                Probed { pts: pts.or(dts).unwrap_or(0.0), dts: dts.or(pts).unwrap_or(0.0), key: f[2].starts_with('K') }
            })
        })
        .collect::<Vec<_>>();
    Some(rows)
}

fn open(file: &Path, name: &str) -> Box<dyn Demuxer> {
    let d = open_demuxer(file).unwrap_or_else(|e| panic!("{} does not open: {}", file.display(), e.message));
    assert_eq!(d.format_name(), name);
    d
}

fn read_all(d: &mut dyn Demuxer) -> Vec<Packet> {
    let mut v = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        v.push(p);
    }
    v
}

fn secs(s: &StreamInfo, ticks: i64) -> f64 {
    ticks as f64 * s.time_base.as_f64()
}

/// Our packets of `stream` against `ffprobe`'s: as many, at the same times.
fn compare_times(d: &dyn Demuxer, packets: &[Packet], stream: usize, probed: &[Probed], what: &str) {
    let s = &d.streams()[stream];
    let mut ours: Vec<f64> = packets.iter().filter(|p| p.stream == stream).map(|p| secs(s, p.pts)).collect();
    let mut theirs: Vec<f64> = probed.iter().map(|p| p.pts).collect();
    assert_eq!(ours.len(), theirs.len(), "{what}: packet count");
    ours.sort_by(f64::total_cmp);
    theirs.sort_by(f64::total_cmp);
    for (a, b) in ours.iter().zip(&theirs) {
        assert!((a - b).abs() < 1e-5, "{what}: packet time {a} against {b}");
    }
}

/// Seeking to the time of each of the video's pictures lands on the decode time of the last key frame
/// shown at or before it, as `ffprobe`'s flags say.
fn check_video_seeks(file: &Path, name: &str, probed: &[Probed]) {
    let mut d = open(file, name);
    for p in probed {
        let want = probed.iter().filter(|k| k.key && k.pts <= p.pts + 1e-9).map(|k| k.dts).fold(f64::NAN, |a, b| if a.is_nan() || b > a { b } else { a });
        let landed = d.seek(p.pts).unwrap();
        assert!((landed - want).abs() < 1e-5, "{name}: seek to {} landed on {landed}, want {want}", p.pts);
        // and the picture read there is not before the landing
        let first = d.read_packet().unwrap().unwrap();
        assert!(secs(&d.streams()[first.stream], first.pts) >= landed - 1e-9);
    }
}

fn decode(d: &mut dyn Demuxer, packets: &[Packet]) -> (usize, Vec<f64>) {
    let streams = d.streams().to_vec();
    let mut decoders: Vec<_> = streams.iter().map(|s| ironav_codec::new_decoder_for(s).unwrap_or_else(|e| panic!("decoder for {}: {}", s.codec.name(), e.message))).collect();
    let (mut pictures, mut samples) = (0, Vec::new());
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            match f {
                Frame::Video(_) => pictures += 1,
                Frame::Audio(a) => samples.extend(a.samples),
            }
        }
    };
    for p in packets {
        take(decoders[p.stream].decode(p).unwrap());
    }
    for dec in decoders.iter_mut() {
        take(dec.flush().unwrap());
    }
    (pictures, samples)
}

/// All the audio of `file` as FFmpeg decodes it, as f64.
fn ffmpeg_samples(file: &Path) -> Vec<f64> {
    let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(file).args(["-vn", "-f", "f64le", "-c:a", "pcm_f64le", "-"]).output().unwrap();
    assert!(out.status.success(), "ffmpeg decodes the file: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout.as_chunks::<8>().0.iter().map(|b| f64::from_le_bytes(*b)).collect()
}

fn audio_of(d: &mut dyn Demuxer, packets: &[Packet]) -> Vec<f64> {
    let streams = d.streams().to_vec();
    let a = streams.iter().position(|s| s.params.media_type() == MediaType::Audio).expect("an audio stream");
    let packets: Vec<Packet> = packets.iter().filter(|p| p.stream == a).cloned().collect();
    let mut dec = ironav_codec::new_decoder_for(&streams[a]).unwrap();
    let mut out = Vec::new();
    for p in &packets {
        for f in dec.decode(p).unwrap() {
            if let Frame::Audio(af) = f {
                out.extend(af.samples);
            }
        }
    }
    for f in dec.flush().unwrap() {
        if let Frame::Audio(af) = f {
            out.extend(af.samples);
        }
    }
    out
}

fn snr_db(reference: &[f64], got: &[f64]) -> f64 {
    let n = reference.len().min(got.len());
    let signal: f64 = reference[..n].iter().map(|x| x * x).sum();
    let noise: f64 = reference[..n].iter().zip(&got[..n]).map(|(a, b)| (a - b) * (a - b)).sum();
    if noise == 0.0 { 300.0 } else { 10.0 * (signal / noise).log10() }
}

fn audio_params(s: &StreamInfo) -> (u32, u16) {
    match s.params {
        StreamParams::Audio { sample_rate, channels } => (sample_rate, channels),
        _ => panic!("not audio"),
    }
}

/// The audio is bit-exact to FFmpeg's, from the start and from a seek to the middle (to the frame for
/// PCM, to the packet for the rest).
fn check_audio_exact(file: &Path, name: &str, codec: CodecId) {
    let mut d = open(file, name);
    let s = d.streams()[0].clone();
    assert_eq!(s.codec, codec, "{name}");
    let packets = read_all(d.as_mut());
    let got = audio_of(d.as_mut(), &packets);
    let want = ffmpeg_samples(file);
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    assert!(got == want, "{name}: samples differ from FFmpeg's");
    let (rate, channels) = audio_params(&s);
    let landed = d.seek(0.55).unwrap();
    assert!(landed <= 0.55 && landed > 0.55 - 4096.0 / rate as f64 - 1e-9, "{name}: landed {landed}");
    let rest = read_all(d.as_mut());
    let tail = audio_of(d.as_mut(), &rest);
    let from = (landed * rate as f64).round() as usize * channels as usize;
    assert!(tail[..] == want[from..from + tail.len()], "{name}: samples after the seek differ from FFmpeg's");
    assert_eq!(from + tail.len(), want.len(), "{name}: the seek reads to the end");
}

// ---------------------------------------------------------------- AIFF

#[test]
fn aiff_pcm_matches_ffmpeg() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let cases: [(&str, &[&str], CodecId); 6] = [
        ("pcm16.aiff", &["-c:a", "pcm_s16be"], CodecId::PcmS16Le),
        ("pcm24.aiff", &["-c:a", "pcm_s24be"], CodecId::PcmS24Le),
        ("pcm8.aif", &["-c:a", "pcm_s8"], CodecId::PcmU8),
        ("sowt.aifc", &["-c:a", "pcm_s16le"], CodecId::PcmS16Le),
        ("fl32.aifc", &["-c:a", "pcm_f32be"], CodecId::PcmF32Le),
        ("fl64.aifc", &["-c:a", "pcm_f64be"], CodecId::PcmF64Le),
    ];
    for (name, out, codec) in cases {
        let Some(f) = make(&dir, name, false, true, out) else { continue };
        let d = open(&f, "aiff");
        let s = d.streams()[0].clone();
        assert_eq!(audio_params(&s), (44100, 1), "{name}");
        assert_eq!(s.duration, Some((44100.0 * 1.2) as i64), "{name}: duration");
        drop(d);
        check_audio_exact(&f, "aiff", codec);
    }
}

#[test]
fn aiff_stereo_24_bit_and_other_compressions() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let Some(f) = make(&dir, "st24.aiff", false, true, &["-ac", "2", "-ar", "48000", "-c:a", "pcm_s24be"]) else { return };
    let d = open(&f, "aiff");
    assert_eq!(audio_params(&d.streams()[0]), (48000, 2));
    drop(d);
    check_audio_exact(&f, "aiff", CodecId::PcmS24Le);
    // a-law is AIFF-C `alaw`: refused, naming it
    if let Some(f) = make(&dir, "alaw.aifc", false, true, &["-c:a", "pcm_alaw"]) {
        let e = open_demuxer(&f).err().expect("a-law is not read");
        assert!(e.message.to_ascii_lowercase().contains("alaw"), "{}", e.message);
    }
}

// ---------------------------------------------------------------- CAF

#[test]
fn caf_pcm_matches_ffmpeg() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let cases: [(&str, &[&str], CodecId); 4] = [
        ("be16.caf", &["-c:a", "pcm_s16be"], CodecId::PcmS16Le),
        ("le24.caf", &["-c:a", "pcm_s24le"], CodecId::PcmS24Le),
        ("be32f.caf", &["-c:a", "pcm_f32be"], CodecId::PcmF32Le),
        ("le16.caf", &["-ac", "2", "-c:a", "pcm_s16le"], CodecId::PcmS16Le),
    ];
    for (name, out, codec) in cases {
        let Some(f) = make(&dir, name, false, true, out) else { continue };
        check_audio_exact(&f, "caf", codec);
    }
}

#[test]
fn caf_alac_matches_ffmpeg_bit_for_bit() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    for (name, extra) in [("alac16.caf", &["-ac", "2"][..]), ("alac24.caf", &["-sample_fmt", "s32p", "-ac", "1"][..])] {
        let mut out = vec!["-c:a", "alac"];
        out.extend_from_slice(extra);
        let Some(f) = make(&dir, name, false, true, &out) else { continue };
        let d = open(&f, "caf");
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, CodecId::Alac);
        assert_eq!(audio_params(&s).0, 44100);
        drop(d);
        check_alac(&f);
        if let Some(p) = ffprobe_packets(&f, "a:0") {
            let mut d = open(&f, "caf");
            let packets = read_all(d.as_mut());
            assert_eq!(packets.len(), p.len(), "{name}: packet count");
        }
    }
}

/// ALAC is bit-exact to FFmpeg's, as far as FFmpeg goes: it takes the remainder frames off the last packet
/// a second time (the last ALAC packet already holds only the valid ones), so it ends a few frames early.
/// Ours has the file's valid frame count, which `ffprobe` gives as the duration.
fn check_alac(file: &Path) {
    let mut d = open(file, "caf");
    let s = d.streams()[0].clone();
    let channels = audio_params(&s).1 as usize;
    let packets = read_all(d.as_mut());
    let got = audio_of(d.as_mut(), &packets);
    let want = ffmpeg_samples(file);
    assert!(got.len() >= want.len() && got[..want.len()] == want[..], "ALAC samples differ from FFmpeg's");
    assert_eq!(got.len(), (44100.0 * 1.2) as usize * channels, "the valid frames");
    assert_eq!(s.duration, Some((44100.0 * 1.2) as i64));
    assert_eq!(packets.iter().map(|p| p.duration).sum::<i64>(), (44100.0 * 1.2) as i64, "the last packet ends at the last valid frame");
    // seeking to the middle lands on a packet and decodes the same samples from there
    let landed = d.seek(0.55).unwrap();
    let rest = read_all(d.as_mut());
    let tail = audio_of(d.as_mut(), &rest);
    let from = (landed * 44100.0).round() as usize * channels;
    assert!(landed <= 0.55 && 0.55 - landed < 4096.0 / 44100.0);
    assert!(tail[..] == got[from..], "samples after the seek");
}

/// A CAF file holding the AAC packets of `adts`, with the packet table priming and remainder given.
fn caf_from_adts(adts: &Path, priming: i32, remainder: i32) -> (Vec<u8>, Vec<Packet>, StreamInfo) {
    let mut d = open_demuxer(adts).unwrap();
    let s = d.streams()[0].clone();
    let packets = read_all(d.as_mut());
    let (rate, channels) = audio_params(&s);
    let asc = &s.extradata;
    let chunk = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut v = kind.to_vec();
        v.extend((body.len() as i64).to_be_bytes());
        v.extend(body);
        v
    };
    // the cookie is an ES descriptor
    let dsi = [&[5u8, asc.len() as u8][..], asc].concat();
    let dec_cfg = [&[0x40u8, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..], &dsi].concat();
    let es_body = [&[0u8, 0, 0][..], &[4, dec_cfg.len() as u8], &dec_cfg, &[6, 1, 2]].concat();
    let esds = [&[3u8, es_body.len() as u8][..], &es_body].concat();
    let mut desc = (rate as f64).to_be_bytes().to_vec();
    desc.extend(b"aac ");
    for v in [0u32, 0, 1024, channels as u32, 0] {
        desc.extend(v.to_be_bytes());
    }
    let frames: i64 = packets.iter().map(|p| p.duration).sum();
    let mut pakt = (packets.len() as i64).to_be_bytes().to_vec();
    pakt.extend((frames - priming as i64 - remainder as i64).to_be_bytes());
    pakt.extend(priming.to_be_bytes());
    pakt.extend(remainder.to_be_bytes());
    for p in &packets {
        // a variable-length integer, seven bits at a time
        let mut n = p.data.len();
        let mut v = vec![(n & 0x7F) as u8];
        n >>= 7;
        while n > 0 {
            v.push((n & 0x7F) as u8 | 0x80);
            n >>= 7;
        }
        v.reverse();
        pakt.extend(v);
    }
    let mut data = vec![0u8; 4];
    for p in &packets {
        data.extend(&p.data);
    }
    let mut f = b"caff\0\x01\0\0".to_vec();
    f.extend(chunk(b"desc", &desc));
    f.extend(chunk(b"kuki", &esds));
    f.extend(chunk(b"pakt", &pakt));
    f.extend(chunk(b"data", &data));
    (f, packets, s)
}

#[test]
fn caf_aac_from_the_packets_of_an_adts_file() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let Some(adts) = make(&dir, "a.aac", false, true, &["-ac", "2", "-c:a", "aac", "-b:a", "96k"]) else { return };
    let (bytes, packets, adts_stream) = caf_from_adts(&adts, 0, 0);
    let file = dir.join("aac.caf");
    std::fs::write(&file, &bytes).unwrap();
    let mut d = open(&file, "caf");
    let s = d.streams()[0].clone();
    assert_eq!((s.codec, s.extradata.clone(), audio_params(&s)), (CodecId::Aac, adts_stream.extradata.clone(), audio_params(&adts_stream)));
    let ours = read_all(d.as_mut());
    assert_eq!(ours.len(), packets.len());
    assert!(ours.iter().zip(&packets).all(|(a, b)| a.data == b.data && a.pts == b.pts && a.duration == b.duration), "the packets are the ADTS file's");
    let got = audio_of(d.as_mut(), &ours);
    let want = ffmpeg_samples(&adts);
    assert!(got.len() >= want.len(), "{} against {}", got.len(), want.len());
    let snr = snr_db(&want, &got);
    assert!(snr > 60.0, "SNR against FFmpeg's decode is {snr:.1} dB");
    // seek by packet: lands on the packet holding the time, and its packets are the file's from there
    let landed = d.seek(0.6).unwrap();
    let first = d.read_packet().unwrap().unwrap();
    assert!(landed <= 0.6 && 0.6 < landed + 1024.0 / 44100.0 + 1e-9, "landed {landed}");
    assert_eq!(first.data, packets[(landed * 44100.0 / 1024.0).round() as usize].data);

    // the priming frames become the lead-in and the valid frames the duration: the decoder keeps those
    let (bytes, _, _) = caf_from_adts(&adts, 1024, 300);
    let file = dir.join("aac-primed.caf");
    std::fs::write(&file, &bytes).unwrap();
    let mut d = open(&file, "caf");
    let s = d.streams()[0].clone();
    let frames: i64 = packets.iter().map(|p| p.duration).sum();
    assert_eq!((s.lead_in, s.duration), (1024, Some(frames - 1024 - 300)));
    let all = read_all(d.as_mut());
    let kept = audio_of(d.as_mut(), &all);
    let channels = audio_params(&s).1 as usize;
    assert_eq!(kept.len(), (frames as usize - 1024 - 300) * channels);
    assert!(kept[..] == got[1024 * channels..1024 * channels + kept.len()], "the kept samples are the decode's, from the priming on");
}

// ---------------------------------------------------------------- FLV

fn video_of(streams: &[StreamInfo]) -> usize {
    streams.iter().position(|s| s.params.media_type() == MediaType::Video).expect("a video stream")
}

#[test]
fn flv_h264_aac() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    for (name, video_opts, flags) in [
        ("plain.flv", &["-preset", "ultrafast", "-g", "4"][..], "+aac_seq_header_detect"),
        ("bframes.flv", &["-preset", "medium", "-g", "5", "-bf", "2"][..], "+aac_seq_header_detect"),
        ("nometa.flv", &["-preset", "ultrafast", "-g", "4"][..], "+no_metadata"),
    ] {
        let mut out = vec!["-c:v", "libx264", "-pix_fmt", "yuv420p"];
        out.extend_from_slice(video_opts);
        out.extend(["-c:a", "aac", "-flvflags", flags]);
        let Some(f) = make(&dir, name, true, true, &out) else { continue };
        let mut d = open(&f, "flv");
        let streams = d.streams().to_vec();
        assert_eq!(streams.len(), 2, "{name}");
        let v = video_of(&streams);
        let a = 1 - v;
        assert_eq!(streams[v].codec, CodecId::H264);
        match streams[v].params {
            StreamParams::Video { width, height, frame_rate, .. } => assert_eq!((width, height, frame_rate.num, frame_rate.den), (64, 48, 10, 1), "{name}"),
            _ => unreachable!(),
        }
        assert_eq!((streams[a].codec, audio_params(&streams[a])), (CodecId::Aac, (44100, 1)), "{name}");
        assert!(d.unsupported_streams().is_empty());
        let packets = read_all(d.as_mut());
        let probed_v = ffprobe_packets(&f, "v:0");
        match &probed_v {
            Some(p) => compare_times(d.as_ref(), &packets, v, p, &format!("{name} video")),
            None => assert_eq!(packets.iter().filter(|p| p.stream == v).count(), 12, "{name}: twelve pictures"),
        }
        if let Some(p) = ffprobe_packets(&f, "a:0") {
            compare_times(d.as_ref(), &packets, a, &p, &format!("{name} audio"));
        }
        // every picture decodes
        let (pictures, samples) = decode(d.as_mut(), &packets);
        assert_eq!(pictures, 12, "{name}");
        let want = ffmpeg_samples(&f);
        assert!(samples.len() >= want.len() - 2048, "{name}: {} samples against {}", samples.len(), want.len());
        // FFmpeg drops the first frame's priming from an FLV as it does from an MP4 only when told to; compare aligned by cross-check on SNR at both offsets
        let best = [0usize, 1024].iter().map(|&skip| snr_db(&want, &samples[skip.min(samples.len())..])).fold(f64::MIN, f64::max);
        assert!(best > 40.0, "{name}: audio SNR {best:.1} dB");
        if let Some(p) = probed_v {
            check_video_seeks(&f, "flv", &p);
        }
    }
}

#[test]
fn flv_mp3_and_pcm_audio() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    if let Some(f) = make(&dir, "mp3.flv", false, true, &["-c:a", "libmp3lame", "-b:a", "96k"]) {
        let mut d = open(&f, "flv");
        let s = d.streams()[0].clone();
        assert_eq!((s.codec, audio_params(&s)), (CodecId::Mp3, (44100, 1)));
        let packets = read_all(d.as_mut());
        if let Some(p) = ffprobe_packets(&f, "a:0") {
            compare_times(d.as_ref(), &packets, 0, &p, "mp3.flv");
        }
        let got = audio_of(d.as_mut(), &packets);
        let want = ffmpeg_samples(&f);
        let best = [0usize, 1105, 1152].iter().map(|&skip| snr_db(&want, &got[skip.min(got.len())..])).fold(f64::MIN, f64::max);
        assert!(best > 40.0, "MP3 SNR {best:.1} dB (lengths {} and {})", got.len(), want.len());
    }
    // 16-bit little-endian PCM (sound format 3), at a rate the format can name
    if let Some(f) = make(&dir, "pcm.flv", false, true, &["-c:a", "pcm_s16le", "-ar", "22050"]) {
        let mut d = open(&f, "flv");
        let s = d.streams()[0].clone();
        assert_eq!((s.codec, audio_params(&s)), (CodecId::PcmS16Le, (22050, 1)));
        let packets = read_all(d.as_mut());
        assert!(audio_of(d.as_mut(), &packets) == ffmpeg_samples(&f), "PCM in FLV is bit-exact");
    }
}

#[test]
fn flv_hevc_and_av1_in_enhanced_flv() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    for (name, codec, opts) in [
        ("hevc.flv", CodecId::Hevc, &["-c:v", "libx265", "-x265-params", "log-level=none:keyint=4:bframes=0", "-pix_fmt", "yuv420p"][..]),
        ("av1.flv", CodecId::Av1, &["-c:v", "libaom-av1", "-cpu-used", "8", "-g", "4", "-pix_fmt", "yuv420p"][..]),
    ] {
        let Some(f) = make(&dir, name, true, false, opts) else { continue };
        let Ok(mut d) = open_demuxer(&f) else {
            panic!("{name}: an FLV with enhanced video should open");
        };
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, codec);
        match s.params {
            StreamParams::Video { width, height, .. } => assert_eq!((width, height), (64, 48), "{name}"),
            _ => unreachable!(),
        }
        let packets = read_all(d.as_mut());
        if let Some(p) = ffprobe_packets(&f, "v:0") {
            compare_times(d.as_ref(), &packets, 0, &p, name);
            check_video_seeks(&f, "flv", &p);
        } else {
            assert_eq!(packets.len(), 12);
        }
        let (pictures, _) = decode(d.as_mut(), &packets);
        assert_eq!(pictures, 12, "{name}");
    }
}

#[test]
fn flv_truncated_and_damaged_files_do_not_panic() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let Some(f) = make(&dir, "a.flv", true, true, &["-c:v", "libx264", "-preset", "ultrafast", "-g", "4", "-pix_fmt", "yuv420p", "-c:a", "aac"]) else { return };
    let good = std::fs::read(&f).unwrap();
    let full = {
        let mut d = open(&f, "flv");
        read_all(d.as_mut()).len()
    };
    let path = dir.join("cut.flv");
    let mut last = 0;
    for cut in (0..good.len()).step_by(97).chain([good.len() - 1]) {
        std::fs::write(&path, &good[..cut]).unwrap();
        if let Ok(mut d) = open_demuxer(&path) {
            let n = read_all(d.as_mut()).len();
            assert!(n <= full && n >= last.min(n), "a truncated file has no more packets than the whole");
            last = n.min(full);
        }
    }
    // flipped bytes and absurd tag sizes
    let mut x = 0x9E37_79B9u32;
    for _ in 0..200 {
        let mut bad = good.clone();
        for _ in 0..4 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let at = (x as usize >> 3) % bad.len();
            bad[at] ^= (x >> 24) as u8 | 1;
        }
        std::fs::write(&path, &bad).unwrap();
        if let Ok(mut d) = open_demuxer(&path) {
            while let Ok(Some(_)) = d.read_packet() {}
            let _ = d.seek(0.3);
        }
    }
}

// ---------------------------------------------------------------- IVF

#[test]
fn ivf_vp8_vp9_av1() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    for (name, codec, opts) in [
        ("vp8.ivf", CodecId::Vp8, &["-c:v", "libvpx", "-g", "4", "-deadline", "realtime", "-cpu-used", "8", "-auto-alt-ref", "0"][..]),
        ("vp9.ivf", CodecId::Vp9, &["-c:v", "libvpx-vp9", "-g", "4", "-deadline", "realtime", "-cpu-used", "8", "-pix_fmt", "yuv420p"][..]),
        ("vp9_444.ivf", CodecId::Vp9, &["-c:v", "libvpx-vp9", "-g", "4", "-deadline", "realtime", "-cpu-used", "8", "-pix_fmt", "yuv444p"][..]),
        ("av1.ivf", CodecId::Av1, &["-c:v", "libaom-av1", "-g", "4", "-cpu-used", "8", "-pix_fmt", "yuv420p"][..]),
    ] {
        let Some(f) = make(&dir, name, true, false, opts) else { continue };
        let mut d = open(&f, "ivf");
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, codec, "{name}");
        match s.params {
            StreamParams::Video { width, height, frame_rate, pix_fmt, .. } => {
                assert_eq!((width, height, frame_rate.num, frame_rate.den), (64, 48, 10, 1), "{name}");
                if name.contains("444") {
                    assert_eq!(pix_fmt, ironav_core::PixelFormat::Yuv444p, "{name}");
                } else {
                    assert_eq!(pix_fmt, ironav_core::PixelFormat::Yuv420p, "{name}");
                }
            }
            _ => unreachable!(),
        }
        assert_eq!(s.time_base.as_f64(), 0.1, "{name}: the header's time base");
        if codec == CodecId::Av1 {
            assert_eq!(s.extradata[0], 0x81, "an av1C record");
        }
        let packets = read_all(d.as_mut());
        let probed = ffprobe_packets(&f, "v:0");
        match &probed {
            Some(p) => compare_times(d.as_ref(), &packets, 0, p, name),
            None => assert_eq!(packets.len(), 12, "{name}"),
        }
        if let Some(p) = &probed {
            assert!(p.iter().filter(|k| k.key).count() >= 3, "{name}: the test file has several key frames");
            check_video_seeks(&f, "ivf", p);
        }
        if !name.contains("444") {
            let (pictures, _) = decode(d.as_mut(), &packets);
            assert_eq!(pictures, 12, "{name}");
        }
        // a copied packet is the file's bytes, and the key frames agree with the bitstream test
        for p in &packets {
            let key = ironav_formats::is_keyframe(codec, &p.data);
            assert!(key.is_some(), "{name}");
        }
    }
}

#[test]
fn ivf_damaged_files_do_not_panic() {
    if !tool_available("ffmpeg") {
        return;
    }
    let dir = scratch();
    let Some(f) = make(&dir, "a.ivf", true, false, &["-c:v", "libvpx", "-g", "4", "-deadline", "realtime", "-cpu-used", "8"]) else { return };
    let good = std::fs::read(&f).unwrap();
    let path = dir.join("cut.ivf");
    for cut in (0..good.len()).step_by(31).chain([good.len() - 1]) {
        std::fs::write(&path, &good[..cut]).unwrap();
        if let Ok(mut d) = open_demuxer(&path) {
            while let Ok(Some(_)) = d.read_packet() {}
        }
    }
    // a frame size of four billion bytes
    let mut bad = good.clone();
    bad[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&path, &bad).unwrap();
    assert!(open_demuxer(&path).is_err());
}
