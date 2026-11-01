use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A complete, declarative description of one conversion. This is the unit of work an
/// agent submits: everything needed is in the JSON, so plans are reproducible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub input: String,
    pub output: String,
    /// More files to read streams from, as with repeated `-i` in ffmpeg: for example a video
    /// file plus a separate audio file. Streams are taken in input order and the first file
    /// with a media type supplies it: one stream of it, or all of that file's streams of it when
    /// the output format holds several (Matroska and WebM).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_inputs: Vec<String>,
    /// Output container (`wav`, `y4m`, ...). Inferred from the output extension when omitted.
    #[serde(default)]
    pub format: Option<String>,
    /// MP4 and MOV output only: write the file fragmented, with the index first and then a `moof` and `mdat`
    /// for each fragment. A fragment is cut at a video key frame (or at any audio packet, with no video) once
    /// it is at least this many seconds long. Absent, the file is written with its index first, as usual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment_seconds: Option<f64>,
    #[serde(default)]
    pub audio: StreamOpts,
    #[serde(default)]
    pub video: StreamOpts,
    #[serde(default)]
    pub filters: Vec<FilterCall>,
    /// `auto` (default), `off`, `require`, or a backend name such as `cuda`.
    #[serde(default = "default_hwaccel")]
    pub hwaccel: String,
    /// Replace the output if it exists. Off by default so agents cannot clobber files by accident.
    #[serde(default)]
    pub overwrite: bool,
}

fn default_hwaccel() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamOpts {
    /// Target codec name, or `copy` to pass packets through untouched (the default).
    #[serde(default)]
    pub codec: Option<String>,
    /// Target average bitrate in kbit/s: for video, mutually exclusive with `qp`; for audio, Opus only (6 to 510).
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// Video only: constant quantizer, 0 (best) to 51 (smallest). Used when no bitrate is set;
    /// defaults to 23.
    #[serde(default)]
    pub qp: Option<u8>,
    /// Video only: `low` encodes for live delivery, where each picture should leave as soon as it
    /// is coded; `normal` (the default) favours size. See [`Latency`].
    #[serde(default)]
    pub latency: Option<Latency>,
    /// Opus encoder settings, for audio written as `opus`: mode, signal, packet length, FEC and DTX.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus: Option<OpusJob>,
    /// Video only: frames between key frames, for the h264, hevc, vp8, vp9, av1, mpeg4 and mpeg2 encoders. Default 250
    /// (one a second with `latency: low`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyint: Option<u32>,
    /// Video only, for the software h264 encoder: how many reference pictures a P picture may
    /// predict from, 1 to 16. Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<u32>,
    /// Video only, for the `hevc` and `mpeg4` encoders: B pictures between anchor pictures, 0 to 3.
    /// Default 0. The GPU encoder (NVENC) codes none, so it ignores this. Not with `latency: low`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bframes: Option<u32>,
    /// Video only, for the `av1` encoder: the tile grid as `COLSxROWS`, each a power of two from 1 to
    /// 64, for example `2x1` (two tile columns, one row). Default: two columns for pictures 1280
    /// samples wide or more, otherwise one tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiles: Option<String>,
    /// Video only, for the `av1` encoder: refine motion vectors to quarter samples as well as half
    /// (a little smaller, slower). Default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quarter_mv: bool,
    /// Video only, for the `av1` encoder: code hidden ALTREF frames, one per group of this many frames
    /// (2 to 16), shown later, with the frames between predicting from them. Default off. Not with
    /// `latency: low`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altref: Option<u32>,
    /// Video only, for the `prores` codec: the profile. Default `hq`. The 4444 profiles keep 4:4:4
    /// chroma (10-bit input, or 12-bit); the others are 4:2:2 at 10 bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prores_profile: Option<ProResProfile>,
}

/// Parses a tile grid such as `2x1` (`COLSxROWS`): each count a power of two from 1 to 64. None when it is not.
pub fn parse_tiles(s: &str) -> Option<(u32, u32)> {
    let (cols, rows) = s.split_once('x')?;
    let (cols, rows): (u32, u32) = (cols.parse().ok()?, rows.parse().ok()?);
    let ok = |n: u32| n.is_power_of_two() && n <= 64;
    (ok(cols) && ok(rows)).then_some((cols, rows))
}

/// The ProRes profiles (see `StreamOpts::prores_profile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProResProfile {
    #[serde(rename = "proxy")]
    Proxy,
    #[serde(rename = "lt")]
    Lt,
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "hq")]
    Hq,
    #[serde(rename = "4444")]
    Fourfour,
    #[serde(rename = "4444xq")]
    FourfourXq,
}

