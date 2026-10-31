//! Live input: byte sources read as the bytes arrive, from a file (whole, or followed while it
//! grows), standard input or a named pipe, a TCP connection, a UDP port, an RTSP camera or server, or
//! HTTP (an HLS playlist, or a progressive download), the last two also over TLS, or a camera or
//! microphone (`capture`).
//!
//! A source only moves bytes. Parsing them is the stream demuxers' job (`ironav_formats::stream`),
//! and pacing, decoding and analysis belong to the session that owns the source. Every read takes
//! a deadline, so whoever reads can notice in time that it has been asked to stop.

pub mod capture;
mod hls;
mod http;
mod md5;
pub mod net;
pub mod reconnect;
pub mod onvif;
mod rtsp;
mod sha1;
mod source;
pub mod synthetic;
pub mod tls;
mod url;
mod youtube;

pub use net::{Allow, ALLOW_VAR};
pub use reconnect::{Health, HealthState, Policy};
pub use source::{open_source, Chunk, Source, SourceInfo};
pub use tls::CA_FILE_VAR;
pub use url::{redact, SourceUrl};
