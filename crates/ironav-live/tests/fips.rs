//! FIPS mode end to end, without a network: plaintext transports, YouTube and ONVIF discovery are
//! refused before anything is connected, and IronCrypto's module comes up. Every test here runs with
//! the mode on (`IRONAV_FIPS=1`), in this process only; the handshake itself is covered by `tls.rs`,
//! which the `fips` feature runs with the FIPS profile.

use ironav_core::ErrorCode;
use ironav_live::{fips, net, onvif, open_source, Allow};
use std::time::Duration;

fn fips_on() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var(fips::FIPS_VAR, "1"));
    assert!(fips::enabled());
}

/// The error opening `url` gives, which must come (the source is not opened).
fn open_error(url: &str) -> ironav_core::Error {
    fips_on();
    match open_source(url) {
        Ok(_) => panic!("{url} was opened in FIPS mode"),
        Err(e) => e,
    }
}

#[test]
fn the_module_comes_up_and_the_profile_is_the_narrowed_one() {
    fips_on();
    fips::activate().expect("IronCrypto's self-tests pass");
    let s = fips::tls_summary();
    assert_eq!(s["profile"], "profile:fips-140-3");
    assert_eq!(s["validated"], false);
    let text = s.to_string();
    assert!(text.contains("TlsAes256GcmSha384") && text.contains("Secp256r1") && text.contains("Secp384r1"), "{text}");
    for banned in ["ChaCha", "X25519", "MlKem", "Ed25519", "Secp521"] {
        assert!(!text.contains(banned), "{banned} in {text}");
    }
}

#[test]
fn plaintext_sources_are_refused_before_any_connection() {
    for url in ["http://127.0.0.1:9/a.ts", "rtsp://127.0.0.1:9/live", "tcp://127.0.0.1:9", "tcp://:9", "udp://127.0.0.1:9", "udp://239.1.1.1:5000", "srt://127.0.0.1:9", "srt://:9?mode=listener&latency=50"] {
        let e = open_error(url);
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{url}");
        assert!(e.message.contains("plaintext") && e.message.contains("FIPS"), "{url}: {e:?}");
        assert!(e.hint.is_some(), "{url}");
    }
}

#[test]
fn an_encrypted_srt_source_is_not_refused_by_the_policy() {
    // allowed, so the failure is the connection (nothing answers on port 9), not the policy
    net::allow_loopback_for_tests();
    fips_on();
    match open_source("srt://127.0.0.1:9?passphrase=0123456789&reconnect=0") {
        Ok(_) => panic!("connected to nothing"),
        Err(e) => assert_eq!(e.code, ErrorCode::Io, "{e:?}"),
    }
    // and it is listening, not refused
    assert!(open_source("srt://127.0.0.1:0?mode=listener&passphrase=0123456789").is_ok());
}

#[test]
fn tls_sources_are_not_refused_by_the_policy() {
    // allowed, so the failure is the connection (nothing listens on port 9), not the policy
    net::allow_loopback_for_tests();
    fips_on();
    for url in ["https://127.0.0.1:9/a.ts", "rtsps://127.0.0.1:9/live"] {
        match open_source(url) {
            Ok(_) => panic!("{url} connected to nothing"),
            Err(e) => assert_eq!(e.code, ErrorCode::Io, "{url}: {e:?}"),
        }
    }
}

#[test]
fn youtube_is_refused_because_yt_dlp_is_outside_the_policy() {
    let e = open_error("https://www.youtube.com/watch?v=jNQXAC9IVRw");
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.message.contains("yt-dlp"), "{e:?}");
}

#[test]
fn onvif_discovery_and_device_calls_over_http_are_refused() {
    fips_on();
    match onvif::discover(onvif::DISCOVERY, Duration::from_millis(50)) {
        Ok(_) => panic!("discovery ran in FIPS mode"),
        Err(e) => assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}"),
    }
    let allow = Allow::parse("loopback").unwrap();
    match onvif::profiles("http://127.0.0.1:9", None, None, &allow) {
        Ok(_) => panic!("an http device service was used in FIPS mode"),
        Err(e) => assert!(e.message.contains("plaintext"), "{e:?}"),
    }
}
