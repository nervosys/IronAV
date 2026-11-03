//! TLS for `https://` and `rtsps://`: IronSocketLayer (TLS 1.3 over IronCrypto) on the connection,
//! and IronPKI, through it, to validate the server's certificate path and name. The trusted roots
//! are the platform's PEM bundle (`SSL_CERT_FILE`, the usual Linux, BSD and macOS paths, or Git for
//! Windows' bundle) together with `IRONAV_CA_FILE`, a PEM file of further roots set by whoever runs
//! IronAV (a camera's own CA, say). Verification cannot be turned off.

use ironav_core::{Error, ErrorCode, Result};
use ironsocketlayer::config::{ClientConfig, Identity, Profile, ServerConfig};
use ironsocketlayer::crypto::sign::SigningKey;
use ironsocketlayer::stream::TlsStream;
use ironsocketlayer::x509::RootStore;
use std::cell::Cell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The environment variable naming a PEM file of further trusted roots.
pub const CA_FILE_VAR: &str = "IRONAV_CA_FILE";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection, plain or under TLS. The socket's own timeouts govern both: under TLS a read that
/// times out leaves the stream usable, since partial records wait in the TLS engine.
pub(crate) enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<Sock>>),
}

impl Conn {
    /// The TCP socket underneath, for timeouts and addresses.
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(t) => &t.get_ref().sock,
        }
    }

    /// Ends a TLS session cleanly (close_notify); nothing for a plain one.
    pub fn close(&mut self) {
        if let Conn::Tls(t) = self {
            let _ = t.close();
        }
    }
}

/// A TLS stream whose socket write failed has lost part of a record it had already committed to,
/// so the connection is over, even if the failure was a timeout (which on a read is harmless).
fn broken(t: &TlsStream<Sock>) -> std::io::Result<()> {
    if t.get_ref().broken.get() {
        return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the TLS connection failed while sending and cannot carry on"));
    }
    Ok(())
}

fn after<T>(t: &TlsStream<Sock>, r: std::io::Result<T>) -> std::io::Result<T> {
    if r.is_err() {
        broken(t)?;
    }
    r
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(t) => {
                broken(t)?;
                let r = t.read(buf);
                after(t, r)
            }
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(t) => {
                broken(t)?;
                let r = t.write(buf);
                after(t, r)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(t) => {
                broken(t)?;
                let r = t.flush();
                after(t, r)
            }
        }
    }
}

/// The socket under a TLS stream: until the handshake is done each read and write is limited to
/// what is left of its deadline; afterwards the socket's timeouts are the caller's.
pub(crate) struct Sock {
    sock: TcpStream,
    deadline: Cell<Option<Instant>>,
    /// A write failed (see [`broken`]).
    broken: Cell<bool>,
}

impl Sock {
    fn arm(&self, write: bool) -> std::io::Result<()> {
        let Some(d) = self.deadline.get() else { return Ok(()) };
        let left = d.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the TLS handshake took too long"));
        }
        if write { self.sock.set_write_timeout(Some(left)) } else { self.sock.set_read_timeout(Some(left)) }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm(false)?;
        (&self.sock).read(buf)
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm(true)?;
        let r = (&self.sock).write(buf);
        if r.is_err() {
            self.broken.set(true);
        }
        r
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let r = (&self.sock).flush();
        if r.is_err() {
            self.broken.set(true);
        }
        r
    }
}

/// The trusted roots: the platform bundle and `IRONAV_CA_FILE`, read once.
fn roots() -> Result<RootStore> {
    static ROOTS: OnceLock<std::result::Result<RootStore, String>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let (mut store, system) = match RootStore::from_system() {
            Ok(s) => (s, None),
            Err(e) => (RootStore::new(), Some(e.to_string())),
        };
        match std::env::var(CA_FILE_VAR) {
            Ok(path) if !path.trim().is_empty() => {
                let path = path.trim();
                let text = std::fs::read_to_string(path).map_err(|e| format!("{CA_FILE_VAR}: cannot read {path}: {e}"))?;
                store.add_pem_bundle(&text).map_err(|e| format!("{CA_FILE_VAR}: {path} is not a PEM file of certificates: {e}"))?;
            }
            _ => {}
        }
        match system {
            Some(why) if store.is_empty() => Err(format!("no trusted roots: {why}")),
            _ => Ok(store),
        }
    });
    roots.clone().map_err(|e| Error::new(ErrorCode::Io, e).hint(format!("set SSL_CERT_FILE or {CA_FILE_VAR} to a PEM file of trusted roots")))
}

/// The client configuration: IronSocketLayer's default profile, or FIPS mode's when it is on (see
/// [`crate::fips`]).
fn client_config(roots: RootStore) -> Result<ClientConfig> {
    if crate::fips::enabled() {
        return crate::fips::client_config(roots);
    }
    ClientConfig::new(Profile::Default, roots).map_err(|e| Error::new(ErrorCode::Io, format!("TLS configuration: {e}")))
}

