//! TrueHD and MLP in their containers. Two short stereo streams made with FFmpeg's encoders (the
//! fixtures `data/tone_stereo.thd`, 21 KB, TrueHD at 24 bits, and `data/tone_stereo.mlp`, 22 KB,
//! MLP at 16 bits, both 48 kHz) are read with `open_demuxer` as raw files and decode to the exact
//! samples FFmpeg decodes (a hash of them is pinned, so those tests need no FFmpeg). Muxed by FFmpeg
//! into Matroska, MP4 and MPEG-TS and M2TS (made at test time, so those are skipped without `ffmpeg`
//! on PATH), and written to Matroska by IronAV, they decode to the same samples. A Blu-ray style
//! transport stream, with the AC-3 packets that HDMV puts on the TrueHD PID, is built by the test.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamInfo, StreamParams};
use ironav_formats::{create_muxer, open_demuxer};
use std::path::{Path, PathBuf};
use std::process::Command;

const THD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tone_stereo.thd");
const MLP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tone_stereo.mlp");
/// FNV-1a of FFmpeg's decode of each fixture: 32-bit little-endian samples (TrueHD) and 16-bit (MLP).
const THD_HASH: u64 = 0x54b6894db2037d88;
const MLP_HASH: u64 = 0x16cd664d9c212869;
/// Samples per channel: 100 ms and 200 ms at 48 kHz.
const THD_FRAMES: usize = 4800;
const MLP_FRAMES: usize = 9600;

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("truehd-container-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fnv(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// The bytes of the samples: little-endian 32-bit for TrueHD, 16-bit for MLP (a 16-bit stream).
fn sample_bytes(samples: &[i32], truehd: bool) -> Vec<u8> {
    if truehd {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    } else {
        samples.iter().flat_map(|&s| (s as i16).to_le_bytes()).collect()
    }
}

/// Demux `file` with the formats registry and decode it: the integers (32-bit scale for TrueHD,
/// 16-bit for MLP), the stream's rate and channels.
fn demux_and_decode(file: &Path, codec: CodecId) -> (Vec<i32>, u32, u16) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    assert_eq!(d.streams().len(), 1, "{file:?}");
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, codec);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("a decoder");
    let scale = if codec == CodecId::TrueHd { 2147483648.0 } else { 32768.0 };
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        let p = Packet { stream: 0, ..p };
        for f in dec.decode(&p).expect("a decoded frame") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (sample_rate, channels));
            out.extend(a.samples.iter().map(|&x| (x * scale) as i32));
        }
    }
    (out, sample_rate, channels)
}

/// What FFmpeg decodes `file` to (the first audio stream), as 32-bit or 16-bit integers.
fn reference(dir: &Path, name: &str, file: &Path, truehd: bool) -> Vec<i32> {
    let out = dir.join(format!("{name}.raw"));
    let (fmt, codec) = if truehd { ("s32le", "pcm_s32le") } else { ("s16le", "pcm_s16le") };
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-map", "0:a:0", "-f", fmt, "-acodec", codec, out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    let b = std::fs::read(&out).unwrap();
    if truehd {
        b.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c)).collect()
    } else {
        b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c) as i32).collect()
    }
}

/// The raw fixture's packets: each is an access unit.
fn packets(file: &Path) -> (Vec<Packet>, StreamInfo) {
    let mut d = open_demuxer(file).unwrap();
    let stream = d.streams()[0].clone();
    let mut v = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        v.push(p);
    }
    (v, stream)
}

#[test]
fn raw_truehd_opens_and_decodes_to_ffmpegs_samples() {
    let d = open_demuxer(Path::new(THD)).expect("the fixture opens");
    assert_eq!(d.format_name(), "truehd");
    let s = &d.streams()[0];
    assert_eq!((s.codec, s.time_base.den, s.duration), (CodecId::TrueHd, 48000, Some(THD_FRAMES as i64)));
    assert!(matches!(s.params, StreamParams::Audio { sample_rate: 48000, channels: 2 }));
    let (pkts, _) = packets(Path::new(THD));
    assert_eq!(pkts.len(), THD_FRAMES / 40, "one packet per access unit of 40 samples");
    let (samples, rate, ch) = demux_and_decode(Path::new(THD), CodecId::TrueHd);
    assert_eq!((rate, ch, samples.len()), (48000, 2, THD_FRAMES * 2));
    assert_eq!(fnv(sample_bytes(&samples, true).into_iter()), THD_HASH, "the samples FFmpeg decodes");
}

