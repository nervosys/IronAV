//! Live input: byte sources read as the bytes arrive, from a file (whole, or followed while it
//! grows), standard input or a named pipe, a TCP connection, a UDP port, an SRT sender, an RTSP camera or server, or
//! HTTP (an HLS playlist, or a progressive download), the last two also over TLS, or a camera or
//! microphone (`capture`), or a WebRTC publisher through a WHIP endpoint (`whip://`). Output goes
//! the other way through [`open_sink`]: MPEG-TS sent over SRT.
//!
//! A source only moves bytes. Parsing them is the stream demuxers' job (`ironav_formats::stream`),
//! and pacing, decoding and analysis belong to the session that owns the source. Every read takes
//! a deadline, so whoever reads can notice in time that it has been asked to stop.

pub mod capture;
pub mod exe;
pub mod fips;
mod auth;
mod hls;
mod http;
#[cfg(not(feature = "fips"))]
mod md5;
pub mod net;
pub mod reconnect;
pub mod onvif;
mod rtsp;
mod sha1;
mod sha256;
mod sink;
mod source;
mod srt;
pub mod synthetic;
pub mod tls;
mod url;
pub mod webvtt;
mod webrtc;
mod youtube;

pub use net::{Allow, ALLOW_VAR, METADATA_VAR};
pub use reconnect::{Health, HealthState, Policy};
pub use sink::{open_sink, plan_sink, whip_endpoint, Sink, SinkInfo, SinkPlan, SinkStats, WhipEndpoint};
pub use source::{open_source, Caption, Chunk, Source, SourceInfo};
pub use tls::CA_FILE_VAR;
pub use url::{redact, SourceUrl};
pub use webrtc::{websocket, AuthRequest, Authorize, Fallback, H264Profile, HttpReply, HttpRequest, WhepOptions, WhepServer, WhipOptions, WhipPublisher};
