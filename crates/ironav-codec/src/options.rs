//! Codec settings that the registry functions in `lib.rs` take. They live here rather than with the
//! codecs that read them, so that the signatures stay the same whichever codecs are compiled in.

/// What an [`OpusEncoder`] codes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OpusMode {
    /// CELT, full band: music and anything else, at any bit rate.
    #[default]
    Celt,
    /// SILK, wideband (audio up to 8 kHz): speech, at low bit rates (about 8 to 30 kb/s).
    Silk,
    /// Hybrid, full band: SILK up to 8 kHz and CELT above, at a variable rate; speech with its
    /// high frequencies (about 16 to 40 kb/s).
    Hybrid,
    /// One of the three for each packet, by the bit rate and the signal, as libopus picks: SILK
    /// below 16 kb/s (24 for stereo), hybrid above that for speech, and CELT above a threshold
    /// that runs from 16 kb/s for music to 64 kb/s for speech (44 for stereo), with some
    /// hysteresis. Frames of 10 ms or more.
    Auto,
}

/// What the input is, for [`OpusMode::Auto`] ([`OpusEncoder::with_signal`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OpusSignal {
    /// Guessed from how the level moves (see [`OpusEncoder::with_signal`]).
    #[default]
    Auto,
    /// Speech: SILK and hybrid up to a higher bit rate.
    Voice,
    /// Music: CELT down to a lower one.
    Music,
}

/// The ProRes profiles, by their sample entry FOURCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Proxy,
    Lt,
    Standard,
    Hq,
    Fourfour,
    FourfourXq,
}

impl Profile {
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            Self::Proxy => *b"apco",
            Self::Lt => *b"apcs",
            Self::Standard => *b"apcn",
            Self::Hq => *b"apch",
            Self::Fourfour => *b"ap4h",
            Self::FourfourXq => *b"ap4x",
        }
    }

    pub fn from_fourcc(t: &[u8]) -> Option<Self> {
        [Self::Proxy, Self::Lt, Self::Standard, Self::Hq, Self::Fourfour, Self::FourfourXq].into_iter().find(|p| p.fourcc() == t)
    }

    /// Whether the profile is 4:4:4 (and 12-bit).
    pub fn is_444(self) -> bool {
        matches!(self, Self::Fourfour | Self::FourfourXq)
    }

    /// The quantiser the encoder uses: a step of this many 10-bit coefficient units.
    #[cfg(feature = "prores")]
    pub(crate) fn qscale(self) -> u32 {
        match self {
            Self::Proxy => 24,
            Self::Lt => 16,
            Self::Standard => 8,
            Self::Hq | Self::Fourfour => 4,
            Self::FourfourXq => 2,
        }
    }
}
