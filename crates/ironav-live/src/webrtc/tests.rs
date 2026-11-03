//! The endpoint end to end on loopback, against a test publisher that does what a WebRTC one does:
//! POST an offer, STUN, a DTLS client handshake, then H.264 and Opus over SRTP and a sender report
//! over SRTCP; and against garbage on the same port.

use super::dtls::client::Client;
use super::*;
use ironav_formats::stream::{stream_demuxer, StreamDemuxer, StreamOptions};
use std::io::{Read, Write};
use std::net::TcpStream;

/// A port free for both TCP and UDP on loopback.
fn free_port() -> u16 {
    loop {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// One HTTP request to the endpoint: the status, the headers and the body.
fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Vec<(String, String)>, String) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(c, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).unwrap();
    let text = String::from_utf8(got).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    (status, headers, body.to_string())
}

fn offer(fingerprint: &[u8; 32]) -> String {
    let fp = dtls::fingerprint_text(fingerprint);
    format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE v a\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 96 102\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:pubU\r\na=ice-pwd:publisherpassword0123456\r\n\
         a=fingerprint:sha-256 {fp}\r\na=setup:actpass\r\na=mid:v\r\na=sendonly\r\na=rtcp-mux\r\n\
         a=rtpmap:96 VP8/90000\r\na=rtpmap:102 H264/90000\r\na=fmtp:102 packetization-mode=1;profile-level-id=42e01f\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:a\r\na=sendonly\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\n"
    )
}

/// The value of an answer's `a=NAME:` line.
fn attr<'a>(sdp: &'a str, name: &str) -> &'a str {
    sdp.lines().find_map(|l| l.strip_prefix(&format!("a={name}:"))).unwrap_or_else(|| panic!("no a={name} in {sdp}"))
}

/// The H.264 RTP packets of the RTP fixture, and the Opus packets of a WebM file.
fn media() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let d = std::fs::read(format!("{}/../ironav-formats/tests/data/h264_aac.rtp", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut video = Vec::new();
    let mut i = 0;
    while i + 4 <= d.len() {
        let n = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
        if d[i + 1] == 0 {
            video.push(d[i + 4..i + 4 + n].to_vec());
        }
        i += 4 + n;
    }
    let webm = std::fs::read(format!("{}/../ironav-formats/tests/data/vp9_opus_unsized.webm", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut demux = stream_demuxer(&webm, &StreamOptions::default()).unwrap();
    demux.feed(&webm).unwrap();
    demux.end();
    let mut audio = Vec::new();
    while let Some(p) = demux.next_packet().unwrap() {
        if demux.streams()[p.packet.stream].codec == ironav_core::CodecId::Opus {
            audio.push(p.packet.data);
        }
    }
    assert!(video.len() > 100 && audio.len() > 20);
    (video, audio)
}

/// A publisher that has connected: its socket, SRTP for what it sends, SRTCP for what it hears.
struct Publisher {
    sock: UdpSocket,
    to: SocketAddr,
    tx: srtp::Context,
    rx: srtp::Context,
    location: String,
}

fn publish(port: u16, path: &str, token: Option<&str>, profiles: &[u16]) -> Publisher {
    let mut client = Client::new([0; 32], profiles, true);
    let (status, headers, answer) = request(port, "POST", path, token, &offer(&client.id.fingerprint));
    assert_eq!(status, 201, "{answer}");
    let location = headers.iter().find(|(k, _)| k == "location").unwrap().1.clone();
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "application/sdp"));
    assert!(answer.contains("a=ice-lite") && answer.contains("a=setup:passive") && answer.contains("a=group:BUNDLE v a"), "{answer}");
    let fp = attr(&answer, "fingerprint").strip_prefix("sha-256 ").unwrap();
    let fp: Vec<u8> = fp.split(':').map(|h| u8::from_str_radix(h, 16).unwrap()).collect();
    client.server_fingerprint = fp.try_into().unwrap();
    let (ufrag, pwd) = (attr(&answer, "ice-ufrag").to_string(), attr(&answer, "ice-pwd").to_string());
    let candidate: Vec<&str> = attr(&answer, "candidate").split(' ').collect();
    let to: SocketAddr = format!("{}:{}", candidate[4], candidate[5]).parse().unwrap();
    assert_eq!(to.port(), port);

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let txid = [3u8; 12];
    sock.send_to(&stun::request(&txid, &format!("{ufrag}:pubU"), &pwd, true), to).unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = sock.recv_from(&mut buf).unwrap();
    assert_eq!(stun::check_success(&buf[..n], &txid, &pwd), Some(sock.local_addr().unwrap()));

    let mut pending = client.start();
    let started = Instant::now();
    while client.keys.is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "the handshake did not finish");
        for d in pending.drain(..) {
            sock.send_to(&d, to).unwrap();
        }
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        pending = client.handle(&buf[..n]).unwrap();
    }
    let k = client.keys.clone().unwrap();
    let want = if profiles.contains(&0x0007) { srtp::Profile::AeadAes128Gcm } else { srtp::Profile::Aes128CmHmacSha1_80 };
    assert_eq!(k.profile, want);
    sock.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    Publisher { sock, to, tx: srtp::Context::new(k.profile, &k.client_key, &k.client_salt).unwrap(), rx: srtp::Context::new(k.profile, &k.server_key, &k.server_salt).unwrap(), location }
}

