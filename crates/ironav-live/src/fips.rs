//! FIPS mode: the transport and algorithm policy that applies when this build has the `fips` cargo
//! feature, or `IRONAV_FIPS=1` is set (the feature cannot be switched off at run time).
//!
//! What it does:
//! - TLS (`https://`, `rtsps://`) runs IronSocketLayer's FIPS 140-3 profile with the gate enabled
//!   (`policy::enable_fips`, which fails the run if IronCrypto's self-tests do). The client offers
//!   only AES-GCM suites, the P-256 and P-384 key exchanges, and ECDSA P-256/P-384 and RSA-PSS
//!   signatures. There is no ChaCha20-Poly1305, no X25519 or hybrid group, and no ECH GREASE (its
//!   HPKE uses X25519). RSA PKCS#1 v1.5 stays in the signature list only because certificate chains
//!   are commonly signed with it.
//! - Plaintext transports are refused: `http://`, `rtsp://`, `tcp://` and `udp://`, and `srt://`
//!   without a `passphrase` (with one, the stream is AES-CTR encrypted). That includes plaintext redirects and ONVIF's device service and
//!   discovery.
//! - Digest authentication with MD5 is refused. SHA-256 Digest and Basic over TLS still work.
//! - YouTube sources are refused: yt-dlp fetches them with its own TLS stack, outside this policy.
//!
//! What it does not do: IronCrypto, IronSocketLayer and IronPKI are not CMVP-validated, so this
//! restricts the build to FIPS-approved algorithms and disciplines its use of them. It is not a
//! FIPS 140-3 validated module, and nothing here should be described as one.

use ironav_core::{Error, ErrorCode, Result};
use ironsocketlayer::config::{ClientConfig, Profile};
use ironsocketlayer::enums::{CipherSuite, NamedGroup, SignatureScheme};
use ironsocketlayer::x509::RootStore;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// The environment variable that turns FIPS mode on (`1`, `true`, `yes` or `on`).
pub const FIPS_VAR: &str = "IRONAV_FIPS";

/// The cipher suites FIPS mode offers, in preference order.
const SUITES: [CipherSuite; 2] = [CipherSuite::TlsAes256GcmSha384, CipherSuite::TlsAes128GcmSha256];
/// The key exchange groups FIPS mode offers, in preference order.
const GROUPS: [NamedGroup; 2] = [NamedGroup::Secp256r1, NamedGroup::Secp384r1];
/// The signature schemes FIPS mode accepts from a peer and in its certificate chains.
const SCHEMES: [SignatureScheme; 8] = [
    SignatureScheme::EcdsaSecp256r1Sha256,
    SignatureScheme::EcdsaSecp384r1Sha384,
    SignatureScheme::RsaPssRsaeSha256,
    SignatureScheme::RsaPssRsaeSha384,
    SignatureScheme::RsaPssRsaeSha512,
    SignatureScheme::RsaPkcs1Sha256,
    SignatureScheme::RsaPkcs1Sha384,
    SignatureScheme::RsaPkcs1Sha512,
];

/// The transports that carry no TLS in IronAV.
const PLAINTEXT: [&str; 5] = ["http", "rtsp", "tcp", "udp", "srt"];

/// Whether `IRONAV_FIPS` is set to a true value.
pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| truthy(&v))
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Whether FIPS mode is on for this process: the `fips` feature of this build, or `IRONAV_FIPS`.
pub fn enabled() -> bool {
    cfg!(feature = "fips") || env_flag(FIPS_VAR)
}

/// Brings IronCrypto's module up in approved mode, once, when FIPS mode is on. Fails (and the caller
/// must not proceed) if the module's self-tests do not pass. Does nothing when FIPS mode is off.
pub fn activate() -> Result<()> {
    static MODULE: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    if !enabled() {
        return Ok(());
    }
    MODULE
        .get_or_init(|| ironsocketlayer::policy::enable_fips().map(|_| ()).map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| Error::new(ErrorCode::Internal, format!("FIPS mode: IronCrypto's module did not come up: {e}")).hint(format!("unset {FIPS_VAR} and build without the fips feature, or repair the module")))
}

/// The TLS client configuration for FIPS mode: IronSocketLayer's FIPS 140-3 profile, narrowed to
/// the groups, signatures and suites in this module, with ECH GREASE off.
pub(crate) fn client_config(roots: RootStore) -> Result<ClientConfig> {
    activate()?;
    let mut c = ClientConfig::new(Profile::Fips140_3, roots).map_err(|e| Error::new(ErrorCode::Io, format!("TLS configuration: {e}")))?;
    c.common.suites = SUITES.to_vec();
    c.common.groups = GROUPS.to_vec();
    c.common.schemes = SCHEMES.to_vec();
    c.ech_grease = false;
    Ok(c)
}

fn refuse_plaintext(on: bool, scheme: &str) -> Result<()> {
    if on && PLAINTEXT.iter().any(|p| p.eq_ignore_ascii_case(scheme)) {
        let instead = match scheme.to_ascii_lowercase().as_str() {
            "http" => "https://",
            "rtsp" => "rtsps://",
            _ => "a TLS source (https:// or rtsps://); this transport has no TLS form in IronAV",
        };
        return Err(Error::invalid_arg(format!("{scheme}:// is a plaintext transport, which FIPS mode refuses")).hint(format!("use {instead}")));
    }
    Ok(())
}

