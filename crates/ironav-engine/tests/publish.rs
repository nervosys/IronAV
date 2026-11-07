//! A live session that sends what it makes out over SRT, end to end: a receiver (IronAV's own `srt://`
//! source) takes the transport stream and it demuxes to the media that went in.

use ironav_core::{CodecId, Demuxer, Packet, Rational, StreamInfo, StreamParams};
use ironav_formats::stream::StreamDemuxer;
use ironav_engine::live::{close, open, OpenArgs};
use ironav_live::{open_source, Chunk};
use serde_json::{json, Value};
use std::io::Cursor;
use std::net::{TcpListener, UdpSocket};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");

/// A port free for UDP and TCP both: a WHIP endpoint listens on each. Windows reserves ranges of TCP ports, so a
/// port free for UDP alone can be refused for TCP (os error 10013).
fn free_port() -> u16 {
    loop {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

fn args(v: Value) -> OpenArgs {
    serde_json::from_value(v).unwrap()
}

/// Everything a receiver gets until the sender says goodbye.
fn receive(url: &str, limit: Duration) -> Vec<u8> {
    let mut rx = open_source(url).unwrap();
    let (mut out, mut buf, start) = (Vec::new(), vec![0u8; 64 << 10], Instant::now());
    while start.elapsed() < limit {
        match rx.read(&mut buf, Duration::from_millis(20)).unwrap() {
            Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
            Chunk::Idle | Chunk::Restart => {}
            Chunk::End => break,
        }
    }
    out
}

fn packets(ts: Vec<u8>) -> (Vec<CodecId>, Vec<Packet>) {
    let mut d = ironav_formats::ts::TsDemuxer::open(Cursor::new(ts)).unwrap();
    let codecs = d.streams().iter().map(|s| s.codec).collect();
    (codecs, std::iter::from_fn(|| d.read_packet().unwrap()).collect())
}

fn wait_end(id: &str) -> Value {
    let start = Instant::now();
    loop {
        let st = ironav_engine::call_tool("stream_status", &json!({ "session": id })).unwrap();
        if !matches!(st["state"].as_str(), Some("running" | "waiting")) {
            return st;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{st}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn events(id: &str, kind: &str) -> Vec<Value> {
    let q = ironav_engine::call_tool("stream_query", &json!({ "session": id, "kinds": [kind] })).unwrap();
    q["events"].as_array().unwrap().clone()
}

#[test]
fn a_session_sends_its_media_as_transport_stream_over_srt() {
    ironav_live::net::allow_loopback_for_tests();
    let port = free_port();
    let url = format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0&passphrase=0123456789abcdef");
    let receiver = std::thread::spawn(move || receive(&url, Duration::from_secs(30)));
    // the receiver binds before the session calls
    std::thread::sleep(Duration::from_millis(300));
    let s = open(&args(json!({ "source": FIXTURE, "pace": "realtime", "analyzers": ["structure"], "publish": format!("srt://127.0.0.1:{port}?passphrase=0123456789abcdef&pbkeylen=32") }))).unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "finished", "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    assert_eq!(st["publish"]["packets"], 145, "{st}");
    // the passphrase stays out of the status
    assert!(!st.to_string().contains("0123456789abcdef"), "{st}");
    let (codecs, got) = packets(receiver.join().unwrap());
    assert_eq!(codecs, [CodecId::H264, CodecId::Aac]);
    assert_eq!(got.iter().filter(|p| p.stream == 0).count(), 50);
    assert!(got.iter().filter(|p| p.stream == 1).count() >= 93);
    // the first picture can be decoded from
    let first = got.iter().find(|p| p.stream == 0).unwrap();
    assert_eq!(ironav_formats::is_keyframe(CodecId::H264, &first.data), Some(true));
}

#[test]
fn with_a_transcode_what_is_sent_is_what_the_transcode_writes() {
    ironav_live::net::allow_loopback_for_tests();
    let port = free_port();
    let url = format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0&passphrase=0123456789abcdef");
    let receiver = std::thread::spawn(move || receive(&url, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(300));
    let dir = std::env::temp_dir().join(format!("ironav-publish-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("tc.ts");
    let s = open(&args(json!({
        "source": FIXTURE, "pace": "realtime", "analyzers": ["structure"],
        "transcode": { "output": out.to_str().unwrap(), "overwrite": true },
        "publish": format!("srt://127.0.0.1:{port}?passphrase=0123456789abcdef"),
    })))
    .unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["transcode"]["error"], Value::Null, "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    let (codecs, got) = packets(receiver.join().unwrap());
    assert_eq!(codecs, [CodecId::H264, CodecId::Aac]);
    // the same pictures as the file the transcode wrote
    let (_, written) = packets(std::fs::read(&out).unwrap());
    let n = |v: &[Packet], s: usize| v.iter().filter(|p| p.stream == s).count();
    assert!(n(&written, 0) > 40, "{}", n(&written, 0));
    assert_eq!((n(&got, 0), n(&got, 1)), (n(&written, 0), n(&written, 1)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_destination_that_cannot_be_reached_is_reported_and_the_session_goes_on() {
    ironav_live::net::allow_loopback_for_tests();
    // nobody listens: a caller is tried again with backoff, and says so
    let s = open(&args(json!({ "source": FIXTURE, "pace": "realtime", "publish": format!("srt://127.0.0.1:{}?passphrase=0123456789abcdef", free_port()) }))).unwrap();
    wait_end(&s.id);
    let lost = events(&s.id, "publish_lost");
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(lost.len(), 1, "{st}");
    assert!(st["publish"]["attempts"].as_u64().unwrap() >= 1, "{st}");
    assert!(st["publish"]["last_error"].is_string(), "{st}");
    // with reconnect=0 the first failure ends the publishing, and the session still ends well
    let s = open(&args(json!({ "source": FIXTURE, "analyzers": ["structure"], "publish": format!("srt://127.0.0.1:{}?passphrase=0123456789abcdef&reconnect=0", free_port()) }))).unwrap();
    wait_end(&s.id);
    let (failed, lost) = (events(&s.id, "publish_failed"), events(&s.id, "publish_lost"));
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "failed", "{st}");
    assert_eq!((failed.len(), lost.len()), (1, 0), "{st}");
    // an address the allow-list refuses is not tried again, with or without reconnect
    let s = open(&args(json!({ "source": FIXTURE, "publish": "srt://10.255.255.1:9000?passphrase=0123456789abcdef" }))).unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "failed", "{st}");
    assert!(st["publish"]["error"]["hint"].as_str().is_some_and(|h| h.contains("IRONAV_NET_ALLOW")), "{st}");
}

/// What a source gives until it ends, or until the limit passes.
fn read_until_end(s: &mut dyn ironav_live::Source, limit: Duration) -> Vec<u8> {
    let (mut out, mut buf, start) = (Vec::new(), vec![0u8; 64 << 10], Instant::now());
    while start.elapsed() < limit {
        match s.read(&mut buf, Duration::from_millis(20)).unwrap() {
            Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
            Chunk::Idle | Chunk::Restart => {}
            Chunk::End => break,
        }
    }
    out
}

#[test]
fn a_session_publishes_its_h264_to_a_whip_endpoint() {
    ironav_live::net::allow_loopback_for_tests();
    let port = free_port();
    let token = "s3cret";
    // the endpoint is listening before the session calls it; it ends when the session says goodbye
    let mut endpoint = open_source(&format!("whip://127.0.0.1:{port}/live?token={token}")).unwrap();
    let reader = std::thread::spawn(move || read_until_end(endpoint.as_mut(), Duration::from_secs(30)));
    let s = open(&args(json!({
        "source": FIXTURE, "pace": "realtime", "analyzers": ["structure"],
        "publish": format!("whip://127.0.0.1:{port}/live?token={token}"),
    })))
    .unwrap();
    wait_end(&s.id);
    let got = reader.join().unwrap();
    let skipped = events(&s.id, "publish_audio_skipped");
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "finished", "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    // the fixture's 50 pictures; its AAC is not sent over WHIP, and said so once
    assert_eq!(st["publish"]["packets"], 50, "{st}");
    assert_eq!(skipped.len(), 1, "{st}");
    // the token stays out of what is shown
    assert!(!st.to_string().contains(token), "{st}");
    // what the endpoint received: H.264 only, every picture, and the first one a keyframe
    let mut r = ironav_formats::rtp::RtpStream::default();
    r.feed(&got).unwrap();
    r.end();
    let mut out = Vec::new();
    while let Some(p) = r.next_packet().unwrap() {
        out.push(p);
    }
    let codecs: Vec<_> = r.streams().iter().map(|s| s.codec).collect();
    assert_eq!(codecs, [CodecId::H264]);
    let video: Vec<_> = out.iter().filter(|p| p.packet.stream == 0).collect();
    assert_eq!(video.len(), 50, "{st}");
    assert_eq!(ironav_formats::is_keyframe(CodecId::H264, &video[0].packet.data), Some(true));
}

/// A Matroska file holding the fixture's H.264 video and two seconds of a stereo tone as Opus (20 ms
/// packets), interleaved by time. Returns the Opus packets it holds, in order.
fn h264_and_opus_source(path: &std::path::Path) -> Vec<Vec<u8>> {
    let mut d = ironav_formats::open_demuxer(std::path::Path::new(FIXTURE)).unwrap();
    let video = d.streams()[0].clone();
    assert_eq!(video.codec, CodecId::H264);
    let vid: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).filter(|p| p.stream == 0).collect();
    let mut enc = ironav_codec::opus::OpusEncoder::new(48_000, 2, 96_000).unwrap();
    let n = enc.frame_samples();
    let mut opus = Vec::new();
    let mut pcm = vec![0i16; n * 2];
    for k in 0..100 {
        for i in 0..n {
            let t = (k * n + i) as f64 / 48_000.0;
            let v = ((t * 2.0 * std::f64::consts::PI * 440.0).sin() * 8000.0) as i16;
            (pcm[2 * i], pcm[2 * i + 1]) = (v, v / 2);
        }
        let mut out = vec![0u8; enc.packet_bytes()];
        let len = enc.encode_frame_into_i16(&pcm, &mut out).unwrap();
        out.truncate(len);
        opus.push(out);
    }
    let audio = StreamInfo {
        index: 1,
        codec: CodecId::Opus,
        time_base: Rational::new(1, 48_000),
        duration: None,
        bits: None,
        extradata: enc.head(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: 48_000, channels: 2 },
    };
    // by time: the video's own times and the audio's 20 ms steps; the video first at a tie
    let mut all: Vec<(f64, Packet)> = vid.into_iter().map(|p| (p.pts as f64 * video.time_base.as_f64(), p)).collect();
    for (k, data) in opus.iter().enumerate() {
        let pts = (k * n) as i64;
        all.push((pts as f64 / 48_000.0, Packet { stream: 1, pts, duration: n as i64, data: data.clone() }));
    }
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut m = ironav_formats::create_muxer(path, "mkv").unwrap();
    m.write_header(&[video, audio]).unwrap();
    for (_, p) in &all {
        m.write_packet(p).unwrap();
    }
    m.finish().unwrap();
    opus
}

#[test]
fn a_session_publishes_its_h264_and_opus_to_a_whip_endpoint() {
    ironav_live::net::allow_loopback_for_tests();
    let dir = std::env::temp_dir().join(format!("ironav-whip-opus-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("av.mkv");
    let sent = h264_and_opus_source(&path);
    let port = free_port();
    let token = "s3cret";
    let mut endpoint = open_source(&format!("whip://127.0.0.1:{port}/live?token={token}")).unwrap();
    let reader = std::thread::spawn(move || read_until_end(endpoint.as_mut(), Duration::from_secs(30)));
    let s = open(&args(json!({
        "source": path.to_str().unwrap(), "pace": "realtime", "analyzers": ["structure"],
        "publish": format!("whip://127.0.0.1:{port}/live?token={token}"),
    })))
    .unwrap();
    wait_end(&s.id);
    let got = reader.join().unwrap();
    let skipped = events(&s.id, "publish_audio_skipped");
    let st = close(&s.id).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "finished", "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    // the fixture's 50 pictures and the 100 Opus packets: nothing is skipped, and no audio is said to be unsent
    assert_eq!(st["publish"]["packets"], 50 + sent.len(), "{st}");
    assert_eq!(st["publish"]["skipped_packets"], 0, "{st}");
    assert!(skipped.is_empty(), "{st}");
    // the token stays out of what is shown
    assert!(!st.to_string().contains(token), "{st}");
    let mut r = ironav_formats::rtp::RtpStream::default();
    r.feed(&got).unwrap();
    r.end();
    let mut out = Vec::new();
    while let Some(p) = r.next_packet().unwrap() {
        out.push(p);
    }
    let codecs: Vec<_> = r.streams().iter().map(|s| s.codec).collect();
    assert_eq!(codecs, [CodecId::H264, CodecId::Opus]);
    // Opus: every packet the source held, byte for byte and in order, on a 48 kHz clock (20 ms is 960 ticks)
    let audio: Vec<&Packet> = out.iter().map(|p| &p.packet).filter(|p| p.stream == 1).collect();
    assert_eq!(r.streams()[1].time_base, Rational::new(1, 48_000));
    assert_eq!(audio.len(), sent.len(), "{st}");
    for (p, want) in audio.iter().zip(&sent) {
        assert_eq!(&p.data, want);
    }
    for w in audio.windows(2) {
        assert_eq!(w[1].pts - w[0].pts, 960, "{} {}", w[0].pts, w[1].pts);
    }
    // H.264 still arrives whole: every picture, the first one a keyframe
    let video: Vec<&Packet> = out.iter().map(|p| &p.packet).filter(|p| p.stream == 0).collect();
    assert_eq!(video.len(), 50, "{st}");
    assert_eq!(ironav_formats::is_keyframe(CodecId::H264, &video[0].data), Some(true));
}

#[test]
fn a_video_that_is_not_h264_ends_publishing_with_a_hint_to_transcode() {
    ironav_live::net::allow_loopback_for_tests();
    // nothing listens: the codec is refused before anything is connected to
    let s = open(&args(json!({ "source": "test://video?duration=1", "publish": "whip://127.0.0.1:9/live" }))).unwrap();
    wait_end(&s.id);
    let failed = events(&s.id, "publish_failed");
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "failed", "{st}");
    assert_eq!(st["publish"]["error"]["code"], "unsupported-codec", "{st}");
    assert!(st["publish"]["error"]["hint"].as_str().is_some_and(|h| h.contains("--transcode")), "{st}");
    assert_eq!(failed.len(), 1, "{st}");
}

#[test]
fn a_destination_is_checked_before_the_session_starts() {
    ironav_live::net::allow_loopback_for_tests();
    for bad in [
        "udp://127.0.0.1:9", "out.ts", "srt://127.0.0.1", "srt://127.0.0.1:9?latency=x", "srt://127.0.0.1:9?format=h264&passphrase=0123456789", "srt://127.0.0.1:9?pbkeylen=7&passphrase=0123456789",
        "whip://127.0.0.1:9?cert=a.pem&key=b.pem", "whips://:9/live", "whip://127.0.0.1:9/live?latency=1",
    ] {
        let e = open(&args(json!({ "source": FIXTURE, "publish": bad }))).err().unwrap_or_else(|| panic!("{bad} was accepted"));
        assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument, "{bad}: {e:?}");
    }
}