fn rtp(pt: u8, seq: u16, ts: u32, ssrc: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, pt | if marker { 0x80 } else { 0 }];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

const VIDEO_SSRC: u32 = 0x1111_2222;
const AUDIO_SSRC: u32 = 0x3333_4444;

/// The publisher's RTP: the fixture's H.264 as payload type 102 and the Opus as 111, interleaved.
fn packets() -> Vec<(u8, Vec<u8>)> {
    let (video, audio) = media();
    let mut out = Vec::new();
    let mut a = audio.iter().enumerate();
    for (k, v) in video.iter().enumerate() {
        let mut p = v.clone();
        let h = srtp::rtp_header_len(&p).unwrap();
        p[1] = (p[1] & 0x80) | 102;
        p[8..12].copy_from_slice(&VIDEO_SSRC.to_be_bytes());
        // the fixture's header extensions, if any, are kept: they are the sender's business
        assert!(h >= 12);
        out.push((0, p));
        if k % 4 == 3 {
            if let Some((j, f)) = a.next() {
                out.push((2, rtp(111, 500 + j as u16, 7000 + 960 * j as u32, AUDIO_SSRC, true, f)));
            }
        }
    }
    for (j, f) in a {
        out.push((2, rtp(111, 500 + j as u16, 7000 + 960 * j as u32, AUDIO_SSRC, true, f)));
    }
    out
}

/// What the publisher sends, in order: `packets()` with video packets 30 and 31 late (after 34,
/// as a retransmission after a NACK would be) and 60 to 79 lost (so a keyframe is asked for).
fn plan() -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut late = Vec::new();
    let mut v = 0;
    for (c, p) in packets() {
        if c == 0 {
            v += 1;
            match v - 1 {
                30 | 31 => {
                    late.push((c, p));
                    continue;
                }
                60..=79 => continue,
                34 => {
                    out.push((c, p));
                    out.append(&mut late);
                    continue;
                }
                _ => {}
            }
        }
        out.push((c, p));
    }
    out
}

/// A sender report for the video, with an NTP time.
fn sender_report() -> Vec<u8> {
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&VIDEO_SSRC.to_be_bytes());
    p.extend_from_slice(&0xE6B0_0000_8000_0000u64.to_be_bytes());
    p.extend_from_slice(&1234u32.to_be_bytes());
    p.extend_from_slice(&[0; 8]);
    p
}

/// Everything the source gives until it ends.
fn drain(s: &mut dyn Source, out: &mut Vec<u8>, until: Instant) -> bool {
    let mut buf = vec![0u8; 65536];
    while Instant::now() < until {
        match s.read(&mut buf, Duration::from_millis(20)).unwrap() {
            Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
            Chunk::Idle | Chunk::Restart => {}
            Chunk::End => return true,
        }
    }
    false
}

