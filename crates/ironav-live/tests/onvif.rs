//! ONVIF against a fake camera on loopback: discovery, the clock, the token, and the profiles.

use ironav_live::onvif::{discover, password_digest, profiles};
use ironav_live::Allow;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let i = s.find(open)? + open.len();
    let j = s[i..].find(close)?;
    Some(&s[i..i + j])
}

fn b64decode(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        acc = acc << 6 | A.iter().position(|&a| a == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

const ENV: &str = r#"<?xml version="1.0" encoding="UTF-8"?><env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><env:Body>"#;

/// A camera whose clock says 2031, which accepts `admin`/`secret` and serves two profiles.
fn camera() -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for conn in l.incoming() {
            let mut conn = conn.unwrap();
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let (mut line, mut len, mut path) = (String::new(), 0, String::new());
            r.read_line(&mut line).unwrap();
            path.push_str(line.split_whitespace().nth(1).unwrap());
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let action = between(&body, "<s:Body><", " ").unwrap().to_string();
            seen.push(format!("{path} {action}"));
            if action == "Stop" {
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                break;
            }
            let authed = match between(&body, "<Username>", "</Username>") {
                Some(user) => {
                    let pass = between(&body, "PasswordDigest\">", "</Password>").unwrap();
                    let nonce = b64decode(between(&body, "Base64Binary\">", "</Nonce>").unwrap());
                    let created = between(&body, "utility-1.0.xsd\">", "</Created>").unwrap();
                    assert!(created.starts_with("2031-04-05T06:0"), "{created}");
                    user == "admin" && pass == password_digest(&nonce, created, "secret")
                }
                None => false,
            };
            let (status, answer) = if action == "GetSystemDateAndTime" {
                (200, "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>6</tt:Hour><tt:Minute>7</tt:Minute><tt:Second>8</tt:Second></tt:Time><tt:Date><tt:Year>2031</tt:Year><tt:Month>4</tt:Month><tt:Day>5</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>".to_string())
            } else if !authed {
                (400, "<env:Fault><env:Code><env:Value>env:Sender</env:Value><env:Subcode><env:Value>ter:NotAuthorized</env:Value></env:Subcode></env:Code><env:Reason><env:Text xml:lang=\"en\">Sender not Authorized</env:Text></env:Reason></env:Fault>".to_string())
            } else {
                let media = format!("http://10.9.8.7:{port}/onvif/media_service");
                match action.as_str() {
                    "GetDeviceInformation" => (200, "<tds:GetDeviceInformationResponse><tds:Manufacturer>Acme &amp; Co</tds:Manufacturer><tds:Model>Eye-2</tds:Model><tds:FirmwareVersion>1.2</tds:FirmwareVersion><tds:SerialNumber>SN1</tds:SerialNumber><tds:HardwareId>H</tds:HardwareId></tds:GetDeviceInformationResponse>".into()),
                    "GetServices" => (200, format!("<tds:GetServicesResponse><tds:Service><tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace><tds:XAddr>http://127.0.0.1:{port}/onvif/device_service</tds:XAddr></tds:Service><tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace><tds:XAddr>{media}</tds:XAddr></tds:Service></tds:GetServicesResponse>")),
                    "GetProfiles" => (200, "<trt:GetProfilesResponse><trt:Profiles token=\"main\" fixed=\"true\"><tt:Name>mainStream</tt:Name><tt:VideoEncoderConfiguration token=\"v0\"><tt:Name>enc</tt:Name><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution><tt:RateControl><tt:FrameRateLimit>25</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>4096</tt:BitrateLimit></tt:RateControl></tt:VideoEncoderConfiguration><tt:AudioEncoderConfiguration token=\"a0\"><tt:Name>a</tt:Name><tt:Encoding>G711</tt:Encoding><tt:Bitrate>64</tt:Bitrate><tt:SampleRate>8</tt:SampleRate></tt:AudioEncoderConfiguration></trt:Profiles><trt:Profiles token=\"sub\"><tt:Name>subStream</tt:Name><tt:VideoEncoderConfiguration><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>640</tt:Width><tt:Height>360</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration></trt:Profiles></trt:GetProfilesResponse>".into()),
                    "GetStreamUri" => {
                        let token = between(&body, "<ProfileToken>", "</ProfileToken>").unwrap();
                        assert!(body.contains("<Protocol>RTSP</Protocol>"));
                        (200, format!("<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>rtsp://10.9.8.7:554/Streaming/{token}?transport=tcp&amp;x=1</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect></trt:MediaUri></trt:GetStreamUriResponse>"))
                    }
                    "GetSnapshotUri" => {
                        let token = between(&body, "<ProfileToken>", "</ProfileToken>").unwrap();
                        if token == "sub" {
                            (500, "<env:Fault><env:Code><env:Value>env:Receiver</env:Value><env:Subcode><env:Value>ter:ActionNotSupported</env:Value></env:Subcode></env:Code><env:Reason><env:Text>no</env:Text></env:Reason></env:Fault>".into())
                        } else {
                            (200, format!("<trt:GetSnapshotUriResponse><trt:MediaUri><tt:Uri><![CDATA[http://10.9.8.7/snap.jpg?p={token}&q=1]]></tt:Uri></trt:MediaUri></trt:GetSnapshotUriResponse>"))
                        }
                    }
                    other => panic!("unexpected {other}"),
                }
            };
            let xml = format!("{ENV}{answer}</env:Body></env:Envelope>");
            let _ = write!(conn, "HTTP/1.1 {status} X\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len());
        }
        seen
    });
    (port, h)
}