#[test]
fn raw_mlp_opens_and_decodes_to_ffmpegs_samples() {
    let d = open_demuxer(Path::new(MLP)).expect("the fixture opens");
    assert_eq!(d.format_name(), "mlp");
    let s = &d.streams()[0];
    assert_eq!((s.codec, s.time_base.den, s.duration), (CodecId::Mlp, 48000, Some(MLP_FRAMES as i64)));
    let (samples, rate, ch) = demux_and_decode(Path::new(MLP), CodecId::Mlp);
    assert_eq!((rate, ch, samples.len()), (48000, 2, MLP_FRAMES * 2));
    assert_eq!(fnv(sample_bytes(&samples, false).into_iter()), MLP_HASH, "the samples FFmpeg decodes");
}

/// A decode of the fixture lined up with what FFmpeg makes of `file`.
fn same_as_ffmpeg(dir: &Path, name: &str, file: &Path, codec: CodecId) {
    let truehd = codec == CodecId::TrueHd;
    let want = reference(dir, name, file, truehd);
    let (got, rate, ch) = demux_and_decode(file, codec);
    assert_eq!((rate, ch), (48000, 2), "{name}: stream");
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let same = got == want;
    eprintln!("{name:>14}: {} samples, IronAV == FFmpeg: {same}", got.len());
    assert!(same, "{name}: the decoded samples differ from FFmpeg's");
    let pinned = if truehd { THD_HASH } else { MLP_HASH };
    assert_eq!(fnv(sample_bytes(&got, truehd).into_iter()), pinned, "{name}: and so from the raw file's");
}

/// Mux a fixture with FFmpeg into `name` and compare. `decode_from` is the file FFmpeg decodes for the
/// reference: the muxed file, or the fixture where FFmpeg cannot read its own output.
fn mux_and_compare(dir: &Path, fixture: &str, name: &str, codec: CodecId, args: &[&str], ffmpeg_reads_it: bool) {
    let file = dir.join(name);
    let mut a = vec!["-i", fixture, "-c", "copy"];
    a.extend_from_slice(args);
    a.push(file.to_str().unwrap());
    assert!(ffmpeg(&a), "muxing {name}");
    let truehd = codec == CodecId::TrueHd;
    let want = reference(dir, name, if ffmpeg_reads_it { &file } else { Path::new(fixture) }, truehd);
    let (got, rate, ch) = demux_and_decode(&file, codec);
    assert_eq!((rate, ch), (48000, 2), "{name}: stream");
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let same = got == want;
    eprintln!("{name:>14}: {} samples, IronAV == FFmpeg: {same}", got.len());
    assert!(same, "{name}: the decoded samples differ from FFmpeg's");
}

