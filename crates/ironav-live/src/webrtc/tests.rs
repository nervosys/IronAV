//! The endpoint end to end on loopback, against a test publisher that does what a WebRTC one does:
//! POST an offer, STUN, a DTLS client handshake, then H.264 and Opus over SRTP and a sender report
//! over SRTCP; and against garbage on the same port. Over `whips://` the signalling goes through a
//! TLS client, to a certificate this module issues; the FIPS build runs the flow that way only.

use super::dtls::client::Client;
use super::*;
use ironav_formats::stream::{stream_demuxer, StreamDemuxer, StreamOptions};
use ironsocketlayer::config::{ClientConfig, Profile};
use ironsocketlayer::crypto::sign::SigningKey;
use ironsocketlayer::stream::TlsStream;
use ironsocketlayer::x509::{self, CertificateParams, Purpose, RootStore};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

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

/// A certificate chain and private key in PEM files, issued by a CA of their own, and the TLS client
/// configuration that trusts that CA.
struct Tls {
    cert: String,
    key: String,
    client: Arc<ClientConfig>,
}

/// The TLS configuration of each port a test serves `whips://` on, for [`request`] to find.
static TLS_PORTS: Mutex<Vec<(u16, Arc<ClientConfig>)>> = Mutex::new(Vec::new());

