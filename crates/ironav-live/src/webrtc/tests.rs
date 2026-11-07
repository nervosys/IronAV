//! The endpoint end to end on loopback, against a test publisher that does what a WebRTC one does:
//! POST an offer, STUN, a DTLS client handshake, then H.264 and Opus over SRTP and a sender report
//! over SRTCP; and against garbage on the same port. Over `whips://` the signalling goes through a
//! TLS client, to a certificate this module issues; the FIPS build runs the flow that way only.

use super::dtls::Client;
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
    let mut client = Client::with_options(Arc::new(dtls::Identity::generate("WHIP test client").unwrap()), [0; 32], profiles.to_vec(), true).unwrap();
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
    sock.send_to(&stun::request(&txid, &format!("{ufrag}:pubU"), &pwd, true, [1, 2, 3, 4, 5, 6, 7, 8]), to).unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = sock.recv_from(&mut buf).unwrap();
    assert_eq!(stun::check_success(&buf[..n], &txid, &pwd), Some(sock.local_addr().unwrap()));

    let mut pending = client.start(Instant::now()).unwrap();
    let started = Instant::now();
    while client.keys.is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "the handshake did not finish");
        for d in pending.drain(..) {
            sock.send_to(&d, to).unwrap();
        }
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        pending = client.handle(&buf[..n], Instant::now()).unwrap();
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

// ---- the publisher, against this module's own endpoint ----

/// The URL of the endpoint on `port` as a publisher is given it, and the options that trust its
/// certificate (the endpoint serves `whips://` as `https://` to a publisher).
fn publisher_target(port: u16, path: &str, tls: bool) -> (String, WhipOptions) {
    let mut opts = WhipOptions::default();
    if tls {
        opts.tls = TLS_PORTS.lock().unwrap().iter().find(|(p, _)| *p == port).map(|(_, c)| c.clone());
        assert!(opts.tls.is_some());
    }
    (format!("{}://127.0.0.1:{port}{path}", if tls { "https" } else { "http" }), opts)
}

/// The H.264 access units of the RTP fixture in Annex B form, with their 90 kHz times from the first:
/// single NAL unit packets, STAP-A and FU-A put back together.
fn access_units() -> Vec<(Vec<u8>, u64)> {
    let (video, _) = media();
    let mut out = Vec::new();
    let (mut au, mut first) = (Vec::new(), None);
    let start = [0u8, 0, 0, 1];
    for p in &video {
        let h = srtp::rtp_header_len(p).unwrap();
        let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        let pl = &p[h..];
        match pl[0] & 31 {
            24 => {
                let mut r = &pl[1..];
                while r.len() >= 2 {
                    let n = u16::from_be_bytes([r[0], r[1]]) as usize;
                    au.extend_from_slice(&start);
                    au.extend_from_slice(&r[2..2 + n]);
                    r = &r[2 + n..];
                }
            }
            28 => {
                if pl[1] & 0x80 != 0 {
                    au.extend_from_slice(&start);
                    au.push((pl[0] & 0xE0) | (pl[1] & 31));
                }
                au.extend_from_slice(&pl[2..]);
            }
            _ => {
                au.extend_from_slice(&start);
                au.extend_from_slice(pl);
            }
        }
        if p[1] & 0x80 != 0 {
            let f = *first.get_or_insert(ts);
            out.push((std::mem::take(&mut au), ts.wrapping_sub(f) as u64));
        }
    }
    out
}

/// The NAL units of an Annex B access unit that a publisher sends (no delimiters or filler).
fn sent_nals(au: &[u8]) -> Vec<Vec<u8>> {
    packetize::split_annexb(au).into_iter().filter(|n| !matches!(n[0] & 31, 9 | 12)).map(<[u8]>::to_vec).collect()
}