/// The frames of the RTP stream framing, the description first.
fn frames(mut b: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let n = 4 + u16::from_be_bytes([b[2], b[3]]) as usize;
        out.push((b[1], b[4..n].to_vec()));
        b = &b[n..];
    }
    assert!(b.is_empty());
    out
}

fn publishes(profiles: &[u16], token: Option<&str>) {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let query = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let url = format!("whip://127.0.0.1:{port}/live{query}");
    let mut s = crate::open_source(&url).unwrap();
    assert_eq!(s.info().kind, "whip");
    assert!(!s.info().url.contains("s3cret"), "{}", s.info().url);
    let sent = plan();
    let publisher = std::thread::spawn({
        let token = token.map(str::to_string);
        let profiles = profiles.to_vec();
        let sent = sent.clone();
        move || {
            let tok = token.as_deref();
            if tok.is_some() {
                // without the token, or with another, nothing happens
                assert_eq!(request(port, "POST", "/live", None, &offer(&[1; 32])).0, 401);
                assert_eq!(request(port, "POST", "/live", Some("wrong"), &offer(&[1; 32])).0, 401);
            }
            assert_eq!(request(port, "POST", "/elsewhere", tok, &offer(&[1; 32])).0, 404);
            assert_eq!(request(port, "OPTIONS", "/live", None, "").0, 204);
            let mut p = publish(port, "/live", tok, &profiles);
            // a second publisher is turned away while the first is connected
            assert_eq!(request(port, "POST", "/live", tok, &offer(&[1; 32])).0, 409);
            assert_eq!(request(port, &p.location.clone(), "/x", tok, "").0, 400, "a malformed request line");
            assert_eq!(request(port, "PATCH", &p.location, tok, "").0, 405);
            // garbage on the media port, from the publisher's address and from another, changes nothing
            let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut seed = 0x9e37_79b9u32;
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            let first = p.tx.protect_rtp(&sent[0].1).unwrap();
            let mut heard = Vec::new();
            let mut rbuf = vec![0u8; 4096];
            for (k, (_, pkt)) in sent.iter().enumerate() {
                if k == 0 {
                    p.sock.send_to(&first, p.to).unwrap();
                } else {
                    p.sock.send_to(&p.tx.protect_rtp(pkt).unwrap(), p.to).unwrap();
                }
                if k == 10 {
                    p.sock.send_to(&p.tx.protect_rtcp(&sender_report()).unwrap(), p.to).unwrap();
                    // a replay of the first packet, truncated and bit-flipped copies, and noise
                    p.sock.send_to(&first, p.to).unwrap();
                    // (in small bursts, so the socket's buffer never overflows and drops real packets)
                    for n in 0..first.len() {
                        p.sock.send_to(&first[..n], p.to).unwrap();
                        if n % 16 == 0 {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    for _ in 0..50 {
                        let mut bad = first.clone();
                        let i = next() as usize % bad.len();
                        bad[i] ^= 1 << (next() % 8);
                        p.sock.send_to(&bad, p.to).unwrap();
                        let mut noise: Vec<u8> = (0..next() % 300).map(|_| next() as u8).collect();
                        if let Some(b) = noise.first_mut() {
                            *b = [0u8, 1, 22, 23, 0x80, 0x90][next() as usize % 6];
                        }
                        p.sock.send_to(&noise, p.to).unwrap();
                        stranger.send_to(&noise, p.to).unwrap();
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if k % 4 == 0 {
                    std::thread::sleep(Duration::from_millis(2));
                }
                while let Ok((n, _)) = p.sock.recv_from(&mut rbuf) {
                    heard.push(rbuf[..n].to_vec());
                }
            }
            // the endpoint's feedback: receiver reports, and a keyframe asked for
            let until = Instant::now() + Duration::from_millis(2500);
            while Instant::now() < until {
                if let Ok((n, _)) = p.sock.recv_from(&mut rbuf) {
                    heard.push(rbuf[..n].to_vec());
                }
            }
            let rtcp: Vec<Vec<u8>> = heard.iter().filter_map(|d| p.rx.unprotect_rtcp(d).ok()).collect();
            assert!(!rtcp.is_empty(), "receiver reports come");
            assert!(rtcp.iter().all(|r| r[1] == 201), "each compound packet starts with a receiver report");
            let has = |r: &Vec<u8>, pt: u8| {
                let mut at = 0;
                while let Some(h) = r.get(at..at + 4) {
                    if h[1] == pt {
                        return true;
                    }
                    at += 4 * (u16::from_be_bytes([h[2], h[3]]) as usize + 1);
                }
                false
            };
            assert!(rtcp.iter().any(|r| r[0] & 31 >= 1 && r[8..12] == VIDEO_SSRC.to_be_bytes()), "a report block for the video");
            assert!(rtcp.iter().any(|r| has(r, 205)), "the late packets were NACKed");
            assert!(rtcp.iter().any(|r| has(r, 206)), "a keyframe was asked for after the loss");
            assert_eq!(request(port, "DELETE", &p.location, tok, "").0, 200);
        }
    });
    let mut got = Vec::new();
    assert!(drain(s.as_mut(), &mut got, Instant::now() + Duration::from_secs(30)), "the source ends after the DELETE");
    publisher.join().unwrap();

    // the description first, then every packet sent, decrypted, on its track's channel, in order
    let f = frames(&got);
    assert_eq!(f[0].0, ironav_formats::rtp::DESCRIPTION_CHANNEL);
    let desc: Description = serde_json::from_slice(&f[0].1).unwrap();
    assert_eq!(desc.tracks.iter().map(|t| (t.media, t.channel)).collect::<Vec<_>>(), [(0, 0), (1, 2)]);
    assert!(desc.tracks.iter().all(|t| t.rtptime.is_some()));
    assert!(desc.sdp.contains("m=video 0 RTP/AVP 102") && desc.sdp.contains("m=audio 0 RTP/AVP 111"), "{}", desc.sdp);
    let rtp_frames: Vec<(u8, Vec<u8>)> = f[1..].iter().filter(|(c, _)| c % 2 == 0).cloned().collect();
    // loopback UDP may still drop a datagram when the machine is busy (other tests, builds), so a
    // few packets may be missing; what does come must be unchanged, once each, in order
    let mut missing = Vec::new();
    let mut next = rtp_frames.iter().peekable();
    for (i, p) in sent.iter().enumerate() {
        if next.peek() == Some(&p) {
            next.next();
        } else {
            missing.push(i);
        }
    }
    assert!(next.next().is_none(), "every packet received was sent, once, unchanged, in order");
    assert!(missing.len() <= sent.len() / 50, "missing {missing:?}");
    let reports: Vec<&(u8, Vec<u8>)> = f[1..].iter().filter(|(c, _)| c % 2 == 1).collect();
    assert_eq!(reports.len(), 1);
    assert_eq!((reports[0].0, &reports[0].1), (1, &sender_report()));

    // and the RTP stream reader makes H.264 and Opus of it, the Opus packets those of the file
    let mut r = ironav_formats::rtp::RtpStream::default();
    r.feed(&got).unwrap();
    r.end();
    let mut out = Vec::new();
    while let Some(p) = r.next_packet().unwrap() {
        out.push(p);
    }
    let codecs: Vec<_> = r.streams().iter().map(|s| s.codec).collect();
    assert_eq!(codecs, [ironav_core::CodecId::H264, ironav_core::CodecId::Opus]);
    let (_, audio) = media();
    let opus: Vec<&Vec<u8>> = out.iter().filter(|p| p.packet.stream == 1).map(|p| &p.packet.data).collect();
    if missing.is_empty() {
        assert_eq!(opus, audio.iter().collect::<Vec<_>>());
    } else {
        assert!(opus.len() + missing.len() >= audio.len() && opus.iter().all(|o| audio.contains(o)));
    }
    let video: Vec<_> = out.iter().filter(|p| p.packet.stream == 0).collect();
    assert!(video.len() > 10 && (video[0].key || !missing.is_empty()));
    assert!(r.wall_clock(0).is_some(), "the sender report reached the video track");
}

#[test]
fn publishes_over_srtp_aes_gcm_with_a_token() {
    publishes(&[0x0001, 0x0007], Some("s3cret"));
}

#[test]
#[cfg_attr(feature = "fips", ignore = "FIPS build: AES128_CM_HMAC_SHA1_80 is not offered (dtls tests cover the refusal)")]
fn publishes_over_srtp_aes_cm_hmac_sha1() {
    publishes(&[0x0001], None);
}

#[test]
fn a_publisher_that_never_connects_frees_the_endpoint() {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let mut s = crate::open_source(&format!("whip://127.0.0.1:{port}")).unwrap();
    // a session still setting up gives way to the next offer
    let (status, _, _) = request(port, "POST", "/", None, &offer(&[1; 32]));
    assert_eq!(status, 201);
    let (status, _, _) = request(port, "POST", "/", None, &offer(&[2; 32]));
    assert_eq!(status, 201);
    // a bad offer says why
    let (status, _, body) = request(port, "POST", "/", None, "v=0\r\n");
    assert_eq!((status, body.contains("media")), (400, true));
    let mut buf = [0u8; 100];
    assert_eq!(s.read(&mut buf, Duration::from_millis(50)).unwrap(), Chunk::Idle);
    drop(s);
    // and dropping the source closes the port
    std::thread::sleep(Duration::from_millis(50));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err() || TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn targets_parse() {
    assert_eq!(parse_target(":8080/live").unwrap(), ("127.0.0.1".into(), 8080, "/live".into()));
    assert_eq!(parse_target("0.0.0.0:8080").unwrap(), ("0.0.0.0".into(), 8080, "/".into()));
    assert_eq!(parse_target("[::1]:9/a/b/").unwrap(), ("::1".into(), 9, "/a/b".into()));
    assert!(parse_target("host").is_err());
    assert!(parse_target(":99999").is_err());
    assert!(parse_target(":80/a b").is_err());
    assert!(h264_key(&[0x65]) && h264_key(&[0x7C, 0x85]) && h264_key(&[0x78, 0, 4, 0x67]) && !h264_key(&[0x41]) && !h264_key(&[0x7C, 0x05]) && !h264_key(&[]));
}

/// FFmpeg's WHIP muxer (FFmpeg 8 or later on PATH) publishing H.264 and Opus. FFmpeg offers
/// `a=setup:passive` unless told `-whip_flags dtls_active`, and this endpoint is the DTLS server.
#[test]
#[ignore = "needs ffmpeg with the WHIP muxer, libx264 and libopus on PATH"]
fn ffmpeg_publishes() {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let mut s = crate::open_source(&format!("whip://127.0.0.1:{port}/live?token=t0ken")).unwrap();
    let mut ff = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "info", "-re", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "4"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-profile:v", "baseline", "-tune", "zerolatency", "-bf", "0", "-g", "30", "-c:a", "libopus", "-ac", "2", "-ar", "48000"])
        .args(["-whip_flags", "dtls_active", "-authorization", "t0ken", "-f", "whip", &format!("http://127.0.0.1:{port}/live")])
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut got = Vec::new();
    let ended = drain(s.as_mut(), &mut got, Instant::now() + Duration::from_secs(20));
    let _ = ff.wait();
    assert!(ended, "the source ends when FFmpeg DELETEs its session");
    let mut r = ironav_formats::rtp::RtpStream::default();
    r.feed(&got).unwrap();
    r.end();
    let mut n = [0usize; 2];
    while let Some(p) = r.next_packet().unwrap() {
        n[p.packet.stream] += 1;
    }
    let codecs: Vec<_> = r.streams().iter().map(|s| s.codec).collect();
    assert_eq!(codecs, [ironav_core::CodecId::Opus, ironav_core::CodecId::H264].iter().copied().filter(|c| codecs.contains(c)).collect::<Vec<_>>());
    assert!(codecs.len() == 2 && n[0] > 50 && n[1] > 50, "{codecs:?} {n:?}");
}