impl ProResProfile {
    /// The codec's profile for these settings.
    pub fn codec_profile(self) -> ironav_codec::prores::Profile {
        use ironav_codec::prores::Profile;
        match self {
            Self::Proxy => Profile::Proxy,
            Self::Lt => Profile::Lt,
            Self::Standard => Profile::Standard,
            Self::Hq => Profile::Hq,
            Self::Fourfour => Profile::Fourfour,
            Self::FourfourXq => Profile::FourfourXq,
        }
    }
}

/// How long an encoder may hold pictures back to make the stream smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Latency {
    /// B pictures, long keyframe intervals and a second of rate-control slack: smallest output.
    Normal,
    /// No B pictures, so no picture waits for a later one; a keyframe every second, so a viewer
    /// joining late waits at most that long; and a bit rate held to a quarter of a second's bits
    /// (on NVENC, whose low-latency tuning is used, constant bit rate with a one-picture buffer).
    /// MJPEG and raw video code each picture alone, so they are low latency either way.
    Low,
}

/// Opus encoder settings beyond the bit rate. Anything left out keeps the default: CELT mode, 20 ms
/// packets, no FEC, no DTX.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpusJob {
    /// `celt` (the default) for music at any bit rate; `silk` for speech at low bit rates
    /// (wideband); `hybrid` for speech with its high frequencies; `auto` to pick for each packet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OpusMode>,
    /// `voice`, `music` or `auto` (the default): what the input is, for `auto` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<OpusSignal>,
    /// Packet length in milliseconds: 2.5, 5, 10, 20 (the default), 40 or 60. SILK and hybrid
    /// modes, and `auto`, need 10 or more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_ms: Option<f64>,
    /// Expected packet loss, 0 to 100 percent. Above 0 turns on in-band forward error correction:
    /// each speech frame is coded again, coarser, into the next packet. Default 0 (off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec_loss_percent: Option<u8>,
    /// Discontinuous transmission: in a pause most frames are not sent, only the packet's TOC byte.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dtx: bool,
}

/// The Opus coding mode (see `OpusJob::mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpusMode {
    /// CELT: music and anything else, at any bit rate.
    Celt,
    /// SILK: speech, wideband, at low bit rates.
    Silk,
    /// Hybrid: speech, full band.
    Hybrid,
    /// Chosen for each packet by the bit rate and the signal.
    Auto,
}

/// What the input is, for `auto` mode (see `OpusJob::signal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpusSignal {
    /// Guessed from how the level moves.
    Auto,
    /// Speech: SILK and hybrid up to a higher bit rate.
    Voice,
    /// Music: CELT down to a lower bit rate.
    Music,
}

impl From<OpusMode> for ironav_codec::opus::OpusMode {
    fn from(m: OpusMode) -> Self {
        match m {
            OpusMode::Celt => Self::Celt,
            OpusMode::Silk => Self::Silk,
            OpusMode::Hybrid => Self::Hybrid,
            OpusMode::Auto => Self::Auto,
        }
    }
}

impl From<OpusSignal> for ironav_codec::opus::OpusSignal {
    fn from(s: OpusSignal) -> Self {
        match s {
            OpusSignal::Auto => Self::Auto,
            OpusSignal::Voice => Self::Voice,
            OpusSignal::Music => Self::Music,
        }
    }
}

impl OpusJob {
    /// The codec's options for these settings.
    pub fn codec_options(&self) -> ironav_codec::OpusOptions {
        ironav_codec::OpusOptions {
            mode: self.mode.map(Into::into).unwrap_or_default(),
            signal: self.signal.map(Into::into).unwrap_or_default(),
            frame_ms: self.frame_ms,
            fec_loss: u32::from(self.fec_loss_percent.unwrap_or(0)),
            dtx: self.dtx,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterCall {
    pub name: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl FilterCall {
    /// Parse the CLI shorthand `name` or `name:key=value,key=value`.
    pub fn parse_shorthand(s: &str) -> ironav_core::Result<FilterCall> {
        let (name, rest) = s.split_once(':').unwrap_or((s, ""));
        let mut args = Map::new();
        for kv in rest.split(',').filter(|p| !p.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or_else(|| {
                ironav_core::Error::invalid_arg(format!("filter argument '{kv}' must look like key=value"))
            })?;
            let value = v.parse::<f64>().map(Value::from).unwrap_or_else(|_| Value::from(v));
            args.insert(k.to_string(), value);
        }
        Ok(FilterCall { name: name.to_string(), args })
    }
}