/// The client configuration for one ALPN protocol (none for RTSP), made once.
fn config(alpn: Option<&'static [u8]>) -> Result<Arc<ClientConfig>> {
    static HTTP: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    static OTHER: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let cell = if alpn.is_some() { &HTTP } else { &OTHER };
    if let Some(c) = cell.get() {
        return c.clone().map_err(|e| Error::new(ErrorCode::Io, e));
    }
    let roots = roots()?;
    let made = client_config(roots).map_err(|e| e.message).map(|c| Arc::new(match alpn {
        Some(p) => c.with_alpn(&[p]),
        None => c,
    }));
    cell.get_or_init(|| made).clone().map_err(|e| Error::new(ErrorCode::Io, format!("TLS configuration: {e}")))
}

/// Starts TLS on `sock` with `host` (a name or an address, checked against the certificate), in
/// at most `HANDSHAKE_TIMEOUT`. `shown` is the redacted URL, for errors. The socket comes back with
/// no timeouts: the caller sets its own.
pub(crate) fn connect(sock: TcpStream, host: &str, alpn: Option<&'static [u8]>, shown: &str) -> Result<Conn> {
    let config = config(alpn)?;
    let sock = Sock { sock, deadline: Cell::new(Some(Instant::now() + HANDSHAKE_TIMEOUT)), broken: Cell::new(false) };
    let t = TlsStream::connect(sock, config, host).map_err(|e| {
        let msg = e.to_string();
        let hint = if msg.contains("protocol-version") || msg.contains("ProtocolVersion") {
            "the server does not speak TLS 1.3, the only version IronAV uses".to_string()
        } else if msg.contains("name-mismatch") {
            "use the host name the server's certificate was issued for".to_string()
        } else if msg.contains("unknown-ca") || msg.contains("certificate") {
            format!("if the server's certificate comes from a private CA, name a PEM file of it in {CA_FILE_VAR}")
        } else {
            "check that the server speaks TLS on this port".to_string()
        };
        Error::new(ErrorCode::Io, format!("TLS with {shown} failed: {msg}")).hint(hint)
    })?;
    // the handshake is done: from here the caller's timeouts apply
    t.get_ref().deadline.set(None);
    t.get_ref().sock.set_read_timeout(None)?;
    t.get_ref().sock.set_write_timeout(None)?;
    Ok(Conn::Tls(Box::new(t)))
}

/// The server configuration for `whips://`: the certificate chain (leaf first) in the PEM file
/// `cert_path` and the private key in `key_path`, with IronSocketLayer's default profile, or FIPS
/// mode's when it is on. Read once, when the endpoint opens.
pub(crate) fn server_config(cert_path: &str, key_path: &str) -> Result<Arc<ServerConfig>> {
    let bad = |what: &str, path: &str, why: String| Error::invalid_arg(format!("whips://: {what} {path}: {why}"));
    let text = std::fs::read_to_string(cert_path).map_err(|e| bad("cannot read cert", cert_path, e.to_string()))?;
    let chain: Vec<Vec<u8>> = pem_blocks(&text).map_err(|e| bad("cert", cert_path, e))?.into_iter().filter(|(l, _)| l == "CERTIFICATE").map(|(_, d)| d).collect();
    if chain.is_empty() {
        return Err(bad("cert", cert_path, "no CERTIFICATE block (a PEM chain, the server's certificate first, is expected)".into()));
    }
    let text = std::fs::read_to_string(key_path).map_err(|e| bad("cannot read key", key_path, e.to_string()))?;
    let key = private_key(&text).map_err(|e| bad("key", key_path, e))?;
    let identity = Identity::new(chain, key).map_err(|e| bad("cert and key", cert_path, format!("{e} (does the key belong to the first certificate?)")))?;
    let config = if crate::fips::enabled() {
        crate::fips::server_config(identity)?
    } else {
        ServerConfig::new(Profile::Default, identity).map_err(|e| Error::new(ErrorCode::Io, format!("TLS configuration: {e}")))?
    };
    Ok(Arc::new(config))
}

/// The blocks of a PEM text, each as its label and decoded bytes.
fn pem_blocks(text: &str) -> std::result::Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut open: Option<(String, String)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) {
            open = Some((label.to_string(), String::new()));
        } else if let Some(label) = line.strip_prefix("-----END ").and_then(|l| l.strip_suffix("-----")) {
            match open.take() {
                Some((l, b64)) if l == label => out.push((l, unbase64(&b64).ok_or_else(|| format!("the {label} block is not base64"))?)),
                _ => return Err(format!("an END {label} line with no matching BEGIN")),
            }
        } else if let Some((_, b64)) = open.as_mut() {
            b64.push_str(line);
        }
    }
    match open {
        Some((l, _)) => Err(format!("the {l} block is not closed")),
        None => Ok(out),
    }
}

