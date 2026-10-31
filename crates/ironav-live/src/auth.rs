//! The credentials answer for HTTP and RTSP: Basic, and Digest (RFC 7616 with MD5 and MD5-sess and
//! qop=auth, or RFC 2069's form without qop). SHA-256 is not implemented. The MD5 is for Digest only.

use crate::http::base64;
use crate::md5::md5_hex;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// The answer to a host's challenge, kept so that later requests to it need no round trip.
pub(crate) enum Auth {
    /// The whole `Authorization` value.
    Basic(String),
    Digest(Digest),
}

pub(crate) struct Digest {
    user: String,
    pass: String,
    realm: String,
    nonce: String,
    opaque: Option<String>,
    /// As the challenge named it, and echoed.
    algorithm: Option<String>,
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
        let sess = match name.to_ascii_lowercase().as_str() {
            "md5" => false,
            "md5-sess" => true,
            _ => return Err(format!("the {name} algorithm")),
        };
        let qop = match get("qop") {
            None => false,
            Some(q) if q.split(',').any(|o| o.trim().eq_ignore_ascii_case("auth")) => true,
            Some(q) => return Err(format!("the qop {q}")),
        };
        if sess && !qop {
            return Err("MD5-sess without qop auth".into());
        }
        let need = |k: &str| get(k).ok_or_else(|| format!("a challenge without a {k}"));
        Ok(Auth::Digest(Digest { user: user.into(), pass: pass.into(), realm: need("realm")?, nonce: need("nonce")?, opaque: get("opaque"), algorithm, sess, qop, nc: 0 }))
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
            (self.nc, format!("{:016x}", RandomState::new().build_hasher().finish()))
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

    /// The request digest: `MD5(HA1:nonce:nc:cnonce:qop:HA2)`, or `MD5(HA1:nonce:HA2)` without qop.
    fn response(&self, method: &str, uri: &str, nc: u32, cnonce: &str) -> String {
        let ha1 = md5_hex(format!("{}:{}:{}", self.user, self.realm, self.pass).as_bytes());
        let ha1 = if self.sess { md5_hex(format!("{ha1}:{}:{cnonce}", self.nonce).as_bytes()) } else { ha1 };
        let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
        if self.qop {
            md5_hex(format!("{ha1}:{}:{nc:08x}:{cnonce}:auth:{ha2}", self.nonce).as_bytes())
        } else {
            md5_hex(format!("{ha1}:{}:{ha2}", self.nonce).as_bytes())
        }
    }
}

/// The answer to a 401's challenges (its `WWW-Authenticate` values, each of which may hold several).
/// Digest is preferred, since it does not send the password. When Digest is offered only in a form
/// IronAV does not speak, the password is not sent in the clear instead, even if Basic is offered too.
pub(crate) fn choose(challenges: &[String], user: &str, pass: &str) -> Result<Auth, String> {
    let all: Vec<String> = challenges.iter().flat_map(|c| split_challenges(c)).collect();
    let digest: Vec<&String> = all.iter().filter(|c| scheme_is(c, "digest")).collect();
    if digest.is_empty() {
        return all.iter().find_map(|c| Auth::answer(c, user, pass).ok()).ok_or_else(|| format!("asks for an authentication scheme IronAV does not speak: {}", challenges.join("; ")));
    }
    let mut why = Vec::new();
    for c in digest {
        match Auth::answer(c, user, pass) {
            Ok(a) => return Ok(a),
            Err(w) => why.push(w),
        }
    }
    Err(format!("asks for Digest authentication IronAV does not implement ({}), and the password is not sent in the clear instead", why.join(", ")))
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
        let picked = choose(&[r#"Basic realm="a""#.into(), r#"Digest realm="r", nonce="n", qop=auth"#.into()], "u", "p").unwrap();
        assert!(matches!(picked, Auth::Digest(_)));
        let picked = choose(&[r#"Basic realm="a", Digest realm="r", nonce="n""#.into()], "u", "p").unwrap();
        assert!(matches!(picked, Auth::Digest(_)));
        let picked = choose(&[r#"Basic realm="a""#.into()], "Aladdin", "open sesame").unwrap();
        assert!(matches!(picked, Auth::Basic(h) if h == "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="));
    }

    #[test]
    fn unsupported_digest_is_named_and_not_downgraded() {
        let e = choose(&[r#"Digest realm="r", nonce="n", algorithm=SHA-256"#.into(), r#"Basic realm="r""#.into()], "u", "p").err().unwrap();
        assert!(e.contains("SHA-256") && e.contains("not sent in the clear"), "{e}");
        assert!(choose(&[r#"Digest realm="r", nonce="n", qop="auth-int""#.into()], "u", "p").is_err());
    }

    #[test]
    fn rfc_2069_form_without_qop() {
        let mut a = Auth::answer(r#"Digest realm="r", nonce="n""#, "u", "p").unwrap();
        let want = md5_hex(format!("{}:n:{}", md5_hex(b"u:r:p"), md5_hex(b"GET:/x")).as_bytes());
        let h = a.header("GET", "/x");
        assert!(h.contains(&format!("response=\"{want}\"")) && !h.contains("nc="), "{h}");
    }
}
