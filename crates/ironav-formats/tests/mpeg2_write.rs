//! MPEG-2 video written to the four containers that take it: a raw elementary stream (`.m2v`), an
//! MPEG transport stream, Matroska and MP4. Pictures from the software encoder go through each writer
//! and come back through `open_demuxer`: the codec reads back as MPEG-2 video, and the picture bytes
//! are the encoder's, in decode order.

use ironav_codec::mpeg2::enc::{config, Mpeg2Encoder};
use ironav_core::{CodecId, Demuxer, Encoder, Frame, Packet, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
use ironav_formats::{create_muxer, is_keyframe, open_demuxer};
use std::path::{Path, PathBuf};

const W: u32 = 64;
const H: u32 = 48;
const FRAMES: i64 = 6;
const FPS: i64 = 25;

fn params() -> StreamParams {
    StreamParams::Video { width: W, height: H, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(FPS, 1) }
}

/// A moving gradient in 4:2:0.
fn frame(t: i64) -> Frame {
    let (w, h) = (W as usize, H as usize);
    let mut y = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            y[r * w + c] = ((c as i64) * 3 + (r as i64) * 2 + t * 7).rem_euclid(256) as u8;
        }
    }
    let chroma = |k: i64| (0..(w / 2) * (h / 2)).map(|i| ((i as i64) * 5 + k * 11 + t * 3).rem_euclid(256) as u8).collect::<Vec<u8>>();
    Frame::Video(VideoFrame { width: W, height: H, pix_fmt: PixelFormat::Yuv420p, pts: t, planes: vec![y, chroma(1), chroma(2)] })
}

/// The encoder's packets for the test frames, in decode order (with B pictures), and the stream
/// they make.
fn encode() -> (Vec<Packet>, StreamInfo) {
    let mut enc = Mpeg2Encoder::new(&params(), Some(8), None).expect("an MPEG-2 encoder").with_bframes(2);
    let mut packets = Vec::new();
    for t in 0..FRAMES {
        packets.extend(enc.encode(&frame(t)).expect("a packet"));
    }
    packets.extend(enc.flush().expect("the pictures still held"));
    assert_eq!(packets.len(), FRAMES as usize, "one picture per frame");
    let info = StreamInfo {
        index: 0,
        codec: CodecId::Mpeg2Video,
        time_base: Rational::new(1, FPS),
        duration: Some(FRAMES),
        bits: None,
        extradata: config(W, H, FPS, 1),
        lead_in: 0,
        params: params(),
    };
    (packets, info)
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mpeg2-write-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes `packets` as `info` to `name`, through the writer the format registry gives for `format`.
fn write(dir: &Path, name: &str, format: &str, info: &StreamInfo, packets: &[Packet]) -> PathBuf {
    let path = dir.join(name);
    let mut m = create_muxer(&path, format).expect("a writer");
    m.write_header(std::slice::from_ref(info)).expect("the header");
    for p in packets {
        m.write_packet(p).expect("a packet");
    }
    m.finish().expect("the trailer");
    path
}

/// The stream of `path` and its packets, read back with the formats registry.
fn read_back(path: &Path) -> (StreamInfo, Vec<Packet>) {
    let mut d: Box<dyn Demuxer> = open_demuxer(path).expect("the file opens");
    let info = d.streams()[0].clone();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        packets.push(p);
    }
    (info, packets)
}

/// The pictures of `got` are those of `want`, byte for byte and in the same order.
fn same_pictures(got: &[Packet], want: &[Packet]) {
    let g: Vec<&[u8]> = got.iter().map(|p| p.data.as_slice()).collect();
    let w: Vec<&[u8]> = want.iter().map(|p| p.data.as_slice()).collect();
    assert_eq!(g.len(), w.len(), "picture count");
    assert!(g == w, "the picture bytes differ from the encoder's");
}

/// Which pictures are key pictures (I pictures), by their bytes.
fn keys(packets: &[Packet]) -> Vec<bool> {
    packets.iter().map(|p| is_keyframe(CodecId::Mpeg2Video, &p.data) == Some(true)).collect()
}

#[test]
fn raw_m2v_holds_the_pictures_in_order() {
    let dir = scratch();
    let (packets, info) = encode();
    let path = write(&dir, "stream.m2v", "m2v", &info, &packets);
    let want: Vec<u8> = packets.iter().flat_map(|p| p.data.iter().copied()).collect();
    assert!(std::fs::read(&path).unwrap() == want, "the file is the pictures, verbatim and in order");
    let (back, read) = read_back(&path);
    assert_eq!(back.codec, CodecId::Mpeg2Video);
    let got: Vec<u8> = read.iter().flat_map(|p| p.data.iter().copied()).collect();
    assert!(got == want, "the reader gives the same bytes back");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpeg2_in_a_transport_stream_reads_back_as_mpeg2() {
    let dir = scratch();
    let (packets, info) = encode();
    let path = write(&dir, "stream.ts", "mpegts", &info, &packets);
    let (back, read) = read_back(&path);
    assert_eq!(back.codec, CodecId::Mpeg2Video);
    same_pictures(&read, &packets);
    assert_eq!(keys(&read), keys(&packets));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpeg2_in_matroska_reads_back_as_mpeg2() {
    let dir = scratch();
    let (packets, info) = encode();
    let path = write(&dir, "stream.mkv", "mkv", &info, &packets);
    let (back, read) = read_back(&path);
    assert_eq!(back.codec, CodecId::Mpeg2Video);
    assert_eq!(read.len(), packets.len(), "packet count");
    same_pictures(&read, &packets);
    assert_eq!(keys(&read), keys(&packets));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpeg2_in_mp4_reads_back_as_mpeg2() {
    let dir = scratch();
    let (packets, info) = encode();
    let path = write(&dir, "stream.mp4", "mp4", &info, &packets);
    let (back, read) = read_back(&path);
    assert_eq!(back.codec, CodecId::Mpeg2Video);
    assert_eq!(read.len(), packets.len(), "packet count");
    same_pictures(&read, &packets);
    assert_eq!(keys(&read), keys(&packets));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn raw_m2v_refuses_other_codecs() {
    let dir = scratch();
    let (_, info) = encode();
    for codec in [CodecId::H264, CodecId::Mpeg1Video] {
        let mut m = create_muxer(&dir.join("refused.m2v"), "m2v").expect("a writer");
        let other = StreamInfo { codec, ..info.clone() };
        assert!(m.write_header(&[other]).is_err(), "{} is not written as m2v", codec.name());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