fn publisher_publishes(tls: bool, token: Option<&str>) {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let query = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let mut s = open_whip(port, &format!("/live{query}"), tls);
    let (url, opts) = publisher_target(port, "/live", tls);
    let (aus, (_, audio)) = (access_units(), media());
    assert!(aus.len() > 50 && aus.iter().any(|(a, _)| a.len() > 1200), "the fixture has access units that need FU-A");
    let publisher = std::thread::spawn({
        let (aus, audio, token) = (aus.clone(), audio.clone(), token.map(str::to_string));
        move || {
            if token.is_some() {
                // the wrong token, and none, are refused by the endpoint and leave nothing behind
                for t in [None, Some("wrong")] {
                    let e = WhipPublisher::connect(&url, t, opts.clone()).err().expect("refused");
                    assert!(e.message.contains("401") && e.hint.as_deref().is_some_and(|h| h.contains("token")), "{e:?}");
                }
            }
            let mut p = WhipPublisher::connect(&url, token.as_deref(), opts).unwrap();
            // a second publisher is turned away while this one is connected
            // (the endpoint takes one at a time)
            // P frames alone get a request for a keyframe, from the endpoint's PLI
            let p_frame = [0u8, 0, 0, 1, 0x41, 0x9A, 1, 2, 3, 4];
            let started = Instant::now();
            let mut asked = false;
            while !asked && started.elapsed() < Duration::from_secs(8) {
                p.send_h264_access_unit(&p_frame, 0).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                asked = p.keyframe_requested();
            }
            assert!(asked, "the endpoint asks for a keyframe");
            assert!(!p.keyframe_requested(), "and it is told once");
            for (i, (au, pts)) in aus.iter().enumerate() {
                p.send_h264_access_unit(au, *pts).unwrap();
                if i % 2 == 0 && i / 2 < audio.len() {
                    p.send_opus(&audio[i / 2], 960 * (i / 2) as u64).unwrap();
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            // a sender report goes out each second
            std::thread::sleep(Duration::from_millis(1300));
            assert_eq!(p.failure(), None);
            p.close().unwrap();
        }
    });
    let mut got = Vec::new();
    assert!(drain(s.as_mut(), &mut got, Instant::now() + Duration::from_secs(60)), "the source ends after the publisher's DELETE");
    publisher.join().unwrap();

    let f = frames(&got);
    assert_eq!(f[0].0, ironav_formats::rtp::DESCRIPTION_CHANNEL);
    let desc: Description = serde_json::from_slice(&f[0].1).unwrap();
    assert!(desc.sdp.contains("m=video 0 RTP/AVP 102") && desc.sdp.contains("m=audio 0 RTP/AVP 111"), "{}", desc.sdp);
    // the publisher's sender reports reached the endpoint
    assert!(f[1..].iter().any(|(c, _)| c % 2 == 1 && c / 2 == 0 && f.iter().any(|_| true) && !f.is_empty()), "an RTCP packet on the video's channel");
    let mut r = ironav_formats::rtp::RtpStream::default();
    r.feed(&got).unwrap();
    r.end();
    let mut out = Vec::new();
    while let Some(p) = r.next_packet().unwrap() {
        out.push(p);
    }
    let codecs: Vec<_> = r.streams().iter().map(|s| s.codec).collect();
    assert_eq!(codecs, [ironav_core::CodecId::H264, ironav_core::CodecId::Opus]);
    // every Opus packet, as sent
    let opus: Vec<&Vec<u8>> = out.iter().filter(|p| p.packet.stream == 1).map(|p| &p.packet.data).collect();
    let sent_opus = (0..aus.len()).filter(|i| i % 2 == 0 && i / 2 < audio.len()).count();
    assert_eq!(opus, audio[..sent_opus].iter().collect::<Vec<_>>());
    // and every access unit: its NAL units, in order, after the P frames that were sent to be asked for a keyframe
    let video: Vec<Vec<Vec<u8>>> = out.iter().filter(|p| p.packet.stream == 0).map(|p| sent_nals(&p.packet.data)).collect();
    let want: Vec<Vec<Vec<u8>>> = aus.iter().map(|(a, _)| sent_nals(a)).collect();
    assert!(video.len() >= want.len(), "{} access units of {}", video.len(), want.len());
    assert_eq!(video[video.len() - want.len()..], want[..]);
    assert!(out.iter().filter(|p| p.packet.stream == 0 && p.key).count() >= 3, "the fixture's IDR pictures arrive as keyframes");
}

#[test]
fn a_publisher_publishes_to_our_whip_endpoint() {
    // FIPS mode refuses plaintext signalling, so that build publishes through TLS
    publisher_publishes(cfg!(feature = "fips"), Some("s3cret"));
}

#[test]
fn a_publisher_publishes_to_our_whips_endpoint() {
    publisher_publishes(true, None);
}

/// The body of a plain `GET` of `path` on `port`, with the status.
fn get(port: u16, path: &str) -> (u16, String) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut got = Vec::new();
    let _ = c.read_to_end(&mut got);
    let text = String::from_utf8_lossy(&got).to_string();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

/// The number after `"name":` in a JSON text.
fn number_after(json: &str, name: &str) -> Option<u64> {
    let rest = json.split(&format!("\"{name}\":")).nth(1)?;
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// With MediaMTX (a full ICE agent and a DTLS client, answering `a=setup:active`) at the path in
/// `IRONAV_MEDIAMTX`, on a loopback-only configuration written for the test: the publisher connects, the
/// path becomes ready with H264 and Opus and takes bytes, and closing frees it.
/// Run it with `IRONAV_MEDIAMTX=/path/to/mediamtx cargo test -p ironav-live interop_with_mediamtx -- --ignored`.
#[ignore = "needs MediaMTX: set IRONAV_MEDIAMTX to its executable"]
#[test]
fn interop_with_mediamtx() {
    let Ok(exe) = std::env::var("IRONAV_MEDIAMTX") else { panic!("IRONAV_MEDIAMTX is not set") };
    crate::net::allow_loopback_for_tests();
    let (http, udp, api) = (free_port(), free_port(), free_port());
    let dir = std::env::temp_dir().join(format!("ironav-mediamtx-{http}"));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("mtx.yml");
    std::fs::write(
        &cfg,
        format!("logLevel: warn\napi: yes\napiAddress: 127.0.0.1:{api}\nrtsp: no\nrtmp: no\nhls: no\nsrt: no\nmoq: no\nwebrtc: yes\nwebrtcAddress: 127.0.0.1:{http}\nwebrtcLocalUDPAddress: 127.0.0.1:{udp}\nwebrtcIPsFromInterfaces: no\nwebrtcAdditionalHosts: [127.0.0.1]\npaths:\n  live:\n    source: publisher\n"),
    )
    .unwrap();
    struct Server(std::process::Child, std::path::PathBuf);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let _server = Server(std::process::Command::new(&exe).arg(&cfg).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().expect("MediaMTX starts"), dir);
    // the API answers once it is up (a loaded machine takes a while)
    let up = Instant::now();
    while get_status(api) != 200 {
        assert!(up.elapsed() < Duration::from_secs(60), "MediaMTX did not come up");
        std::thread::sleep(Duration::from_millis(200));
    }

    let (aus, (_, audio)) = (access_units(), media());
    let mut p = WhipPublisher::connect(&format!("http://127.0.0.1:{http}/live/whip"), None, WhipOptions::default()).expect("MediaMTX takes the offer");
    let started = Instant::now();
    let mut seen = (false, 0u64);
    let span = aus.last().map_or(0, |a| a.1) + 3000;
    let (mut sent, mut opus) = (0usize, 0u64);
    'send: for round in 0..20u64 {
        for (i, (au, pts)) in aus.iter().enumerate() {
            p.send_h264_access_unit(au, pts + round * span).unwrap();
            if i % 2 == 0 {
                p.send_opus(&audio[(opus as usize) % audio.len()], 960 * opus).unwrap();
                opus += 1;
            }
            sent += 1;
            std::thread::sleep(Duration::from_millis(20));
            if sent % 20 == 0 {
                let (status, body) = get(api, "/v3/paths/get/live");
                assert_eq!(status, 200, "{body}");
                seen = (body.contains("\"ready\":true") && body.contains("H264") && body.contains("Opus"), number_after(&body, "inboundBytes").unwrap_or(0));
                assert_eq!(number_after(&body, "inboundFramesInError").unwrap_or(0), 0, "{body}");
                if seen.0 && seen.1 > 20_000 {
                    break 'send;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(120), "no media reached MediaMTX: {seen:?}");
        }
    }
    assert!(seen.0 && seen.1 > 0, "the path is ready with video and takes bytes: {seen:?}");
    assert_eq!(p.failure(), None);
    p.close().unwrap();
    let (_, body) = get(api, "/v3/paths/get/live");
    assert!(body.contains("\"ready\":false"), "closing frees the path: {body}");
}

fn get_status(port: u16) -> u16 {
    match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => get(port, "/v3/paths/list").0,
        Err(_) => 0,
    }
}

/// A server of one scripted answer after another on a port of its own: each reply is sent to the
/// next connection, and the request heads (and bodies) it saw are returned when it has served them all.
fn scripted_server(replies: Vec<String>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = c.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
                assert!(n > 0, "the client closed early");
            }
            seen.push(String::from_utf8_lossy(&got).to_string());
            c.write_all(reply.as_bytes()).unwrap();
        }
        seen
    });
    (port, h)
}

