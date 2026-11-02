//! The credentials answer for HTTP and RTSP: Basic, and Digest (RFC 7616 with MD5, MD5-sess, SHA-256
//! and SHA-256-sess and qop=auth, or RFC 2069's form without qop). SHA-512-256 is not implemented.
//! The hashes are for Digest only.

use crate::http::base64;
use crate::md5::md5_hex;
use crate::sha256::sha256_hex;
use ironav_core::{Error, ErrorCode};

/// `N` random bytes for a nonce or a cnonce: from the operating system's entropy through IronCrypto's
/// SP 800-90A DRBG, the generator IronSocketLayer uses for TLS. If the operating system cannot supply
/// entropy this stops the process rather than answer with a guessable value.
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    ic_drbg::Rng::from_os().and_then(|mut rng| rng.random_array()).expect("the operating system's entropy source failed; no random value can be made safely")
}

/// The environment variable that lets Basic credentials go over plaintext (`http://`, `rtsp://`).
pub const ALLOW_PLAINTEXT_BASIC_VAR: &str = "IRONAV_ALLOW_PLAINTEXT_BASIC";

/// Whether whoever runs IronAV set [`ALLOW_PLAINTEXT_BASIC_VAR`] to `1`. A request cannot.
pub(crate) fn plaintext_basic_opted_in() -> bool {
    opt_in(std::env::var(ALLOW_PLAINTEXT_BASIC_VAR).ok().as_deref())
}

fn opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The answer to a host's challenge, kept so that later requests to it need no round trip.
pub(crate) enum Auth {
    /// The whole `Authorization` value.
    Basic(String),
    Digest(Digest),
}

/// The hash a Digest answer is computed with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hash {
    Md5,
    Sha256,
}

impl Hash {
    fn hex(self, data: &[u8]) -> String {
        match self {
            Hash::Md5 => md5_hex(data),
            Hash::Sha256 => sha256_hex(data),
        }
    }
}

pub(crate) struct Digest {
    user: String,
    /// HA1 = H(user:realm:password), the value RFC 7616 has a server keep. The password itself is not kept.
    ha1: String,
    realm: String,
    nonce: String,
    opaque: Option<String>,
    /// As the challenge named it, and echoed.
    algorithm: Option<String>,
    hash: Hash,
    sess: bool,
    qop: bool,
    nc: u32,
}

impl Auth {
    /// The answer to one challenge (one scheme, as `WWW-Authenticate` gives it), or why there is none.
    pub fn answer(challenge: &str, user: &str, pass: &str) -> Result<Auth, String> {
        let (scheme, params) = challenge.trim().split_once(|c: char| c.is_ascii_whitespace()).unwrap_or((challenge.trim(), ""));
        if scheme.eq_ignore_ascii_case("basic") {
            return Ok(Auth::Basic(format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))));
        }
        if !scheme.eq_ignore_ascii_case("digest") {
            return Err(format!("the {scheme} scheme"));
        }
        let p = auth_params(params);
        let get = |k: &str| p.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        let algorithm = get("algorithm");
        let name = algorithm.clone().unwrap_or_else(|| "MD5".into());
        let (hash, sess) = match name.to_ascii_lowercase().as_str() {
            "md5" => (Hash::Md5, false),
            "md5-sess" => (Hash::Md5, true),
            "sha-256" => (Hash::Sha256, false),
            "sha-256-sess" => (Hash::Sha256, true),
            _ => return Err(format!("the {name} algorithm")),
        };
        let qop = match get("qop") {
            None => false,
            Some(q) if q.split(',').any(|o| o.trim().eq_ignore_ascii_case("auth")) => true,
            Some(q) => return Err(format!("the qop {q}")),
        };
        if sess && !qop {
            return Err(format!("{name} without qop auth"));
        }
        let need = |k: &str| get(k).ok_or_else(|| format!("a challenge without a {k}"));
        let realm = need("realm")?;
        let ha1 = hash.hex(format!("{user}:{realm}:{pass}").as_bytes());
        Ok(Auth::Digest(Digest { user: user.into(), ha1, realm, nonce: need("nonce")?, opaque: get("opaque"), algorithm, hash, sess, qop, nc: 0 }))
    }

    fn is_sha256(&self) -> bool {
        matches!(self, Auth::Digest(d) if d.hash == Hash::Sha256)
    }

    /// The `Authorization` value for a request with `method` and request target `uri`.
    pub fn header(&mut self, method: &str, uri: &str) -> String {
        match self {
            Auth::Basic(h) => h.clone(),
            Auth::Digest(d) => d.header(method, uri),
        }
    }
}