/// Standard base64, padded or not; `None` for anything else.
pub(crate) fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().take_while(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The first private key in a PEM text: PKCS#8 (`PRIVATE KEY`, any key type IronSocketLayer takes)
/// or SEC1 (`EC PRIVATE KEY`, P-256, P-384 or P-521).
fn private_key(text: &str) -> std::result::Result<SigningKey, String> {
    for (label, der) in pem_blocks(text)? {
        match label.as_str() {
            "PRIVATE KEY" => return SigningKey::from_pkcs8_der(&der).map_err(|e| e.to_string()),
            "EC PRIVATE KEY" => {
                let scalar = sec1_scalar(&der).ok_or("a malformed EC PRIVATE KEY")?;
                return match scalar.len() {
                    32 => SigningKey::ecdsa_p256(scalar),
                    48 => SigningKey::ecdsa_p384(scalar),
                    66 => SigningKey::ecdsa_p521(scalar),
                    n => return Err(format!("an EC private key of {n} bytes is not P-256, P-384 or P-521")),
                }
                .map_err(|e| e.to_string());
            }
            "ENCRYPTED PRIVATE KEY" => return Err("the key is encrypted; decrypt it first (openssl pkcs8 -nocrypt)".into()),
            "RSA PRIVATE KEY" => return Err("a PKCS#1 RSA key; convert it to PKCS#8 (openssl pkcs8 -topk8 -nocrypt)".into()),
            _ => {}
        }
    }
    Err("no PRIVATE KEY block (PKCS#8 or SEC1 EC) in the file".into())
}

/// One DER element: its tag, its content and what follows it.
fn der(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&first, b) = b.split_first()?;
    let (len, b) = match first {
        0..=0x7f => (first as usize, b),
        0x81 => (*b.first()? as usize, &b[1..]),
        0x82 => (u16::from_be_bytes([*b.first()?, *b.get(1)?]) as usize, &b[2..]),
        _ => return None,
    };
    Some((tag, b.get(..len)?, &b[len..]))
}

/// The private scalar of a SEC1 `ECPrivateKey`: SEQUENCE { INTEGER 1, OCTET STRING scalar, ... }.
fn sec1_scalar(b: &[u8]) -> Option<&[u8]> {
    let (0x30, body, _) = der(b)? else { return None };
    let (0x02, version, rest) = der(body)? else { return None };
    let (0x04, scalar, _) = der(rest)? else { return None };
    (version == [1]).then_some(scalar)
}

/// For tests: a throwaway CA, trusted through `IRONAV_CA_FILE` (so before this process's first TLS
/// connection), and a server configuration with a certificate from it for 127.0.0.1 and localhost.
#[cfg(all(test, not(feature = "fips")))]
pub(crate) fn test_server_config() -> Arc<ironsocketlayer::config::ServerConfig> {
    use ironsocketlayer::config::{Identity, ServerConfig};
    use ironsocketlayer::crypto::sign::{KeyKind, SigningKey};
    use ironsocketlayer::x509::{self, CertificateParams, Purpose};
    let rng = || ic_drbg::Rng::from_os().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let ca_key = SigningKey::generate(KeyKind::EcdsaP256, &mut rng()).unwrap();
    let mut p = CertificateParams::new("IronAV Test Root", now - 3600, now + 86_400, rng().random_array().unwrap());
    p.is_ca = true;
    p.path_len = Some(0);
    let ca = x509::self_signed(&p, &ca_key, &mut rng()).unwrap();
    let key = SigningKey::generate(KeyKind::EcdsaP256, &mut rng()).unwrap();
    let (dns, ips) = (["localhost"], [x509::IpAddr::V4([127, 0, 0, 1])]);
    let mut p = CertificateParams::new("localhost", now - 3600, now + 86_400, rng().random_array().unwrap());
    p.dns_names = &dns;
    p.ip_addresses = &ips;
    p.usage = &[Purpose::TlsServer];
    let cert = x509::issue(&p, key.spki(), &ca, &ca_key, &mut rng()).unwrap();
    let b64 = crate::http::base64(&ca);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|l| std::str::from_utf8(l).unwrap()).collect();
    let path = std::env::temp_dir().join(format!("ironav-test-ca-{}.pem", std::process::id()));
    std::fs::write(&path, format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))).unwrap();
    std::env::set_var(CA_FILE_VAR, &path);
    Arc::new(ServerConfig::new(Profile::Default, Identity::new(vec![cert], key).unwrap()).unwrap())
}