#[test]
#[cfg_attr(feature = "fips", ignore = "FIPS build: the scripted server speaks plain http, which FIPS mode refuses (fips_publishers_refuse_http covers it)")]
fn a_session_that_cannot_start_is_deleted_and_the_tokens_stay_home() {
    crate::net::allow_loopback_for_tests();
    // 201, a Location on the same server, and an answer that is no SDP: the publisher must DELETE the Location
    let created = "HTTP/1.1 201 Created\r\nLocation: /whip/session/1\r\nContent-Type: application/sdp\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope".to_string();
    let deleted = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    let (port, server) = scripted_server(vec![created, deleted]);
    let (url, opts) = publisher_target(port, "/whip", false);
    let e = WhipPublisher::connect(&url, Some("tok3n"), opts).err().expect("a bad answer");
    assert!(e.message.contains("answer"), "{e:?}");
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("POST /whip HTTP/1.1\r\n") && seen[0].contains("Authorization: Bearer tok3n\r\n") && seen[0].contains("Content-Type: application/sdp\r\n"), "{}", seen[0]);
    assert!(seen[0].contains("a=setup:actpass") && seen[0].contains("a=sendonly") && seen[0].contains("a=rtcp-mux"), "{}", seen[0]);
    assert!(seen[1].starts_with("DELETE /whip/session/1 HTTP/1.1\r\n") && seen[1].contains("Authorization: Bearer tok3n\r\n"), "{}", seen[1]);

    // a Location on another host gets the DELETE but not the token
    let created = "HTTP/1.1 201 Created\r\nLocation: http://localhost:PORT/elsewhere\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope";
    let deleted = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string();
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    let (port, server) = scripted_server(vec![created.replace("PORT", &other_port.to_string())]);
    let other = std::thread::spawn(move || {
        let (mut c, _) = other.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 2048];
        let n = c.read(&mut buf).unwrap();
        c.write_all(deleted.as_bytes()).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    });
    let (url, opts) = publisher_target(port, "/whip", false);
    assert!(WhipPublisher::connect(&url, Some("tok3n"), opts).is_err());
    server.join().unwrap();
    let delete = other.join().unwrap();
    assert!(delete.starts_with("DELETE /elsewhere HTTP/1.1\r\n") && !delete.contains("tok3n") && !delete.contains("Authorization"), "{delete}");

    // anything but 201 is an error that says what the server said, and deletes nothing
    for (reply, wants) in [
        ("HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\nConnection: close\r\n\r\nnot you", "403"),
        ("HTTP/1.1 307 Temporary Redirect\r\nLocation: https://elsewhere.example/whip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "elsewhere.example"),
        ("HTTP/1.1 201 Created\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope", "Location"),
    ] {
        let (port, server) = scripted_server(vec![reply.to_string()]);
        let (url, opts) = publisher_target(port, "/whip", false);
        let e = WhipPublisher::connect(&url, None, opts).err().expect("refused");
        assert!(e.message.contains(wants) || e.hint.as_deref().is_some_and(|h| h.contains(wants)), "{e:?}");
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[test]
#[cfg_attr(feature = "fips", ignore = "FIPS build: the scripted server speaks plain http, which FIPS mode refuses (fips_publishers_refuse_http covers it)")]
fn a_publisher_goes_only_where_the_allow_list_permits() {
    // (the list this process runs with is loopback only)
    crate::net::allow_loopback_for_tests();
    let e = WhipPublisher::connect("http://192.0.2.1:9/whip", None, WhipOptions::default()).err().unwrap();
    assert!(e.message.contains("IRONAV_NET_ALLOW") || e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW")), "{e:?}");
    // an answer that names an address outside it is refused before anything is sent there
    let answer = "v=0\r\no=- 1 2 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuv\r\na=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\na=setup:passive\r\nm=video 5000 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 192.0.2.1\r\na=mid:0\r\na=recvonly\r\na=rtcp-mux\r\na=rtpmap:102 H264/90000\r\na=candidate:1 1 udp 2130706431 192.0.2.1 5000 typ host\r\n";
    let created = format!("HTTP/1.1 201 Created\r\nLocation: /s\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
    let deleted = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    let (port, server) = scripted_server(vec![created, deleted]);
    let (url, opts) = publisher_target(port, "/whip", false);
    let e = WhipPublisher::connect(&url, None, WhipOptions { audio: false, ..opts }).err().expect("an address the list does not permit");
    assert!(e.message.contains("does not permit") && e.message.contains("192.0.2.1"), "{e:?}");
    assert_eq!(server.join().unwrap().len(), 2, "the session was deleted");
}

/// FIPS mode refuses a plaintext endpoint, before anything is sent.
#[cfg(feature = "fips")]
#[test]
fn fips_publishers_refuse_http() {
    let e = WhipPublisher::connect("http://127.0.0.1:9/whip", None, WhipOptions::default()).err().expect("http opened in FIPS mode");
    assert!(e.message.contains("plaintext") && e.message.contains("FIPS"), "{e:?}");
    assert!(e.hint.as_deref().is_some_and(|h| h.contains("https")), "{e:?}");
}

// ---- the WHEP endpoint: a scripted viewer ----

/// A WHEP endpoint on a free port, with `f` setting its options. `tls` serves it over HTTPS, to a
/// certificate this module issues, which `request` then talks to.
fn whep_server(tls: bool, f: impl FnOnce(&mut WhepOptions)) -> (WhepServer, u16) {
    crate::net::allow_loopback_for_tests();
    let port = free_port();
    let mut o = WhepOptions::default();
    if tls {
        let t = tls_fixture(false);
        TLS_PORTS.lock().unwrap().push((port, t.client.clone()));
        o.tls = Some(crate::tls::server_config(&t.cert, &t.key).unwrap());
    }
    f(&mut o);
    (WhepServer::bind(format!("127.0.0.1:{port}").parse().unwrap(), "/whep", o).unwrap(), port)
}

/// A viewer's offer: `recvonly` video offering VP8, two H.264 (the better one last, to see it taken), and
/// `recvonly` Opus audio.
fn viewer_offer(fingerprint: &[u8; 32]) -> String {
    let fp = dtls::fingerprint_text(fingerprint);
    format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE v a\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:viewU\r\na=ice-pwd:viewerpassword01234567\r\n\
         a=fingerprint:sha-256 {fp}\r\na=setup:actpass\r\na=mid:v\r\na=recvonly\r\na=rtcp-mux\r\n\
         a=rtpmap:96 VP8/90000\r\na=rtpmap:97 H264/90000\r\na=fmtp:97 packetization-mode=1;profile-level-id=64001f\r\n\
         a=rtpmap:102 H264/90000\r\na=fmtp:102 packetization-mode=0;profile-level-id=42e01f\r\n\
         a=rtpmap:103 H264/90000\r\na=fmtp:103 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
         a=rtcp-fb:103 nack\r\na=rtcp-fb:103 nack pli\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:a\r\na=recvonly\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\n"
    )
}

/// A viewer that has connected: its socket, SRTP for what it hears, SRTCP for what it says.
struct Watcher {
    sock: UdpSocket,
    to: SocketAddr,
    rx: srtp::Context,
    tx: srtp::Context,
    location: String,
    answer: String,
    ssrc: u32,
    profile: srtp::Profile,
}

/// POSTs an offer, answers the endpoint's STUN check and runs DTLS as the client.
fn watch(port: u16, token: Option<&str>, profiles: &[u16]) -> Watcher {
    let mut client = Client::with_options(Arc::new(dtls::Identity::generate("WHEP test client").unwrap()), [0; 32], profiles.to_vec(), true).unwrap();
    let (status, headers, answer) = request(port, "POST", "/whep", token, &viewer_offer(&client.id.fingerprint));
    assert_eq!(status, 201, "{answer}");
    let location = headers.iter().find(|(k, _)| k == "location").unwrap().1.clone();
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "application/sdp"));
    let fp = attr(&answer, "fingerprint").strip_prefix("sha-256 ").unwrap();
    let fp: Vec<u8> = fp.split(':').map(|h| u8::from_str_radix(h, 16).unwrap()).collect();
    client.server_fingerprint = fp.try_into().unwrap();
    let (ufrag, pwd) = (attr(&answer, "ice-ufrag").to_string(), attr(&answer, "ice-pwd").to_string());
    let candidate: Vec<&str> = attr(&answer, "candidate").split(' ').collect();
    let to: SocketAddr = format!("{}:{}", candidate[4], candidate[5]).parse().unwrap();
    assert_eq!(to.port(), port);
    let ssrc: u32 = attr(&answer, "ssrc").split(' ').next().unwrap().parse().unwrap();

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let txid = [5u8; 12];
    sock.send_to(&stun::request(&txid, &format!("{ufrag}:viewU"), &pwd, true, [1, 2, 3, 4, 5, 6, 7, 8]), to).unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = sock.recv_from(&mut buf).unwrap();
    assert_eq!(stun::check_success(&buf[..n], &txid, &pwd), Some(sock.local_addr().unwrap()));
    let mut pending = client.start(Instant::now()).unwrap();
    let started = Instant::now();
    while client.keys.is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "the handshake did not finish");
        for d in pending.drain(..) {
            sock.send_to(&d, to).unwrap();
        }
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        pending = client.handle(&buf[..n], Instant::now()).unwrap();
    }
    let k = client.keys.clone().unwrap();
    sock.set_read_timeout(None).unwrap();
    Watcher {
        sock,
        to,
        rx: srtp::Context::new(k.profile, &k.server_key, &k.server_salt).unwrap(),
        tx: srtp::Context::new(k.profile, &k.client_key, &k.client_salt).unwrap(),
        location,
        answer,
        ssrc,
        profile: k.profile,
    }
}

