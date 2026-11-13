//! The streaming writers (`OggStreamWriter`, `FragmentedMp4Writer`) against the path-based muxers: the same
//! packets give the same bytes, the output reads back through the crate's own demuxers, a flush puts every
//! packet written so far into the sink, and none of it needs a seekable sink.

use ironav_core::{CodecId, Packet, PixelFormat, Rational, StreamInfo, StreamParams};
use ironav_formats::{create_muxer, create_muxer_with, open_demuxer, FragmentedMp4Writer, OggStreamWriter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ironav-stream-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// A write-only sink the test can look into while the writer owns it.
struct Shared(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_back(bytes: &[u8], name: &str) -> (Vec<StreamInfo>, Vec<Packet>) {
    let path = tmp(name);
    std::fs::write(&path, bytes).unwrap();
    let mut d = open_demuxer(&path).unwrap();
    let streams = d.streams().to_vec();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        packets.push(p);
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
    (streams, packets)
}

// ---- Ogg ----

fn opus_info() -> StreamInfo {
    let mut head = b"OpusHead\x01\x01".to_vec();
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    StreamInfo {
        index: 0,
        codec: CodecId::Opus,
        time_base: Rational::new(1, 48_000),
        duration: None,
        bits: None,
        extradata: head,
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: 48_000, channels: 1 },
    }
}

/// 20 ms CELT packets (TOC 0xF8) of varying sizes.
fn opus_packets(n: usize) -> Vec<Packet> {
    (0..n).map(|i| Packet { stream: 0, pts: i as i64 * 960, duration: 960, data: [vec![0xF8], vec![(i % 251) as u8; 20 + i % 40]].concat() }).collect()
}

/// (granule, packets in the page) of each Ogg page; panics unless the bytes are whole pages.
fn pages(mut b: &[u8]) -> Vec<(i64, usize)> {
    let mut out = Vec::new();
    while !b.is_empty() {
        assert_eq!(&b[..4], b"OggS", "a page starts here");
        let n = b[26] as usize;
        let table = &b[27..27 + n];
        let body: usize = table.iter().map(|&t| t as usize).sum();
        let packets = table.iter().filter(|&&t| t < 255).count();
        out.push((i64::from_le_bytes(b[6..14].try_into().unwrap()), packets));
        b = &b[27 + n + body..];
    }
    out
}

#[test]
fn ogg_stream_matches_the_file_writer_and_reads_back() {
    let packets = opus_packets(300); // 6 s: several pages
    let path = tmp("a.ogg");
    let mut m = create_muxer(&path, "ogg").unwrap();
    m.write_header(&[opus_info()]).unwrap();
    for p in &packets {
        m.write_packet(p).unwrap();
    }
    m.finish().unwrap();
    drop(m);
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    // a Vec cannot seek: no seekable sink is needed
    let mut w = OggStreamWriter::new(Vec::new(), &[opus_info()]).unwrap();
    for p in &packets {
        w.write_packet(p).unwrap();
    }
    let stream = w.finish().unwrap();
    assert_eq!(stream, file, "the same packets give the same bytes");

    let (streams, back) = read_back(&stream, "b.ogg");
    assert_eq!(streams[0].codec, CodecId::Opus);
    assert_eq!(back.len(), packets.len());
    assert!(back.iter().zip(&packets).all(|(a, b)| a.data == b.data));
    let g = pages(&stream);
    assert!(g.len() > 4, "several pages");
    assert_eq!(g.last().unwrap().0, 300 * 960, "the last page's granule is the samples decoded");
}

#[test]
fn ogg_header_is_written_at_once_and_flush_puts_every_packet_out() {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut w = OggStreamWriter::new(Shared(sink.clone()), &[opus_info()]).unwrap();
    assert_eq!(pages(&sink.lock().unwrap()), vec![(0, 1), (0, 1)], "OpusHead and OpusTags are out already");
    let packets = opus_packets(8);
    for p in &packets[..5] {
        w.write_packet(p).unwrap();
    }
    assert_eq!(pages(&sink.lock().unwrap()).len(), 2, "a page waits to fill");
    w.flush().unwrap();
    let g = pages(&sink.lock().unwrap());
    assert_eq!(g[2], (5 * 960, 5), "all five packets are out, stamped with the samples they reach");
    let len = sink.lock().unwrap().len();
    w.flush().unwrap();
    assert_eq!(sink.lock().unwrap().len(), len, "a flush with nothing pending writes nothing");
    for p in &packets[5..] {
        w.write_packet(p).unwrap();
    }
    w.finish().unwrap();
    let bytes = sink.lock().unwrap().clone();
    assert_eq!(pages(&bytes).last().unwrap().0, 8 * 960);
    let (_, back) = read_back(&bytes, "c.ogg");
    assert_eq!(back.len(), 8, "a flushed stream is one stream");
    assert!(back.iter().zip(&packets).all(|(a, b)| a.data == b.data));
}

#[test]
fn ogg_stream_with_no_packets_is_still_a_stream() {
    let w = OggStreamWriter::new(Vec::new(), &[opus_info()]).unwrap();
    let bytes = w.finish().unwrap();
    assert_eq!(pages(&bytes).len(), 3, "OpusHead, OpusTags, then the end of stream");
}

// ---- MP4 ----

fn h264_info() -> StreamInfo {
    StreamInfo {
        index: 0,
        codec: CodecId::H264,
        time_base: Rational::new(1, 30),
        duration: None,
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
    }
}

fn aac_info(index: usize) -> StreamInfo {
    StreamInfo {
        index,
        codec: CodecId::Aac,
        time_base: Rational::new(1, 48_000),
        duration: None,
        bits: None,
        extradata: vec![0x11, 0x88],
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: 48_000, channels: 1 },
    }
}

