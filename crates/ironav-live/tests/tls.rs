//! `https://` sources against a local TLS 1.3 server whose certificate comes from a throwaway CA,
//! trusted through `IRONAV_CA_FILE`; and one from a CA nobody trusts, refused.

use ironav_core::ErrorCode;
use ironav_live::{open_source, Chunk};
use ironsocketlayer::config::{Identity, Profile, ServerConfig};
use ironsocketlayer::crypto::sign::{KeyKind, SigningKey};
use ironsocketlayer::stream::TlsStream;
use ironsocketlayer::x509::{self, CertificateParams, Purpose};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Ca {
    cert: Vec<u8>,
    key: SigningKey,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn rng() -> ic_drbg::Rng {
    ic_drbg::Rng::from_os().unwrap()
}

fn ca(name: &str) -> Ca {
    let key = SigningKey::generate(KeyKind::EcdsaP256, &mut rng()).unwrap();
    let mut params = CertificateParams::new(name, now() - 3600, now() + 86_400, rng().random_array().unwrap());
    params.is_ca = true;
    params.path_len = Some(0);
    let cert = x509::self_signed(&params, &key, &mut rng()).unwrap();
    Ca { cert, key }
}

/// A server identity for `localhost`, issued by `ca`.
fn identity(ca: &Ca) -> Identity {
    let key = SigningKey::generate(KeyKind::EcdsaP256, &mut rng()).unwrap();
    let names = ["localhost"];
    let mut params = CertificateParams::new("localhost", now() - 3600, now() + 86_400, rng().random_array().unwrap());
    params.dns_names = &names;
    params.usage = &[Purpose::TlsServer];
    let cert = x509::issue(&params, key.spki(), &ca.cert, &ca.key, &mut rng()).unwrap();
    Identity::new(vec![cert], key).unwrap()
}

fn pem(der: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b = String::new();
    for c in der.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            b.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    let lines: Vec<&str> = b.as_bytes().chunks(64).map(|l| std::str::from_utf8(l).unwrap()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}

/// An HTTPS server on localhost answering GETs from `routes`, one request per connection.
fn server(id: Identity, routes: Vec<(&'static str, Vec<u8>)>) -> u16 {
    let config = Arc::new(ServerConfig::new(Profile::Default, id).unwrap().with_alpn(&[b"http/1.1"]));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(c) = c else { continue };
            let Ok(mut s) = TlsStream::accept(c, config.clone()) else { continue };
            let mut req = Vec::new();
            let mut b = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&b[..n]),
                }
            }
            let path = String::from_utf8_lossy(&req).split_whitespace().nth(1).unwrap_or("").to_string();
            match routes.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(body);
                }
                None => {
                    let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
            }
            let _ = s.close();
        }
    });
    port
}

fn drain(url: &str) -> ironav_core::Result<Vec<u8>> {
    ironav_live::net::allow_loopback_for_tests();
    let mut s = open_source(url)?;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 65536];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        match s.read(&mut buf, Duration::from_millis(50))? {
            Chunk::Data(n) => got.extend_from_slice(&buf[..n]),
            Chunk::Idle | Chunk::Restart => {}
            Chunk::End => break,
        }
    }
    Ok(got)
}

#[test]
fn https_downloads_and_playlists_are_verified_against_the_trusted_roots() {
    let trusted = ca("IronAV Test Root");
    let dir = std::env::temp_dir().join(format!("ironav-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, pem(&trusted.cert)).unwrap();
    // read once, at the first TLS connection of this process
    std::env::set_var(ironav_live::CA_FILE_VAR, &ca_file);

    let clip = std::fs::read(format!("{}/../ironav-formats/tests/data/h264_aac.ts", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let playlist = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nclip.ts\n#EXT-X-ENDLIST\n".to_vec();
    let mixed = b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
http://localhost/clip.ts
#EXT-X-ENDLIST
".to_vec();
    let port = server(identity(&trusted), vec![("/clip.ts", clip.clone()), ("/live/index.m3u8", playlist), ("/live/clip.ts", clip.clone()), ("/mixed.m3u8", mixed)]);

    // a progressive download, and an HLS playlist whose segment is resolved against https://
    assert!(drain(&format!("https://localhost:{port}/clip.ts")).unwrap() == clip);
    assert!(drain(&format!("https://localhost:{port}/live/index.m3u8")).unwrap() == clip);
    assert_eq!(drain(&format!("https://localhost:{port}/missing.ts")).unwrap_err().code, ErrorCode::NotFound);
    // a playlist reached over TLS does not send its segments outside it
    assert_eq!(drain(&format!("https://localhost:{port}/mixed.m3u8")).unwrap_err().code, ErrorCode::UnsupportedFormat);

    // the certificate names localhost, not the address
    let e = drain(&format!("https://127.0.0.1:{port}/clip.ts")).unwrap_err();
    assert!(e.message.contains("certificate-name-mismatch"), "{e:?}");
    assert!(e.hint.as_deref().unwrap_or("").contains("host name"), "{e:?}");

    // a server whose CA is not trusted is refused, with a hint about IRONAV_CA_FILE
    let stranger = ca("Somebody Else's Root");
    let port = server(identity(&stranger), vec![("/clip.ts", clip)]);
    let e = drain(&format!("https://localhost:{port}/clip.ts")).unwrap_err();
    assert_eq!(e.code, ErrorCode::Io, "{e:?}");
    assert!(e.message.contains("TLS with"), "{e:?}");
    assert!(e.hint.as_deref().unwrap_or("").contains("IRONAV_CA_FILE"), "{e:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