/// An RTP packet a viewer received.
#[derive(Clone)]
struct Got {
    seq: u16,
    ts: u32,
    marker: bool,
    pt: u8,
    ssrc: u32,
    payload: Vec<u8>,
    /// The datagram as it came, encrypted.
    raw: Vec<u8>,
}

/// What a viewer has heard: RTP in order, and the RTCP packets of each compound packet.
#[derive(Default)]
struct Heard {
    rtp: Vec<Got>,
    /// (packet type, the whole packet) of every RTCP packet.
    rtcp: Vec<(u8, Vec<u8>)>,
    /// Every datagram that looks like RTP, as it came, replays included.
    raw_rtp: Vec<Vec<u8>>,
    /// Datagrams that were neither (DTLS alerts, STUN).
    other: usize,
}

impl Watcher {
    /// Reads whatever has come, for up to `wait`.
    fn listen(&mut self, heard: &mut Heard, wait: Duration) {
        let until = Instant::now() + wait;
        let mut buf = vec![0u8; 4096];
        while Instant::now() < until {
            // (a plain read with a timeout can lose a datagram on Windows, as net::recv_from_within says)
            let Ok((n, _)) = crate::net::recv_from_within(&self.sock, &mut buf, Duration::from_millis(2)) else { continue };
            let d = &buf[..n];
            if d.len() >= 12 && (128..=191).contains(&d[0]) && (192..=223).contains(&d[1]) {
                let Ok(p) = self.rx.unprotect_rtcp(d) else { continue };
                let mut at = 0;
                while let Some(h) = p.get(at..at + 4) {
                    let len = 4 * (u16::from_be_bytes([h[2], h[3]]) as usize + 1);
                    heard.rtcp.push((h[1], p[at..(at + len).min(p.len())].to_vec()));
                    at += len;
                }
            } else if d.len() >= 12 && (128..=191).contains(&d[0]) {
                heard.raw_rtp.push(d.to_vec());
                let Ok(p) = self.rx.unprotect_rtp(d) else { continue };
                let h = srtp::rtp_header_len(&p).unwrap();
                heard.rtp.push(Got {
                    seq: u16::from_be_bytes([p[2], p[3]]),
                    ts: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
                    marker: p[1] & 0x80 != 0,
                    pt: p[1] & 0x7F,
                    ssrc: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
                    payload: p[h..].to_vec(),
                    raw: d.to_vec(),
                });
            } else {
                heard.other += 1;
            }
        }
    }

    /// Sends an RTCP packet to the endpoint, protected.
    fn say(&mut self, rtcp: &[u8]) {
        let p = self.tx.protect_rtcp(rtcp).unwrap();
        self.sock.send_to(&p, self.to).unwrap();
    }

    fn pli(&mut self, media: u32) {
        let mut p = vec![0x81, 206, 0, 2];
        p.extend_from_slice(&0x5555_6666u32.to_be_bytes());
        p.extend_from_slice(&media.to_be_bytes());
        self.say(&p);
    }

    fn fir(&mut self, media: u32) {
        let mut p = vec![0x84, 206, 0, 4];
        p.extend_from_slice(&0x5555_6666u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&media.to_be_bytes());
        p.extend_from_slice(&[1, 0, 0, 0]);
        self.say(&p);
    }

    fn nack(&mut self, media: u32, first: u16, blp: u16) {
        let mut p = vec![0x81, 205, 0, 3];
        p.extend_from_slice(&0x5555_6666u32.to_be_bytes());
        p.extend_from_slice(&media.to_be_bytes());
        p.extend_from_slice(&first.to_be_bytes());
        p.extend_from_slice(&blp.to_be_bytes());
        self.say(&p);
    }
}