#[test]
fn matroska_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    mux_and_compare(&dir, THD, "thd.mkv", CodecId::TrueHd, &["-strict", "-2"], true);
    mux_and_compare(&dir, MLP, "mlp.mkv", CodecId::Mlp, &["-strict", "-2"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mp4_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    // FFmpeg writes TrueHD as `mlpa` with a `dmlp` box (and only TrueHD: MP4 has no MLP entry)
    mux_and_compare(&dir, THD, "thd.mp4", CodecId::TrueHd, &["-strict", "-2"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpegts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    // FFmpeg's muxer writes stream type 0x83 for TrueHD, but its reader takes that type only in a
    // program with the HDMV registration descriptor, so it cannot read these: the reference is its
    // decode of the raw fixture. With `-mpegts_m2ts_mode 1` the packets are 192 bytes.
    mux_and_compare(&dir, THD, "thd.ts", CodecId::TrueHd, &["-strict", "-2", "-f", "mpegts"], false);
    mux_and_compare(&dir, THD, "thd.m2ts", CodecId::TrueHd, &["-strict", "-2", "-f", "mpegts", "-mpegts_m2ts_mode", "1"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What IronAV's Matroska writer makes of the raw fixture's packets (as `A_TRUEHD` and `A_MLP`).
#[test]
fn the_matroska_writer_keeps_the_packets() {
    let dir = scratch();
    for (fixture, codec, name) in [(THD, CodecId::TrueHd, "ours-thd.mkv"), (MLP, CodecId::Mlp, "ours-mlp.mkv")] {
        let (pkts, stream) = packets(Path::new(fixture));
        let file = dir.join(name);
        let mut m = create_muxer(&file, "mkv").expect("a Matroska writer");
        m.write_header(std::slice::from_ref(&stream)).unwrap();
        for p in &pkts {
            m.write_packet(p).unwrap();
        }
        m.finish().unwrap();
        let (got, _, _) = demux_and_decode(&file, codec);
        let (raw, _, _) = demux_and_decode(Path::new(fixture), codec);
        assert_eq!(got, raw, "{name}");
        if ffmpeg_available() {
            same_as_ffmpeg(&dir, name, &file, codec);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn crc32(b: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &x in b {
        crc ^= (x as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// TS packets of one PID carrying `payload` as one PES or section, `pusi` set on the first.
fn ts_packets(pid: u16, cc: &mut u8, payload: &[u8], out: &mut Vec<u8>, m2ts: bool) {
    let mut at = 0;
    while at < payload.len() {
        let room = 184;
        let left = payload.len() - at;
        let mut p = vec![0x47, ((at == 0) as u8) << 6 | (pid >> 8) as u8, pid as u8];
        if left >= room {
            p.push(0x10 | *cc);
            p.extend_from_slice(&payload[at..at + room]);
            at += room;
        } else {
            // stuff the last packet with an adaptation field
            p.push(0x30 | *cc);
            let stuff = room - left;
            p.push(stuff as u8 - 1);
            if stuff > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuff - 2));
            }
            p.extend_from_slice(&payload[at..]);
            at = payload.len();
        }
        *cc = (*cc + 1) & 15;
        assert_eq!(p.len(), 188);
        if m2ts {
            out.extend([0, 0, 0, 0]);
        }
        out.extend(p);
    }
}

fn section(table: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let mut s = vec![table, 0xB0 | ((body.len() + 9) >> 8) as u8, (body.len() + 9) as u8, (id >> 8) as u8, id as u8, 0xC1, 0, 0];
    s.extend_from_slice(body);
    let c = crc32(&s);
    s.extend(c.to_be_bytes());
    s.insert(0, 0); // pointer field
    s
}

/// A PES packet of an extended stream id (0xFD): `ext` names what it holds (0x72 TrueHD, 0x76 AC-3).
fn pes(ext: u8, pts: i64, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0, 1, 0xFD];
    let len = 3 + 8 + payload.len();
    p.extend((len as u16).to_be_bytes());
    p.extend([0x81, 0x81, 8]);
    p.extend([0x21 | ((pts >> 29) & 0xE) as u8, (pts >> 22) as u8, 1 | ((pts >> 14) & 0xFE) as u8, (pts >> 7) as u8, 1 | ((pts << 1) & 0xFE) as u8]);
    p.extend([0x01, 0x81, ext]); // PES extension 2, one byte long, with the extended stream id
    p.extend_from_slice(payload);
    p
}

/// The raw fixture as a Blu-ray style transport stream: the registration descriptor `HDMV`, stream type
/// 0x83 on one PID, and each pair of TrueHD access units followed by a PES of a stand-in AC-3 frame.
fn bluray_ts(m2ts: bool) -> Vec<u8> {
    let (pkts, _) = packets(Path::new(THD));
    let pid = 0x1100u16;
    let mut out = Vec::new();
    let (mut cc_pat, mut cc_pmt, mut cc) = (0u8, 0u8, 0u8);
    let psi = |out: &mut Vec<u8>, cc_pat: &mut u8, cc_pmt: &mut u8| {
        ts_packets(0, cc_pat, &section(0, 1, &[0, 1, 0xE1, 0x00]), out, m2ts);
        let mut pmt = vec![0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 6, 5, 4, b'H', b'D', b'M', b'V'];
        pmt.extend([0x83, 0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 0]);
        ts_packets(0x100, cc_pmt, &section(2, 1, &pmt), out, m2ts);
    };
    let mut t = 90_000i64;
    for pair in pkts.chunks(2) {
        psi(&mut out, &mut cc_pat, &mut cc_pmt);
        let data: Vec<u8> = pair.iter().flat_map(|p| p.data.clone()).collect();
        ts_packets(pid, &mut cc, &pes(0x72, t, &data), &mut out, m2ts);
        let mut ac3 = vec![0x0B, 0x77];
        ac3.extend((0..300).map(|i| (i * 7 + 3) as u8));
        ts_packets(pid, &mut cc, &pes(0x76, t, &ac3), &mut out, m2ts);
        t += 750 * pair.len() as i64;
    }
    out
}

#[test]
fn bluray_truehd_in_transport_streams_drops_the_ac3_packets() {
    let dir = scratch();
    let (raw, _, _) = demux_and_decode(Path::new(THD), CodecId::TrueHd);
    for (name, m2ts) in [("bd.ts", false), ("bd.m2ts", true)] {
        let file = dir.join(name);
        std::fs::write(&file, bluray_ts(m2ts)).unwrap();
        let (got, rate, ch) = demux_and_decode(&file, CodecId::TrueHd);
        assert_eq!((rate, ch), (48000, 2), "{name}");
        assert_eq!(got.len(), raw.len(), "{name}: sample count");
        assert!(got == raw, "{name}: the decoded samples differ from the raw file's");
        eprintln!("{name:>14}: {} samples, equal to the raw file's", got.len());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seeking_lands_on_a_major_sync_and_decodes() {
    let mut d = open_demuxer(Path::new(THD)).unwrap();
    let stream = d.streams()[0].clone();
    let landed = d.seek(0.07).expect("raw TrueHD seeks");
    // the major syncs are 16 access units (13 ms) apart in FFmpeg's encoder
    assert!((0.0..=0.07).contains(&landed), "landed at {landed}");
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    let mut n = 0;
    while let Some(p) = d.read_packet().unwrap() {
        for f in dec.decode(&Packet { stream: 0, ..p }).expect("decodes from the major sync") {
            let Frame::Audio(a) = f else { panic!() };
            n += a.frames();
        }
    }
    assert_eq!(n, THD_FRAMES - (landed * 48000.0).round() as usize);
}

#[test]
fn damaged_files_open_or_fail_without_panicking() {
    let dir = scratch();
    let raw = std::fs::read(THD).unwrap();
    let file = dir.join("damaged.thd");
    let mut x = 99u32;
    for cut in [0usize, 3, 7, 8, 31, 33, 100, 1000, raw.len() / 2, raw.len() - 1] {
        std::fs::write(&file, &raw[..cut]).unwrap();
        let _ = open_demuxer(&file).map(|mut d| while let Ok(Some(_)) = d.read_packet() {});
    }
    // garbage in front, in the middle, and noise over the first access unit's body
    let mut with_junk = vec![0xAAu8; 77];
    with_junk.extend_from_slice(&raw);
    for i in 0..400 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        let at = 200 + (x >> 8) as usize % (raw.len() - 400);
        if i % 3 == 0 {
            with_junk[at] ^= 0xFF;
        }
    }
    std::fs::write(&file, &with_junk).unwrap();
    // the stream is still found behind the junk? Only a file that starts with a major sync is
    // recognised, so this one opens as something else or not at all; it must not panic either way
    if let Ok(mut d) = open_demuxer(&file) {
        while let Ok(Some(_)) = d.read_packet() {}
    }
    let mut damaged = raw.clone();
    for _ in 0..300 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        let at = 40 + (x >> 8) as usize % (raw.len() - 40);
        damaged[at] ^= 1 << (x & 7);
    }
    std::fs::write(&file, &damaged).unwrap();
    let mut d = open_demuxer(&file).expect("a file that starts with a major sync opens");
    let stream = d.streams()[0].clone();
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    while let Ok(Some(p)) = d.read_packet() {
        let _ = dec.decode(&Packet { stream: 0, ..p });
    }
    let _ = std::fs::remove_dir_all(&dir);
}
