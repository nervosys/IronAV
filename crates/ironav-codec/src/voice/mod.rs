//! Building blocks for a live voice link, each coding or processing one frame per call with
//! buffers sized when it is made: no threads, no I/O, no allocation per frame. Each says how
//! much delay it adds, so a caller can add up its budget.
//!
//! - [`StreamResampler`]: rate conversion (16 kHz to and from 24 and 48 kHz, or any other pair).
//! - [`Playout`]: a jitter buffer that gives one packet per frame period, in order.
//! - [`Vad`], [`Gate`], [`Agc`] and [`Denoiser`]: speech detection, push-to-talk trimming,
//!   level control and noise suppression before the encoder.
//!
//! The Opus encoder's frame-at-a-time path is `crate::opus::OpusEncoder::encode_frame_into`.

mod dsp;
mod playout;

pub use dsp::{Agc, Denoiser, Gate, Spectrum, SpectrumMeter, Vad};
pub use playout::{Playout, PlayoutStats, Pop};
pub use crate::resample::StreamResampler;