/// The NAL units of each access unit that RFC 6184's receiver makes of the packets (single NAL unit
/// packets and FU-A fragments), an access unit ending at a marker.
fn viewer_access_units(rtp: &[Got]) -> Vec<Vec<Vec<u8>>> {
    let (mut out, mut au, mut open) = (Vec::new(), Vec::new(), None::<Vec<u8>>);
    for g in rtp {
        let p = &g.payload;
        match p[0] & 31 {
            28 => {
                if p[1] & 0x80 != 0 {
                    assert!(open.is_none(), "a fragment starts inside another NAL unit");
                    open = Some(vec![(p[0] & 0xE0) | (p[1] & 31)]);
                }
                open.as_mut().expect("a fragment with no start").extend_from_slice(&p[2..]);
                if p[1] & 0x40 != 0 {
                    au.push(open.take().unwrap());
                }
            }
            t => {
                assert!((1..=23).contains(&t) && open.is_none());
                au.push(p.clone());
            }
        }
        if g.marker {
            out.push(std::mem::take(&mut au));
        }
    }
    assert!(au.is_empty() && open.is_none(), "the last access unit is whole");
    out
}

/// Waits, for up to `wait`, for `done`.
fn until(wait: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

fn whep_watching(profiles: &[u16], tls: bool, with_token: bool) {
    let (url_token, authorize): (Option<&str>, Option<Arc<whep::Authorize>>) = if with_token { (Some("s3cret"), Some(Arc::new(|h: Option<&str>| h == Some("Bearer s3cret")))) } else { (None, None) };
    let (server, port) = whep_server(tls, |o| o.authorize = authorize);
    let tok = url_token;
    if with_token {
        // without the token, or with another, nothing happens, and the refusal says nothing
        for t in [None, Some("wrong")] {
            let (status, headers, body) = request(port, "POST", "/whep", t, &viewer_offer(&[1; 32]));
            assert_eq!((status, body.as_str()), (401, ""));
            assert!(headers.iter().any(|(k, v)| k == "www-authenticate" && v == "Bearer"));
        }
        assert_eq!(request(port, "DELETE", "/whep/session/x", None, "").0, 401);
    }
    assert_eq!(request(port, "OPTIONS", "/whep", None, "").0, 204);
    assert_eq!(request(port, "POST", "/elsewhere", tok, &viewer_offer(&[1; 32])).0, 404);
    assert_eq!(request(port, "GET", "/whep", tok, "").0, 405);
    assert_eq!(server.viewers(), 0);
    let mut w = watch(port, tok, profiles);
    let want = if profiles.contains(&0x0007) { srtp::Profile::AeadAes128Gcm } else { srtp::Profile::Aes128CmHmacSha1_80 };
    assert_eq!(w.profile, want);

    // the answer: ICE-lite, passive, send-only video with the better H.264, audio declined
    let a = w.answer.clone();
    assert!(a.contains("a=ice-lite") && a.contains("a=setup:passive") && a.contains("a=group:BUNDLE v\r\n"), "{a}");
    assert!(a.contains("m=video ") && a.contains("UDP/TLS/RTP/SAVPF 103\r\n") && a.contains("a=sendonly") && !a.contains("a=recvonly"), "{a}");
    assert!(a.contains("a=rtpmap:103 H264/90000") && a.contains("profile-level-id=42e01f") && !a.contains("VP8"), "{a}");
    assert!(a.contains("m=audio 0 UDP/TLS/RTP/SAVPF 111") && a.contains("a=inactive"), "{a}");
    assert_eq!(request(port, "PATCH", &w.location, tok, "").0, 405);

    // the viewer is connected, and a keyframe is wanted for it
    assert!(until(Duration::from_secs(3), || server.viewers() == 1));
    assert!(until(Duration::from_secs(3), || server.keyframe_requested()), "a new viewer raises a keyframe request");
    // nothing is sent to it before a keyframe
    let mut heard = Heard::default();
    let p_frame = [0u8, 0, 0, 1, 0x41, 0x9A, 1, 2, 3, 4];
    server.send_h264_access_unit(&p_frame, 0).unwrap();
    assert!(server.send_h264_access_unit(b"junk", 0).is_err());
    w.listen(&mut heard, Duration::from_millis(200));
    assert!(heard.rtp.is_empty(), "a viewer is sent nothing before the first keyframe");

    let aus: Vec<(Vec<u8>, u64)> = access_units().into_iter().take(150).collect();
    assert!(aus.iter().any(|(a, _)| a.len() > 1200), "the fixture has access units that need FU-A");
    for (au, pts) in &aus {
        server.send_h264_access_unit(au, 1000 + pts).unwrap();
        w.listen(&mut heard, Duration::from_millis(3));
    }
    // sender reports come each second: wait for one that counts everything sent
    let sr_counts = |h: &Heard| h.rtcp.iter().filter(|(t, _)| *t == 200).any(|(_, p)| u32::from_be_bytes([p[20], p[21], p[22], p[23]]) as usize == h.rtp.len());
    let end = Instant::now() + Duration::from_secs(4);
    while !sr_counts(&heard) && Instant::now() < end {
        w.listen(&mut heard, Duration::from_millis(100));
    }

    // every access unit came, whole and unchanged, once and in order
    assert!(heard.rtp.iter().all(|g| g.pt == 103 && g.ssrc == w.ssrc), "the payload type and SSRC are the answer's");
    assert!(heard.rtp.windows(2).all(|p| p[1].seq == p[0].seq.wrapping_add(1)), "sequence numbers run on");
    let got = viewer_access_units(&heard.rtp);
    assert_eq!(got.len(), aus.len());
    for (i, ((au, _), g)) in aus.iter().zip(&got).enumerate() {
        assert_eq!(g, &sent_nals(au), "access unit {i}");
    }
    // timestamps keep the caller's spacing
    let marked: Vec<&Got> = heard.rtp.iter().filter(|g| g.marker).collect();
    for (m, (_, pts)) in marked.iter().zip(&aus) {
        assert_eq!(m.ts.wrapping_sub(marked[0].ts) as u64, pts - aus[0].1);
    }
    // a sender report, naming the stream and counting what was sent
    let sr = heard.rtcp.iter().rfind(|(t, _)| *t == 200).expect("a sender report");
    assert!(sr_counts(&heard), "a sender report counts the packets sent");
    assert_eq!(u32::from_be_bytes([sr.1[4], sr.1[5], sr.1[6], sr.1[7]]), w.ssrc);
    let count = |at: usize| u32::from_be_bytes([sr.1[at], sr.1[at + 1], sr.1[at + 2], sr.1[at + 3]]);
    assert_eq!(count(20) as usize, heard.rtp.len());
    assert_eq!(count(24) as usize, heard.rtp.iter().map(|g| g.payload.len()).sum::<usize>());
    assert!(heard.rtcp.iter().any(|(t, _)| *t == 202), "with an SDES");

    // a NACK is answered from the history: the same packets again, and no others
    let have: Vec<Got> = heard.rtp.clone();
    let (a, b) = (have[10].seq, have[20].seq);
    let mut again = Heard::default();
    w.nack(w.ssrc, a, 0b11); // a, a+1, a+2
    w.nack(w.ssrc, b, 0);
    w.nack(w.ssrc ^ 1, have[30].seq, 0); // not this stream
    w.listen(&mut again, Duration::from_millis(500));
    // (the replay window refuses the copies, so it is the datagrams that count)
    let sent_again: Vec<u16> = again.raw_rtp.iter().map(|d| u16::from_be_bytes([d[2], d[3]])).collect();
    assert_eq!(sent_again.len(), 4, "{sent_again:?}");
    for s in [a, a.wrapping_add(1), a.wrapping_add(2), b] {
        let was = have.iter().find(|g| g.seq == s).unwrap();
        assert!(again.raw_rtp.contains(&was.raw), "{s} was sent again, as it was sent");
    }

    // picture loss and full intra requests raise the flag; a request for another stream does not
    server.keyframe_requested();
    w.pli(w.ssrc ^ 1);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!server.keyframe_requested(), "a request for another SSRC asks for nothing");
    w.pli(w.ssrc);
    assert!(until(Duration::from_secs(2), || server.keyframe_requested()), "a PLI raises the flag");
    assert!(!server.keyframe_requested(), "the flag is read once");
    w.fir(w.ssrc);
    assert!(until(Duration::from_secs(2), || server.keyframe_requested()), "a FIR raises the flag");

    // DELETE ends the session: goodbye, and the session is gone
    let mut bye = Heard::default();
    assert_eq!(request(port, "DELETE", &w.location, tok, "").0, 200);
    w.listen(&mut bye, Duration::from_millis(300));
    assert!(bye.rtcp.iter().any(|(t, _)| *t == 203), "an RTCP BYE");
    assert!(bye.other > 0, "and a DTLS close_notify");
    assert_eq!(server.viewers(), 0);
    assert_eq!(request(port, "DELETE", &w.location, tok, "").0, 404);
    // what is sent now goes nowhere, and does not fail
    server.send_h264_access_unit(&aus[0].0, 0).unwrap();
}