impl Digest {
    fn header(&mut self, method: &str, uri: &str) -> String {
        let (nc, cnonce) = if self.qop {
            self.nc += 1;
            (self.nc, random_bytes::<16>().iter().map(|b| format!("{b:02x}")).collect::<String>())
        } else {
            (0, String::new())
        };
        let mut h = format!("Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{}\"", self.user, self.realm, self.nonce, self.response(method, uri, nc, &cnonce));
        if self.qop {
            h += &format!(", qop=auth, nc={nc:08x}, cnonce=\"{cnonce}\"");
        }
        if let Some(o) = &self.opaque {
            h += &format!(", opaque=\"{o}\"");
        }
        if let Some(a) = &self.algorithm {
            h += &format!(", algorithm={a}");
        }
        h
    }

    /// The request digest: `H(HA1:nonce:nc:cnonce:qop:HA2)`, or `H(HA1:nonce:HA2)` without qop, for
    /// the hash `H` the challenge named.
    fn response(&self, method: &str, uri: &str, nc: u32, cnonce: &str) -> String {
        let ha1 = if self.sess { self.hash.hex(format!("{}:{}:{cnonce}", self.ha1, self.nonce).as_bytes()) } else { self.ha1.clone() };
        let ha2 = self.hash.hex(format!("{method}:{uri}").as_bytes());
        if self.qop {
            self.hash.hex(format!("{ha1}:{}:{nc:08x}:{cnonce}:auth:{ha2}", self.nonce).as_bytes())
        } else {
            self.hash.hex(format!("{ha1}:{}:{ha2}", self.nonce).as_bytes())
        }
    }
}

/// The answer to a 401's challenges (its `WWW-Authenticate` values, each of which may hold several).
/// Digest is preferred, since it does not send the password, and SHA-256 Digest over MD5. When Digest
/// is offered only in a form IronAV does not speak, the password is not sent in the clear instead,
/// even if Basic is offered too.
///
/// Basic sends the password in every request, reversibly encoded, so it is refused unless the connection
/// is TLS (`tls`) or the operator allowed it (`basic_in_clear`, from [`plaintext_basic_opted_in`]).
pub(crate) fn choose(challenges: &[String], user: &str, pass: &str, tls: bool, basic_in_clear: bool) -> ironav_core::Result<Auth> {
    let all: Vec<String> = challenges.iter().flat_map(|c| split_challenges(c)).collect();
    let digest: Vec<&String> = all.iter().filter(|c| scheme_is(c, "digest")).collect();
    if digest.is_empty() {
        return match all.iter().find_map(|c| Auth::answer(c, user, pass).ok()) {
            Some(Auth::Basic(_)) if !tls && !basic_in_clear => Err(Error::new(ErrorCode::InvalidArgument, "asks for Basic authentication over a connection that is not encrypted, which would send the password in the clear")
                .hint(format!("use https:// or rtsps:// so the password is encrypted, or a camera that offers Digest; to allow Basic over plaintext anyway, set {ALLOW_PLAINTEXT_BASIC_VAR}=1"))),
            Some(a) => Ok(a),
            None => Err(Error::new(ErrorCode::UnsupportedFormat, format!("asks for an authentication scheme IronAV does not speak: {}", challenges.join("; ")))),
        };
    }
    let (mut answers, mut why) = (Vec::new(), Vec::new());
    for c in digest {
        match Auth::answer(c, user, pass) {
            Ok(a) => answers.push(a),
            Err(w) => why.push(w),
        }
    }
    answers.sort_by_key(|a| !a.is_sha256());
    answers.into_iter().next().ok_or_else(|| Error::new(ErrorCode::UnsupportedFormat, format!("asks for Digest authentication IronAV does not implement ({}), and the password is not sent in the clear instead", why.join(", "))))
}

/// `e` with the connection's (redacted) URL in front of its message, as the callers report it.
pub(crate) fn at(e: Error, shown: &str) -> Error {
    Error { message: format!("{shown} {}", e.message), ..e }
}

fn scheme_is(challenge: &str, scheme: &str) -> bool {
    challenge.trim_start().split(|c: char| c.is_ascii_whitespace()).next().is_some_and(|s| s.eq_ignore_ascii_case(scheme))
}