fn stop(port: u16) {
    let body = r#"<s:Envelope xmlns:s="x"><s:Body><Stop xmlns="y"/></s:Body></s:Envelope>"#;
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(c, "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut s = String::new();
    let _ = c.read_to_string(&mut s);
}

#[cfg_attr(feature = "fips", ignore = "FIPS build: exercises plaintext transports or MD5 Digest, which it refuses (tests/fips.rs covers the refusals)")]
#[test]
fn profiles_with_the_cameras_clock() {
    let (port, h) = camera();
    let dev = profiles(&format!("http://127.0.0.1:{port}"), Some("admin"), Some("secret"), &Allow::parse("loopback").unwrap()).unwrap();
    assert_eq!(dev.manufacturer.as_deref(), Some("Acme & Co"));
    assert_eq!(dev.model.as_deref(), Some("Eye-2"));
    assert_eq!(dev.serial.as_deref(), Some("SN1"));
    // the media service named a host only its own network knows, so it is reached by ours
    assert_eq!(dev.media, format!("http://127.0.0.1:{port}/onvif/media_service"));
    assert_eq!(dev.media_version, 1);
    assert_eq!(dev.profiles.len(), 2);
    let main = &dev.profiles[0];
    assert_eq!((main.token.as_str(), main.name.as_str()), ("main", "mainStream"));
    assert_eq!((main.video.as_deref(), main.width, main.height, main.fps, main.bitrate_kbps), (Some("H264"), Some(1920), Some(1080), Some(25.0), Some(4096)));
    assert_eq!((main.audio.as_deref(), main.sample_rate), (Some("G711"), Some(8000)));
    assert_eq!(main.stream_uri.as_deref(), Some("rtsp://10.9.8.7:554/Streaming/main?transport=tcp&x=1"));
    assert_eq!(main.snapshot_uri.as_deref(), Some("http://10.9.8.7/snap.jpg?p=main&q=1"));
    let sub = &dev.profiles[1];
    assert_eq!((sub.width, sub.audio.as_deref(), sub.snapshot_uri.as_deref()), (Some(640), None, None));
    assert_eq!(sub.stream_uri.as_deref(), Some("rtsp://10.9.8.7:554/Streaming/sub?transport=tcp&x=1"));

    // a wrong password is the caller's to fix
    let e = profiles(&format!("http://admin:nope@127.0.0.1:{port}/onvif/device_service"), None, None, &Allow::parse("loopback").unwrap()).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument, "{e:?}");
    assert!(e.message.contains("Sender not Authorized"), "{}", e.message);
    stop(port);
    let seen = h.join().unwrap();
    assert!(seen.contains(&"/onvif/media_service GetStreamUri".to_string()), "{seen:?}");
    assert_eq!(seen[0], "/onvif/device_service GetSystemDateAndTime");
}

#[cfg_attr(feature = "fips", ignore = "FIPS build: exercises plaintext transports or MD5 Digest, which it refuses (tests/fips.rs covers the refusals)")]
#[test]
fn discovery_answers() {
    let cam = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = cam.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        cam.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
        // two probes may come (one resent); answer both, the same device
        let mut probes = 0;
        while let Ok((n, from)) = cam.recv_from(&mut buf) {
            let probe = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(probe.contains("dn:NetworkVideoTransmitter"));
            let id = between(&probe, "<a:MessageID>", "</a:MessageID>").unwrap();
            for relates in ["uuid:someone-else", id] {
                let m = format!(r#"<?xml version="1.0"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"><SOAP-ENV:Header><wsa:RelatesTo>{relates}</wsa:RelatesTo></SOAP-ENV:Header><SOAP-ENV:Body><d:ProbeMatches><d:ProbeMatch><wsa:EndpointReference><wsa:Address>urn:uuid:cam-1</wsa:Address></wsa:EndpointReference><d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types><d:Scopes>onvif://www.onvif.org/name/Eye onvif://www.onvif.org/location/door</d:Scopes><d:XAddrs>http://192.168.1.{probes}/onvif/device_service</d:XAddrs></d:ProbeMatch></d:ProbeMatches></SOAP-ENV:Body></SOAP-ENV:Envelope>"#);
                cam.send_to(m.as_bytes(), from).unwrap();
            }
            probes += 1;
        }
        probes
    });
    let found = discover(&addr.to_string(), Duration::from_millis(600)).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    let d = &found[0];
    assert_eq!(d.endpoint, "urn:uuid:cam-1");
    assert_eq!(d.from, "127.0.0.1");
    assert_eq!(d.scopes, ["onvif://www.onvif.org/name/Eye", "onvif://www.onvif.org/location/door"]);
    assert_eq!(d.types.len(), 2);
    assert!(d.xaddrs[0].starts_with("http://192.168.1.0/"));
    assert!(h.join().unwrap() >= 1);
}