#[test]
fn a_viewer_watches_over_srtp_aes_gcm_with_a_token() {
    // FIPS mode refuses plaintext signalling, so that build serves through TLS
    whep_watching(&[0x0001, 0x0007], crate::fips::enabled(), true);
}

#[test]
fn a_viewer_watches_through_wheps_signalling_over_tls() {
    whep_watching(&[0x0001, 0x0007], true, false);
}

#[test]
#[cfg_attr(feature = "fips", ignore = "FIPS build: AES128_CM_HMAC_SHA1_80 is not offered (dtls tests cover the refusal)")]
fn a_viewer_watches_over_srtp_aes_cm_hmac_sha1() {
    if crate::fips::enabled() {
        return;
    }
    whep_watching(&[0x0001], false, false);
}

#[test]
fn an_offer_a_viewer_cannot_make_is_refused_with_a_400_that_says_why() {
    let (server, port) = whep_server(crate::fips::enabled(), |_| {});
    let good = viewer_offer(&[7; 32]);
    let refused = |offer: &str, why: &str| {
        let (status, _, body) = request(port, "POST", "/whep", None, offer);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains(why), "{body}");
    };
    // no H.264 with packetization-mode=1
    refused(&good.replace("packetization-mode=1", "packetization-mode=0"), "H.264");
    refused(&good.replace("H264/90000", "VP9/90000"), "H.264");
    // a section that only sends is not one to send to, and neither is one that is off
    refused(&good.replace("a=mid:v\r\na=recvonly", "a=mid:v\r\na=sendonly"), "recvonly or sendrecv");
    refused(&good.replace("a=mid:v\r\na=recvonly", "a=mid:v\r\na=inactive"), "recvonly or sendrecv");
    refused(&good.replace("m=video 9", "m=video 0"), "recvonly or sendrecv");
    // audio alone is not taken
    refused(&good.replace("m=video", "m=audio").replace("96 97 102 103", "111"), "video");
    // not a DTLS client
    refused(&good.replace("a=setup:actpass", "a=setup:passive"), "DTLS client");
    refused("v=0\r\n", "media");
    // sendrecv is fine
    let (status, _, answer) = request(port, "POST", "/whep", None, &good.replace("a=mid:v\r\na=recvonly", "a=mid:v\r\na=sendrecv"));
    assert_eq!(status, 201, "{answer}");
    assert!(answer.contains("a=sendonly"), "{answer}");
    let (status, _, _) = request(port, "POST", "/whep", None, "x");
    assert_eq!(status, 400);
    drop(server);
}

#[test]
fn the_viewer_limit_holds_and_a_viewer_that_never_connects_or_goes_quiet_is_dropped() {
    let (server, port) = whep_server(crate::fips::enabled(), |o| {
        o.max_viewers = 2;
        o.setup_timeout = Duration::from_millis(1500);
        o.consent_timeout = Duration::from_millis(700);
    });
    let post = |offer: &str| request(port, "POST", "/whep", None, offer);
    let (s1, h1, _) = post(&viewer_offer(&[1; 32]));
    let (s2, _, _) = post(&viewer_offer(&[2; 32]));
    assert_eq!((s1, s2), (201, 201));
    let (s3, headers, body) = post(&viewer_offer(&[3; 32]));
    assert_eq!(s3, 503, "{body}");
    assert!(headers.iter().any(|(k, _)| k == "retry-after"));
    // a DELETE frees a place at once
    let loc = h1.iter().find(|(k, _)| k == "location").unwrap().1.clone();
    assert_eq!(request(port, "DELETE", &loc, None, "").0, 200);
    assert_eq!(post(&viewer_offer(&[4; 32])).0, 201);
    assert_eq!(post(&viewer_offer(&[5; 32])).0, 503);
    // the two that never connect are dropped after the setup timeout
    std::thread::sleep(Duration::from_millis(1800));
    assert_eq!(post(&viewer_offer(&[6; 32])).0, 201);
    assert_eq!(server.viewers(), 0);
    // a viewer that connects and then says nothing loses consent
    let mut w = watch(port, None, &[0x0001, 0x0007]);
    assert!(until(Duration::from_secs(3), || server.viewers() == 1));
    let mut heard = Heard::default();
    w.listen(&mut heard, Duration::from_millis(1200));
    assert_eq!(server.viewers(), 0, "a viewer with no consent checks is dropped");
    assert_eq!(request(port, "DELETE", &w.location, None, "").0, 404);
}