/// Annex B access units: parameter sets and an IDR frame first, a key frame every `gop`.
fn h264_packets(n: i64, gop: i64) -> Vec<Packet> {
    let sps = vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0xAC];
    let pps = vec![0, 0, 0, 1, 0x68, 0xEE, 0x3C];
    let idr = [&[0u8, 0, 0, 1, 0x65][..], &[7; 20]].concat();
    let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 6]].concat();
    (0..n)
        .map(|i| {
            let data = match i {
                0 => [sps.clone(), pps.clone(), idr.clone()].concat(),
                _ if i % gop == 0 => idr.clone(),
                _ => p.clone(),
            };
            Packet { stream: 0, pts: i, duration: 1, data }
        })
        .collect()
}

fn aac_packets(stream: usize, n: i64) -> Vec<Packet> {
    (0..n).map(|i| Packet { stream, pts: i * 1024, duration: 1024, data: vec![0x21, 0x10, (i % 200) as u8, 4, 5] }).collect()
}

fn top_boxes(mut d: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    while d.len() >= 8 {
        let size = u32::from_be_bytes(d[..4].try_into().unwrap()) as usize;
        out.push(d[4..8].try_into().unwrap());
        assert!(size >= 8 && size <= d.len(), "whole boxes only");
        d = &d[size..];
    }
    assert!(d.is_empty());
    out
}

/// Video and audio packets in time order, as a muxer's caller would interleave them.
fn av_packets() -> Vec<Packet> {
    let mut all: Vec<(f64, Packet)> = Vec::new();
    all.extend(h264_packets(70, 10).into_iter().map(|p| (p.pts as f64 / 30.0, p)));
    all.extend(aac_packets(1, 100).into_iter().map(|p| (p.pts as f64 / 48_000.0, p)));
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    all.into_iter().map(|x| x.1).collect()
}