fn pem(label: &str, der: &[u8]) -> String {
    let b64 = crate::http::base64(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|l| std::str::from_utf8(l).unwrap()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// Issues a CA and a P-256 certificate for localhost and 127.0.0.1, and writes the chain (leaf, then
/// CA) and the key (PKCS#8, or SEC1 as `EC PRIVATE KEY`) to files of their own.
fn tls_fixture(sec1: bool) -> Tls {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let rng = || ic_drbg::Rng::from_os().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let ca_key = SigningKey::generate(ironsocketlayer::crypto::sign::KeyKind::EcdsaP256, &mut rng()).unwrap();
    let mut p = CertificateParams::new("IronAV WHIP Test Root", now - 3600, now + 86_400, rng().random_array().unwrap());
    p.is_ca = true;
    p.path_len = Some(0);
    let ca = x509::self_signed(&p, &ca_key, &mut rng()).unwrap();
    let scalar: [u8; 32] = rng().random_array().unwrap();
    let key = SigningKey::ecdsa_p256(&scalar).unwrap();
    let (dns, ips) = (["localhost"], [x509::IpAddr::V4([127, 0, 0, 1])]);
    let mut p = CertificateParams::new("localhost", now - 3600, now + 86_400, rng().random_array().unwrap());
    p.dns_names = &dns;
    p.ip_addresses = &ips;
    p.usage = &[Purpose::TlsServer];
    let leaf = x509::issue(&p, key.spki(), &ca, &ca_key, &mut rng()).unwrap();
    let sec1_der: Vec<u8> = [&[0x30, 0x25, 0x02, 0x01, 0x01, 0x04, 0x20][..], &scalar].concat();
    let key_pem = if sec1 {
        pem("EC PRIVATE KEY", &sec1_der)
    } else {
        // PKCS#8: version 0, id-ecPublicKey with prime256v1, and the SEC1 key in an OCTET STRING
        let head = [0x30, 0x41, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x27];
        pem("PRIVATE KEY", &[&head[..], &sec1_der].concat())
    };
    let dir = std::env::temp_dir().join(format!("ironav-whips-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let (cert, key) = (dir.join("chain.pem"), dir.join("key.pem"));
    std::fs::write(&cert, pem("CERTIFICATE", &leaf) + &pem("CERTIFICATE", &ca)).unwrap();
    std::fs::write(&key, key_pem).unwrap();
    let mut roots = RootStore::new();
    roots.add_pem_bundle(&pem("CERTIFICATE", &ca)).unwrap();
    let client = if crate::fips::enabled() { crate::fips::client_config(roots).unwrap() } else { ClientConfig::new(Profile::Default, roots).unwrap() };
    let slashed = |p: std::path::PathBuf| p.to_str().unwrap().replace('\\', "/");
    Tls { cert: slashed(cert), key: slashed(key), client: Arc::new(client) }
}

/// Opens the endpoint on `port`: `whip://`, or `whips://` with a certificate issued for it, which
/// `request` then talks to over TLS. `rest` is the path and query, `/live?token=T`.
fn open_whip(port: u16, rest: &str, tls: bool) -> Box<dyn Source> {
    if !tls {
        return crate::open_source(&format!("whip://127.0.0.1:{port}{rest}")).unwrap();
    }
    let t = tls_fixture(false);
    TLS_PORTS.lock().unwrap().push((port, t.client.clone()));
    let sep = if rest.contains('?') { '&' } else { '?' };
    crate::open_source(&format!("whips://127.0.0.1:{port}{rest}{sep}cert={}&key={}", t.cert, t.key)).unwrap()
}

/// One HTTP request to the endpoint, over TLS if the port serves `whips://`: the status, the
/// headers and the body.
fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Vec<(String, String)>, String) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let tls = TLS_PORTS.lock().unwrap().iter().find(|(p, _)| *p == port).map(|(_, c)| c.clone());
    let mut got = Vec::new();
    match tls {
        None => {
            c.write_all(req.as_bytes()).unwrap();
            c.read_to_end(&mut got).unwrap();
        }
        Some(config) => {
            let mut t = TlsStream::connect(c, config, "localhost").unwrap();
            t.write_all(req.as_bytes()).unwrap();
            match t.read_to_end(&mut got) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                Err(e) => panic!("{e}"),
            }
        }
    }
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

fn publishes(profiles: &[u16], token: Option<&str>, tls: bool) {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let query = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let mut s = open_whip(port, &format!("/live{query}"), tls);
    assert_eq!(s.info().kind, "whip");
    assert!(!s.info().url.contains("s3cret"), "{}", s.info().url);
    assert_eq!(s.info().url.contains("whips://"), tls, "{}", s.info().url);
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
    // every packet sent must come, unchanged, once each, in order (loopback with the endpoint's
    // receive buffer loses none; on Windows that needs net::recv_from_within)
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
    assert!(missing.is_empty(), "missing {missing:?}");
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
    assert_eq!(opus, audio.iter().collect::<Vec<_>>());
    let video: Vec<_> = out.iter().filter(|p| p.packet.stream == 0).collect();
    assert!(video.len() > 10 && video[0].key);
    assert!(r.wall_clock(0).is_some(), "the sender report reached the video track");
}

#[test]
fn publishes_over_srtp_aes_gcm_with_a_token() {
    // FIPS mode refuses plaintext signalling, so that build publishes through TLS
    publishes(&[0x0001, 0x0007], Some("s3cret"), cfg!(feature = "fips"));
}

#[test]
fn publishes_through_whips_signalling_over_tls() {
    publishes(&[0x0001, 0x0007], Some("s3cret"), true);
}

#[test]
#[cfg_attr(feature = "fips", ignore = "FIPS build: AES128_CM_HMAC_SHA1_80 is not offered (dtls tests cover the refusal)")]
fn publishes_over_srtp_aes_cm_hmac_sha1() {
    publishes(&[0x0001], None, false);
}

#[test]
fn a_delete_that_comes_after_the_session_ended_is_still_answered() {
    // a browser may close DTLS and then DELETE; here the session ends by its media stopping instead
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let mut s = open_whip(port, "?token=t0k", cfg!(feature = "fips"));
    let p = publish(port, "/", Some("t0k"), &[0x0007]);
    let started = Instant::now();
    let mut buf = vec![0u8; 65536];
    // the media timeout ends the source with an error
    while started.elapsed() < MEDIA_TIMEOUT + Duration::from_secs(10) {
        match s.read(&mut buf, Duration::from_millis(50)) {
            Ok(Chunk::End) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // counted from the handshake, a little before `started`
    assert!(started.elapsed() + Duration::from_secs(2) >= MEDIA_TIMEOUT && started.elapsed() < MEDIA_TIMEOUT + Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(request(port, "DELETE", "/session/nope", Some("t0k"), "").0, 404);
    assert_eq!(request(port, "DELETE", &p.location, Some("t0k"), "").0, 200);
}

#[test]
fn cors_is_offered_only_to_the_origin_given() {
    for o in ["https://studio.example.com", "http://localhost:3000", "http://[::1]:8080", "http://127.0.0.1"] {
        assert!(is_origin(o), "{o}");
    }
    for o in ["", "studio.example.com", "https://", "https://a.example/path", "https://a.example:", "https://a b", "https://a\r\nX: y", "ftp://a.example", "https://a.example:123456"] {
        assert!(!is_origin(o), "{o:?}");
    }
    crate::net::allow_loopback_for_tests();
    let tls = cfg!(feature = "fips");
    let preflight = |port: u16| request(port, "OPTIONS", "/live", None, "");
    let port = free_port();
    let _s = open_whip(port, "/live?cors=http%3A%2F%2Flocalhost%3A3000&token=t0k", tls);
    let (status, headers, _) = preflight(port);
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(status, 204);
    assert_eq!(h("access-control-allow-origin"), Some("http://localhost:3000"));
    assert_eq!(h("vary"), Some("Origin"));
    assert!(h("access-control-allow-headers").is_some_and(|v| v.contains("Authorization")));
    assert!(h("access-control-expose-headers").is_some_and(|v| v.contains("Location")));
    // a refusal carries them too, so the page can read why
    let (status, headers, _) = request(port, "POST", "/live", None, "v=0\r\n");
    assert_eq!(status, 401);
    assert!(headers.iter().any(|(k, _)| k == "access-control-allow-origin"));
    let port = free_port();
    let _s = open_whip(port, "/live", tls);
    assert!(preflight(port).1.iter().all(|(k, _)| !k.starts_with("access-control-")));
    assert!(crate::open_source(&format!("whip://127.0.0.1:{}/live?cors=evil.example", free_port())).is_err());
}

#[test]
fn a_publisher_that_never_connects_frees_the_endpoint() {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let mut s = open_whip(port, "", cfg!(feature = "fips"));
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
fn whips_needs_a_certificate_and_a_key_and_whip_takes_neither() {
    crate::net::allow_loopback_for_tests();
    let t = tls_fixture(false);
    let (cert, key) = (&t.cert, &t.key);
    for url in ["whips://127.0.0.1:0".to_string(), format!("whips://127.0.0.1:0?cert={cert}"), format!("whips://127.0.0.1:0?key={key}"), format!("whips://127.0.0.1:0?key={key}&token=t")] {
        let e = crate::open_source(&url).err().unwrap_or_else(|| panic!("{url} opened"));
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{url}");
        assert!(e.message.contains("cert=") && e.message.contains("key="), "{e:?}");
    }
    // (FIPS mode refuses whip:// before it looks at the options)
    let e = crate::open_source(&format!("whip://127.0.0.1:0?cert={cert}&key={key}")).err().expect("whip:// with cert=");
    assert!(e.message.contains("whips://") || crate::fips::enabled(), "{e:?}");
    // files that are missing, or not what they should be, say which and why
    let e = crate::open_source(&format!("whips://127.0.0.1:0?cert={cert}.nope&key={key}")).err().unwrap();
    assert!(e.message.contains("cannot read cert") && e.message.contains("nope"), "{e:?}");
    let e = crate::open_source(&format!("whips://127.0.0.1:0?cert={key}&key={key}")).err().unwrap();
    assert!(e.message.contains("no CERTIFICATE"), "{e:?}");
    let e = crate::open_source(&format!("whips://127.0.0.1:0?cert={cert}&key={cert}")).err().unwrap();
    assert!(e.message.contains("no PRIVATE KEY"), "{e:?}");
    // a key that is not the certificate's
    let other = tls_fixture(false);
    let e = crate::open_source(&format!("whips://127.0.0.1:0?cert={cert}&key={}", other.key)).err().unwrap();
    assert!(e.message.contains("key"), "{e:?}");
}

#[test]
fn keys_load_in_both_pem_forms() {
    for sec1 in [false, true] {
        let t = tls_fixture(sec1);
        crate::tls::server_config(&t.cert, &t.key).unwrap_or_else(|e| panic!("sec1={sec1}: {e:?}"));
    }
    assert!(crate::tls::unbase64("AQID").unwrap() == [1, 2, 3] && crate::tls::unbase64("AQI=").unwrap() == [1, 2] && crate::tls::unbase64("A*").is_none());
}

#[test]
fn a_failed_tls_handshake_drops_only_that_connection() {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let _s = open_whip(port, "/live", true);
    // plain HTTP to the TLS port, and a client that says nothing, then a good request
    let mut plain = TcpStream::connect(("127.0.0.1", port)).unwrap();
    plain.write_all(b"OPTIONS /live HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    plain.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
    silent.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let started = Instant::now();
    assert_eq!(request(port, "OPTIONS", "/live", None, "").0, 204);
    assert!(started.elapsed() < Duration::from_secs(2), "the endpoint did not wait on the others");
    let mut got = Vec::new();
    let _ = plain.read_to_end(&mut got);
    assert!(!got.starts_with(b"HTTP/"), "no HTTP answer in the clear");
    // the silent one is let go once the handshake time is up
    let _ = silent.read_to_end(&mut Vec::new());
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    assert_eq!(request(port, "OPTIONS", "/live", None, "").0, 204);
}

/// FIPS mode refuses plaintext signalling and takes TLS.
#[cfg(feature = "fips")]
#[test]
fn fips_refuses_whip_and_accepts_whips() {
    crate::net::allow_loopback_for_tests();
    let e = crate::open_source("whip://127.0.0.1:0/live").err().expect("whip:// opened in FIPS mode");
    assert!(e.message.contains("plaintext") && e.message.contains("FIPS"), "{e:?}");
    assert!(e.hint.as_deref().is_some_and(|h| h.contains("whips://") && h.contains("cert=") && h.contains("key=")), "{e:?}");
    let t = tls_fixture(false);
    let s = crate::open_source(&format!("whips://127.0.0.1:0/live?cert={}&key={}", t.cert, t.key)).expect("whips:// is accepted");
    assert_eq!(s.info().kind, "whip");
}

#[test]
fn targets_parse() {
    assert_eq!(parse_target(":8080/live", "whip").unwrap(), ("127.0.0.1".into(), 8080, "/live".into()));
    assert_eq!(parse_target("0.0.0.0:8080", "whip").unwrap(), ("0.0.0.0".into(), 8080, "/".into()));
    assert_eq!(parse_target("[::1]:9/a/b/", "whip").unwrap(), ("::1".into(), 9, "/a/b".into()));
    assert!(parse_target("host", "whip").is_err());
    assert!(parse_target(":99999", "whip").is_err());
    assert!(parse_target(":80/a b", "whip").is_err());
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