#[test]
fn a_whep_url_opens_an_endpoint_with_its_token() {
    crate::net::allow_loopback_for_tests();
    if crate::fips::enabled() {
        return;
    }
    let port = free_port();
    let server = WhepServer::open(&format!("whep://127.0.0.1:{port}/w?token=t%2Bk&cors=http://localhost:3000")).unwrap();
    assert_eq!(server.local_addr().port(), port);
    let offer = viewer_offer(&[9; 32]);
    assert_eq!(request(port, "POST", "/w", None, &offer).0, 401);
    assert_eq!(request(port, "POST", "/w", Some("t"), &offer).0, 401);
    let (status, headers, _) = request(port, "POST", "/w", Some("t+k"), &offer);
    assert_eq!(status, 201);
    assert!(headers.iter().any(|(k, v)| k == "access-control-allow-origin" && v == "http://localhost:3000"));
    assert!(headers.iter().any(|(k, v)| k == "location" && v.starts_with("/w/session/")));
    assert!(WhepServer::open("whep://127.0.0.1:0/w?cert=a&key=b").is_err());
    assert!(WhepServer::open("wheps://127.0.0.1:0/w").is_err());
    assert!(WhepServer::open("whep://127.0.0.1:0/w?token=").is_err());
    // port 0 picks a port free for TCP and UDP
    let s = WhepServer::open("whep://127.0.0.1:0").unwrap();
    assert!(s.local_addr().port() != 0);
    assert!(WhepServer::bind("127.0.0.1:0".parse().unwrap(), "no-slash", WhepOptions::default()).is_err());
    let o = WhepOptions { max_viewers: 0, ..WhepOptions::default() };
    assert!(WhepServer::bind("127.0.0.1:0".parse().unwrap(), "/", o).is_err());
}

#[test]
fn a_whep_endpoint_over_tls_is_opened_from_a_url_and_fips_refuses_plain_whep() {
    crate::net::allow_loopback_for_tests();
    let t = tls_fixture(false);
    let s = WhepServer::open(&format!("wheps://127.0.0.1:0/whep?cert={}&key={}", t.cert, t.key)).expect("wheps:// is accepted");
    assert!(s.local_addr().port() != 0);
    if crate::fips::enabled() {
        let e = WhepServer::open("whep://127.0.0.1:0/whep").err().expect("whep:// opened in FIPS mode");
        assert!(e.message.contains("plaintext") && e.hint.as_deref().is_some_and(|h| h.contains("wheps://")), "{e:?}");
    }
}

// ---- the WHEP server's other paths: a page and WebSockets on the same origin ----

const SITE_ORIGIN: &str = "https://viewer.example";

/// What serves the site's paths: a page, a path that tells what it was asked, a WebSocket that echoes,
/// and three that fail to answer well.
fn site(config: websocket::Config) -> Arc<whep::Fallback> {
    Arc::new(move |req: HttpRequest, reply: HttpReply| match req.path() {
        "/" => reply.send(200, &[("Content-Type", "text/html; charset=utf-8"), ("Content-Security-Policy", "default-src 'self'")], b"<h1>viewer</h1>"),
        "/info" => {
            let ua = req.header("USER-AGENT").unwrap_or("-");
            let text = format!("{} {} q={} ua={ua} body={} peer={}", req.method(), req.path(), req.query(), req.body().len(), req.peer_addr().ip());
            reply.send(200, &[("Content-Type", "text/plain"), ("Cache-Control", "max-age=60")], text.as_bytes());
        }
        "/ws" => {
            let Ok(mut c) = reply.upgrade_websocket(&[SITE_ORIGIN], config) else { return };
            // the connection runs on a thread of its own
            std::thread::spawn(move || {
                let _ = c.send_text("hello");
                loop {
                    match c.recv() {
                        Ok(Some(websocket::Message::Text(t))) => {
                            let _ = c.send_text(&format!("echo:{t}"));
                        }
                        Ok(Some(websocket::Message::Binary(b))) => {
                            let _ = c.send_binary(&b.iter().rev().copied().collect::<Vec<u8>>());
                        }
                        _ => break,
                    }
                }
            });
        }
        "/silent" => drop(reply),
        "/panic" => panic!("a fallback that fails"),
        "/badheader" => reply.send(200, &[("X-A", "b\r\nSet-Cookie: x=1")], b""),
        _ => reply.send(404, &[("Content-Type", "text/plain")], b"no such page"),
    })
}

fn site_server(tls: bool, f: impl FnOnce(&mut WhepOptions)) -> (WhepServer, u16) {
    whep_server(tls, |o| {
        o.fallback = Some(site(websocket::Config::default()));
        f(o)
    })
}

#[test]
fn the_fallback_serves_a_page_with_security_headers_and_sees_the_request() {
    for tls in [false, true] {
        if !tls && crate::fips::enabled() {
            continue;
        }
        // authorize refuses everything: it covers the WHEP paths only
        let (server, port) = site_server(tls, |o| o.authorize = Some(Arc::new(|_| false)));
        let (status, h, body) = request(port, "GET", "/", None, "");
        assert_eq!((status, body.as_str()), (200, "<h1>viewer</h1>"));
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(get("content-security-policy"), Some("default-src 'self'"));
        assert_eq!(get("x-content-type-options"), Some("nosniff"));
        assert_eq!(get("cache-control"), Some("no-store"));
        assert_eq!(get("content-length"), Some("15"));
        // the caller's own Cache-Control is kept, and the request's parts are as sent
        let (status, h, body) = request(port, "POST", "/info?a=1&b=%20", None, "xyz");
        assert_eq!(status, 200);
        assert!(body.starts_with("POST /info q=a=1&b=%20 ua=- body=3 peer=127.0.0.1"), "{body}");
        assert_eq!(h.iter().filter(|(k, _)| k == "cache-control").map(|(_, v)| v.as_str()).collect::<Vec<_>>(), ["max-age=60"]);
        assert_eq!(request(port, "GET", "/nothing/here", None, "").0, 404);
        // the WHEP path and its sessions stay the endpoint's, with their check
        assert_eq!(request(port, "POST", "/whep", None, &viewer_offer(&[1; 32])).0, 401);
        assert_eq!(request(port, "DELETE", "/whep/session/abc", None, "").0, 401);
        drop(server);
    }
}

#[test]
fn without_a_fallback_other_paths_are_404() {
    let (server, port) = whep_server(crate::fips::enabled(), |_| {});
    for path in ["/", "/ws", "/info?x=1"] {
        assert_eq!(request(port, "GET", path, None, "").0, 404, "{path}");
    }
    drop(server);
}

#[test]
fn a_fallback_that_fails_to_answer_gives_a_500_and_the_endpoint_goes_on() {
    let (server, port) = site_server(crate::fips::enabled(), |_| {});
    for path in ["/silent", "/panic", "/badheader"] {
        assert_eq!(request(port, "GET", path, None, "").0, 500, "{path}");
    }
    assert_eq!(request(port, "GET", "/", None, "").0, 200);
    assert_eq!(request(port, "POST", "/whep", None, &viewer_offer(&[1; 32])).0, 201);
    drop(server);
}