/// Fails for a plaintext transport (`http`, `rtsp`, `tcp`, `udp`, `srt`) while FIPS mode is on.
pub fn check_transport(scheme: &str) -> Result<()> {
    refuse_plaintext(enabled(), scheme)
}

fn refuse_youtube(on: bool) -> Result<()> {
    if on {
        return Err(Error::invalid_arg("YouTube sources are fetched by yt-dlp, which uses its own TLS stack, and FIPS mode refuses them")
            .hint("download the media and open the file, or use the stream's https:// playlist URL"));
    }
    Ok(())
}

/// Fails for a YouTube source while FIPS mode is on.
pub fn check_youtube() -> Result<()> {
    refuse_youtube(enabled())
}

fn refuse_digest(on: bool, algorithm: &str) -> std::result::Result<(), String> {
    if on && (algorithm.eq_ignore_ascii_case("md5") || algorithm.eq_ignore_ascii_case("md5-sess")) {
        return Err(format!("the {algorithm} algorithm, which FIPS mode refuses (MD5 is not approved; SHA-256 Digest is)"));
    }
    Ok(())
}

/// Fails for a Digest challenge naming MD5 (or MD5-sess) while FIPS mode is on.
pub(crate) fn check_digest(algorithm: &str) -> std::result::Result<(), String> {
    refuse_digest(enabled(), algorithm)
}

/// The TLS profile in force and what it allows, for capabilities.
pub fn tls_summary() -> Value {
    let (profile, suites, groups, schemes) = if enabled() {
        ("profile:fips-140-3", fmt(&SUITES), fmt(&GROUPS), fmt(&SCHEMES))
    } else {
        ("profile:default", fmt(Profile::Default.suites()), fmt(Profile::Default.groups()), fmt(Profile::Default.schemes()))
    };
    json!({
        "profile": profile,
        "suites": suites,
        "groups": groups,
        "signatures": schemes,
        "validated": false,
        "note": "algorithms are FIPS-approved per IronCrypto's approved-mode table; IronCrypto is not CMVP-validated"
    })
}

fn fmt<T: std::fmt::Debug>(items: &[T]) -> Vec<String> {
    items.iter().map(|i| format!("{i:?}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_transports_are_refused_only_in_fips_mode() {
        for scheme in ["http", "rtsp", "tcp", "udp", "srt", "HTTP"] {
            assert!(refuse_plaintext(true, scheme).is_err(), "{scheme}");
            assert!(refuse_plaintext(false, scheme).is_ok(), "{scheme} off");
        }
        for scheme in ["https", "rtsps", "file", "pipe", "device", "test"] {
            assert!(refuse_plaintext(true, scheme).is_ok(), "{scheme}");
        }
        let e = refuse_plaintext(true, "http").unwrap_err();
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("https://")), "{e:?}");
        let e = refuse_plaintext(true, "rtsp").unwrap_err();
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("rtsps://")), "{e:?}");
    }

    #[test]
    fn md5_digest_is_refused_only_in_fips_mode() {
        for alg in ["MD5", "md5-sess", "Md5"] {
            assert!(refuse_digest(true, alg).is_err(), "{alg}");
            assert!(refuse_digest(false, alg).is_ok(), "{alg} off");
        }
        for alg in ["SHA-256", "sha-256-sess"] {
            assert!(refuse_digest(true, alg).is_ok(), "{alg}");
        }
    }

    #[test]
    fn youtube_is_refused_only_in_fips_mode() {
        assert!(refuse_youtube(true).is_err());
        assert!(refuse_youtube(false).is_ok());
    }

    #[test]
    fn the_flag_values_are_recognised() {
        for v in ["1", "true", "YES", " on "] {
            assert!(truthy(v), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "2"] {
            assert!(!truthy(v), "{v}");
        }
    }

    #[test]
    fn the_fips_profile_offers_only_approved_algorithms() {
        let c = client_config(RootStore::new()).unwrap();
        assert_eq!(c.common.suites, SUITES.to_vec());
        assert_eq!(c.common.groups, GROUPS.to_vec());
        assert!(c.common.fips, "the module gate is on");
        assert!(!c.ech_grease, "ECH GREASE uses X25519 and ChaCha20");
        let names = format!("{:?} {:?} {:?}", c.common.suites, c.common.groups, c.common.schemes);
        for banned in ["ChaCha", "X25519", "MlKem", "Ed25519", "MlDsa", "Secp521", "Secp256r1MlKem"] {
            assert!(!names.contains(banned), "{banned} in {names}");
        }
    }

    #[test]
    fn the_summary_names_the_profile() {
        let s = tls_summary();
        let profile = s["profile"].as_str().unwrap();
        assert!(profile == "profile:fips-140-3" || profile == "profile:default", "{profile}");
        assert_eq!(s["validated"], json!(false));
    }
}
