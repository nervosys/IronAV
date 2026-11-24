//! WebVTT subtitles, as an HLS subtitle rendition carries them: one document per segment.
//!
//! The parser (and its tests) moved to `ironav_formats::webvtt`, which also reads `.vtt` files as a
//! subtitle stream; this module re-exports it under its old path.

pub use ironav_formats::webvtt::*;