#[test]
fn fmp4_stream_matches_the_file_writer_and_reads_back() {
    let infos = [h264_info(), aac_info(1)];
    let packets = av_packets();
    let path = tmp("a.mp4");
    let mut m = create_muxer_with(&path, "mp4", Some(0.5)).unwrap();
    m.write_header(&infos).unwrap();
    for p in &packets {
        m.write_packet(p).unwrap();
    }
    m.finish().unwrap();
    drop(m);
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let mut w = FragmentedMp4Writer::new(Vec::new(), &infos, 0.5).unwrap();
    for p in &packets {
        w.write_packet(p).unwrap();
    }
    let stream = w.finish().unwrap();
    assert_eq!(stream, file, "the same packets give the same bytes");

    let kinds = top_boxes(&stream);
    assert_eq!(&kinds[..2], [*b"ftyp", *b"moov"]);
    assert!(kinds[2..].chunks(2).all(|c| c == [*b"moof", *b"mdat"]) && kinds.len() > 4, "several fragments");
    let (streams, back) = read_back(&stream, "b.mp4");
    assert_eq!(streams.len(), 2);
    assert_eq!(back.iter().filter(|p| streams[p.stream].codec == CodecId::H264).count(), 70);
    assert_eq!(back.iter().filter(|p| streams[p.stream].codec == CodecId::Aac).count(), 100);
}

#[test]
fn fmp4_audio_flush_closes_the_fragment_at_once() {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut w = FragmentedMp4Writer::new(Shared(sink.clone()), &[aac_info(0)], 60.0).unwrap();
    assert_eq!(top_boxes(&sink.lock().unwrap()), [*b"ftyp"], "ftyp is out at once");
    let packets = aac_packets(0, 30);
    for p in &packets[..10] {
        w.write_packet(p).unwrap();
    }
    assert_eq!(top_boxes(&sink.lock().unwrap()).len(), 1, "a long fragment waits");
    w.flush().unwrap();
    assert_eq!(top_boxes(&sink.lock().unwrap()), [*b"ftyp", *b"moov", *b"moof", *b"mdat"], "the ten packets are out");
    let len = sink.lock().unwrap().len();
    w.flush().unwrap();
    assert_eq!(sink.lock().unwrap().len(), len, "a flush with nothing pending writes nothing");
    for p in &packets[10..] {
        w.write_packet(p).unwrap();
    }
    w.finish().unwrap();
    let bytes = sink.lock().unwrap().clone();
    assert_eq!(top_boxes(&bytes), [*b"ftyp", *b"moov", *b"moof", *b"mdat", *b"moof", *b"mdat"]);
    let (_, back) = read_back(&bytes, "c.mp4");
    assert_eq!(back.len(), 30);
    assert!(back.iter().zip(&packets).all(|(a, b)| a.data[..] == b.data[..] && a.pts == b.pts));
}

#[test]
fn fmp4_video_flush_cuts_at_the_next_key_frame() {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut w = FragmentedMp4Writer::new(Shared(sink.clone()), &[h264_info()], 60.0).unwrap();
    let packets = h264_packets(40, 15);
    for p in &packets[..5] {
        w.write_packet(p).unwrap();
    }
    w.flush().unwrap();
    assert_eq!(top_boxes(&sink.lock().unwrap()), [*b"ftyp"], "nothing can be cut before a key frame");
    for p in &packets[5..16] {
        w.write_packet(p).unwrap();
    }
    assert_eq!(top_boxes(&sink.lock().unwrap()), [*b"ftyp", *b"moov", *b"moof", *b"mdat"], "the key frame at 15 closed the flushed fragment");
    for p in &packets[16..] {
        w.write_packet(p).unwrap();
    }
    w.finish().unwrap();
    let bytes = sink.lock().unwrap().clone();
    assert_eq!(top_boxes(&bytes), [*b"ftyp", *b"moov", *b"moof", *b"mdat", *b"moof", *b"mdat"]);
    let (_, back) = read_back(&bytes, "d.mp4");
    assert_eq!(back.iter().map(|p| p.pts).collect::<Vec<_>>(), (0..40).collect::<Vec<i64>>());
}

#[test]
fn fmp4_stream_with_no_packets_is_refused() {
    let w = FragmentedMp4Writer::new(Vec::new(), &[aac_info(0)], 1.0).unwrap();
    assert!(w.finish().is_err(), "a stream that got no packets has nothing to describe");
}