/// A byte stream the test client reads and writes, plain or TLS.
trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

/// A WebSocket client written by hand: the upgrade request, then masked frames out and plain frames in.
struct WsClient {
    io: Box<dyn Io>,
}

/// Sends an upgrade request for `/ws` with `origin` (if any) and reads the response's head: its
/// status, its headers (names in lower case) and the client, positioned after the head.
fn ws_request(port: u16, origin: Option<&str>) -> (u16, Vec<(String, String)>, WsClient) {
    let c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let tls = TLS_PORTS.lock().unwrap().iter().find(|(p, _)| *p == port).map(|(_, c)| c.clone());
    let mut io: Box<dyn Io> = match tls {
        None => Box::new(c),
        Some(config) => Box::new(TlsStream::connect(c, config, "localhost").unwrap()),
    };
    let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
    let req = format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\n{origin}Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n");
    io.write_all(req.as_bytes()).unwrap();
    io.flush().unwrap();
    // byte by byte: frames the server sends at once must stay unread
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match io.read(&mut b) {
            Ok(1) => head.push(b[0]),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("{e}"),
        }
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.trim_end().split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    (status, headers, WsClient { io })
}

/// An upgraded connection: the 101 is checked (its accept value is the RFC's example).
fn ws_connect(port: u16) -> WsClient {
    let (status, headers, c) = ws_request(port, Some(SITE_ORIGIN));
    assert_eq!(status, 101);
    assert!(headers.iter().any(|(k, v)| k == "sec-websocket-accept" && v == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{headers:?}");
    c
}

impl WsClient {
    fn send(&mut self, opcode: u8, payload: &[u8]) {
        assert!(payload.len() < 126);
        let mask = [9u8, 8, 7, 6];
        let mut f = vec![0x80 | opcode, 0x80 | payload.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.io.write_all(&f).unwrap();
        self.io.flush().unwrap();
    }

    /// The next frame: its opcode and payload.
    fn recv(&mut self) -> (u8, Vec<u8>) {
        let mut h = [0u8; 2];
        self.io.read_exact(&mut h).unwrap();
        assert_eq!(h[1] & 0x80, 0, "a server's frames are not masked");
        let len = match h[1] & 0x7f {
            126 => {
                let mut l = [0u8; 2];
                self.io.read_exact(&mut l).unwrap();
                u16::from_be_bytes(l) as usize
            }
            n => n as usize,
        };
        let mut p = vec![0u8; len];
        self.io.read_exact(&mut p).unwrap();
        (h[0] & 0x0f, p)
    }
}

fn websocket_exchange(tls: bool) {
    let (server, port) = site_server(tls, |_| {});
    let mut c = ws_connect(port);
    assert_eq!(c.recv(), (1, b"hello".to_vec()));
    c.send(1, b"move 10 20");
    assert_eq!(c.recv(), (1, b"echo:move 10 20".to_vec()));
    c.send(2, &[1, 2, 3]);
    assert_eq!(c.recv(), (2, vec![3, 2, 1]));
    // a ping is answered with a pong, and the connection goes on
    c.send(9, b"p");
    assert_eq!(c.recv(), (10, b"p".to_vec()));
    c.send(1, b"again");
    assert_eq!(c.recv(), (1, b"echo:again".to_vec()));
    // a close is answered with a close
    c.send(8, &1000u16.to_be_bytes());
    assert_eq!(c.recv(), (8, 1000u16.to_be_bytes().to_vec()));
    // the page is served meanwhile
    assert_eq!(request(port, "GET", "/", None, "").0, 200);
    drop(server);
}

#[test]
fn a_websocket_upgrades_through_the_fallback_over_plain_tcp() {
    if crate::fips::enabled() {
        return; // FIPS mode serves TLS only
    }
    websocket_exchange(false);
}

#[test]
fn a_websocket_upgrades_through_the_fallback_over_tls() {
    websocket_exchange(true);
}

#[test]
fn an_upgrade_from_another_origin_or_without_one_is_refused_with_a_403() {
    let (server, port) = site_server(crate::fips::enabled(), |_| {});
    for origin in [Some("https://evil.example"), Some("https://viewer.example:8443"), None] {
        let (status, _, _) = ws_request(port, origin);
        assert_eq!(status, 403, "{origin:?}");
    }
    // an origin is compared ignoring case
    let (status, _, _) = ws_request(port, Some("HTTPS://VIEWER.EXAMPLE"));
    assert_eq!(status, 101);
    // a plain GET of the path is no upgrade
    assert_eq!(request(port, "GET", "/ws", None, "").0, 400);
    drop(server);
}

#[test]
fn upgraded_connections_have_a_limit_of_their_own_and_closing_one_frees_its_place() {
    let (server, port) = site_server(crate::fips::enabled(), |o| o.max_websockets = 2);
    let mut a = ws_connect(port);
    let b = ws_connect(port);
    let (status, headers, _) = ws_request(port, Some(SITE_ORIGIN));
    assert_eq!(status, 503);
    assert!(headers.iter().any(|(k, _)| k == "retry-after"));
    // the pages and the signalling do not wait on them
    assert_eq!(request(port, "GET", "/", None, "").0, 200);
    drop(b);
    // its thread sees the end, drops the connection, and the place is free
    assert!(until(Duration::from_secs(5), || ws_request(port, Some(SITE_ORIGIN)).0 == 101));
    assert_eq!(a.recv(), (1, b"hello".to_vec()));
    drop(server);
}

#[test]
fn whep_signalling_works_while_eight_websockets_are_open() {
    let (server, port) = site_server(crate::fips::enabled(), |_| {});
    let mut open: Vec<WsClient> = (0..8).map(|_| ws_connect(port)).collect();
    // the ninth is refused: the default is 8
    assert_eq!(ws_request(port, Some(SITE_ORIGIN)).0, 503);
    // the HTTP server's own eight connections are free for the viewers
    let w = watch(port, None, &[0x0001, 0x0007]);
    assert!(until(Duration::from_secs(3), || server.viewers() == 1));
    assert_eq!(request(port, "DELETE", &w.location, None, "").0, 200);
    for c in open.iter_mut() {
        assert_eq!(c.recv(), (1, b"hello".to_vec()));
        c.send(1, b"x");
        assert_eq!(c.recv(), (1, b"echo:x".to_vec()));
    }
    drop(server);
}

#[test]
fn a_whep_server_takes_at_most_1024_websockets() {
    crate::net::allow_loopback_for_tests();
    let with = |n| WhepOptions { max_websockets: n, ..WhepOptions::default() };
    assert!(WhepServer::bind("127.0.0.1:0".parse().unwrap(), "/whep", with(1025)).is_err());
    assert!(WhepServer::bind("127.0.0.1:0".parse().unwrap(), "/whep", with(1024)).is_ok());
}