/// The challenges in one `WWW-Authenticate` value. A comma separates parameters and challenges
/// alike, so a piece that opens with a scheme name (`Digest realm=...`) starts a new challenge.
fn split_challenges(value: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let (mut quoted, mut start) = (false, 0);
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                pieces.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    pieces.push(&value[start..]);
    let mut out: Vec<String> = Vec::new();
    for p in pieces {
        let p = p.trim();
        let opens = p.split_once(|c: char| c.is_ascii_whitespace()).is_some_and(|(tok, _)| !tok.contains('='));
        match out.last_mut() {
            Some(last) if !opens => {
                last.push_str(", ");
                last.push_str(p);
            }
            _ => out.push(p.to_string()),
        }
    }
    out
}

/// `name=value` pairs of an authentication challenge; values may be quoted and hold commas.
fn auth_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let after = after.trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(e) => (&q[..e], &q[e + 1..]),
                None => (q, ""),
            }
        } else {
            match after.find(',') {
                Some(e) => (&after[..e], &after[e..]),
                None => (after, ""),
            }
        };
        out.push((name.trim().trim_start_matches(',').trim().to_string(), value.to_string()));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_7616_md5_example() {
        // RFC 7616 §3.9.1
        let challenge = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let Auth::Digest(d) = Auth::answer(challenge, "Mufasa", "Circle of Life").unwrap() else { panic!("not Digest") };
        assert_eq!(d.response("GET", "/dir/index.html", 1, "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ"), "8ca523f5e9506fed4657c9700eebdbec");
        // the header carries the same response, with the parameters the challenge asked for
        let mut a = Auth::answer(challenge, "Mufasa", "Circle of Life").unwrap();
        let h = a.header("GET", "/dir/index.html");
        assert!(h.starts_with("Digest username=\"Mufasa\", realm=\"http-auth@example.org\"") && h.contains("qop=auth, nc=00000001") && h.contains("algorithm=MD5"), "{h}");
        assert!(h.contains(r#"opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#));
        assert!(a.header("GET", "/dir/index.html").contains("nc=00000002"));
    }

    #[test]
    fn challenges_in_one_value_or_several() {
        assert_eq!(split_challenges(r#"Basic realm="a", Digest realm="r", qop="auth,auth-int", nonce="n""#), vec![r#"Basic realm="a""#, r#"Digest realm="r", qop="auth,auth-int", nonce="n""#]);
        assert_eq!(split_challenges("Basic"), vec!["Basic"]);
        let picked = choose(&[r#"Basic realm="a""#.into(), r#"Digest realm="r", nonce="n", qop=auth"#.into()], "u", "p", false, false).unwrap();
        assert!(matches!(picked, Auth::Digest(_)));
        let picked = choose(&[r#"Basic realm="a", Digest realm="r", nonce="n""#.into()], "u", "p", false, false).unwrap();
        assert!(matches!(picked, Auth::Digest(_)));
        let picked = choose(&[r#"Basic realm="a""#.into()], "Aladdin", "open sesame", true, false).unwrap();
        assert!(matches!(picked, Auth::Basic(h) if h == "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="));
    }

    #[test]
    fn rfc_7616_sha256_example() {
        // RFC 7616 3.9.1, with SHA-256 in place of MD5
        let challenge = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=SHA-256, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let Auth::Digest(d) = Auth::answer(challenge, "Mufasa", "Circle of Life").unwrap() else { panic!("not Digest") };
        assert_eq!(d.response("GET", "/dir/index.html", 1, "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ"), "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1");
        let h = Auth::answer(challenge, "Mufasa", "Circle of Life").unwrap().header("GET", "/dir/index.html");
        assert!(h.contains("algorithm=SHA-256") && h.contains("qop=auth, nc=00000001"), "{h}");
    }

    #[test]
    fn sha256_sess_hashes_the_session_key_with_sha256() {
        let Auth::Digest(d) = Auth::answer(r#"Digest realm="r", nonce="n", qop=auth, algorithm=SHA-256-sess"#, "u", "p").unwrap() else { panic!("not Digest") };
        let ha1 = sha256_hex(format!("{}:n:cn", sha256_hex(b"u:r:p")).as_bytes());
        let ha2 = sha256_hex(b"GET:/x");
        assert_eq!(d.response("GET", "/x", 1, "cn"), sha256_hex(format!("{ha1}:n:00000001:cn:auth:{ha2}").as_bytes()));
    }

    #[test]
    fn sha256_is_preferred_to_md5_whichever_comes_first() {
        let md5 = r#"Digest realm="r", nonce="n", qop=auth"#.to_string();
        let sha = r#"Digest realm="r", nonce="n", qop=auth, algorithm=SHA-256"#.to_string();
        for offered in [[md5.clone(), sha.clone()], [sha.clone(), md5.clone()]] {
            let mut picked = choose(&offered, "u", "p", false, false).unwrap();
            assert!(picked.header("GET", "/x").contains("algorithm=SHA-256"));
        }
        let mut picked = choose(&[md5], "u", "p", false, false).unwrap();
        assert!(!picked.header("GET", "/x").contains("algorithm="));
    }

    #[test]
    fn unsupported_digest_is_named_and_not_downgraded() {
        let e = choose(&[r#"Digest realm="r", nonce="n", algorithm=SHA-512-256"#.into(), r#"Basic realm="r""#.into()], "u", "p", true, false).err().unwrap();
        assert!(e.message.contains("SHA-512-256") && e.message.contains("not sent in the clear"), "{e}");
        assert!(choose(&[r#"Digest realm="r", nonce="n", qop="auth-int""#.into()], "u", "p", false, false).is_err());
    }

    #[test]
    fn basic_is_refused_over_plaintext_unless_opted_in() {
        let basic = [r#"Basic realm="cam""#.to_string()];
        let e = choose(&basic, "Aladdin", "open sesame", false, false).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let hint = e.hint.clone().unwrap_or_default();
        assert!(hint.contains("https://") && hint.contains("rtsps://") && hint.contains("Digest") && hint.contains("IRONAV_ALLOW_PLAINTEXT_BASIC=1"), "{hint}");
        assert!(!e.message.contains("open sesame"));
        // over TLS, or with the operator's opt-in, Basic is answered as before
        assert!(matches!(choose(&basic, "Aladdin", "open sesame", true, false), Ok(Auth::Basic(_))));
        assert!(matches!(choose(&basic, "Aladdin", "open sesame", false, true), Ok(Auth::Basic(_))));
    }

    #[test]
    fn digest_is_allowed_over_plaintext() {
        let offered = [r#"Basic realm="cam""#.to_string(), r#"Digest realm="cam", nonce="n", qop=auth"#.to_string()];
        assert!(matches!(choose(&offered, "u", "p", false, false), Ok(Auth::Digest(_))));
        assert!(matches!(choose(&offered[1..], "u", "p", false, false), Ok(Auth::Digest(_))));
    }

    #[test]
    fn only_the_value_one_opts_in() {
        assert!(opt_in(Some("1")));
        for v in [None, Some(""), Some("0"), Some("true"), Some("yes"), Some(" 1")] {
            assert!(!opt_in(v), "{v:?}");
        }
    }

    #[test]
    fn cnonces_are_128_bits_and_differ() {
        let mut a = Auth::answer(r#"Digest realm="r", nonce="n", qop=auth"#, "u", "p").unwrap();
        let (mut seen_a, mut seen_b) = (String::new(), String::new());
        for _ in 0..2 {
            let h = a.header("GET", "/x");
            let cnonce = h.split("cnonce=\"").nth(1).and_then(|r| r.split('"').next()).unwrap().to_string();
            assert_eq!(cnonce.len(), 32, "{cnonce}");
            assert!(cnonce.bytes().all(|b| b.is_ascii_hexdigit()), "{cnonce}");
            if seen_a.is_empty() { seen_a = cnonce } else { seen_b = cnonce }
        }
        assert_ne!(seen_a, seen_b);
    }

    #[test]
    fn random_bytes_differ_between_calls() {
        let a = random_bytes::<16>();
        let b = random_bytes::<16>();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 16]);
    }

    #[test]
    fn rfc_2069_form_without_qop() {
        let mut a = Auth::answer(r#"Digest realm="r", nonce="n""#, "u", "p").unwrap();
        let want = md5_hex(format!("{}:n:{}", md5_hex(b"u:r:p"), md5_hex(b"GET:/x")).as_bytes());
        let h = a.header("GET", "/x");
        assert!(h.contains(&format!("response=\"{want}\"")) && !h.contains("nc="), "{h}");
    }
}
