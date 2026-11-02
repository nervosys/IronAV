//! Core types shared by every IronAV crate: errors, rationals, media descriptions and the
//! traits that demuxers, decoders, filters, encoders and muxers implement.

pub mod error;
pub mod fsutil;
pub mod media;
pub mod rational;
pub mod traits;

pub use error::{Error, ErrorCode, Result};
pub use fsutil::{create_new, replace_file};
pub use media::*;
pub use rational::Rational;
pub use traits::*;
