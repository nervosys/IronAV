use crate::job::{parse_tiles, FilterCall, Job};
use ironav_codec::{audio_bitrate_range, build_filter, encoder_extradata_with_opts, new_encoder_with_opts, VideoOptions, FILTERS};
use ironav_core::{CodecId, Error, ErrorCode, MediaType, PixelFormat, Result, SampleFormat, StreamInfo, StreamParams};
use ironav_formats::{find_format, format_for_path, open_demuxer};
use crate::job::{Latency, OpusJob, ProResProfile, StreamOpts};
use ironav_hwaccel::{probe_all, provider_codecs, select, Direction, HwPolicy, RateControl, Selection, DEFAULT_QP};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Packets are forwarded untouched: no decode, no re-encode, no quality loss.
    Copy,
    /// Decode → filters → encode.
    Transcode,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// Which input file the stream comes from: 0 is `input`, then `extra_inputs` in order.
    pub input_file: usize,
    pub input_stream: usize,
    pub output_stream: usize,
    pub action: Action,
    pub from_codec: CodecId,
    pub to_codec: CodecId,
    pub filters: Vec<FilterCall>,
    /// The audio encoder's target bit rate, when one was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// The Opus encoder settings asked for, when the audio is written as Opus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opus: Option<OpusJob>,
    /// The key frame interval asked for, in frames, when the video encoder takes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyint: Option<u32>,
    /// The reference pictures asked for a P picture, when the software H.264 encoder is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<u32>,
    /// The B pictures asked for, when the video is written as hevc or mpeg4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bframes: Option<u32>,
    /// The AV1 tile columns and rows asked for, as counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiles: Option<(u32, u32)>,
    /// Quarter-sample motion refinement, for AV1.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub quarter_mv: bool,
    /// The hidden ALTREF lag asked for, for AV1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altref: Option<u32>,
    /// The ProRes profile asked for, when the video is written as prores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prores_profile: Option<ProResProfile>,
    /// The constant quantizer asked for the video encoder (0 to 51), when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qp: Option<u8>,
    /// The video encoder is set up for live delivery (`latency: low`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub low_latency: bool,
    /// Hardware decisions for each codec stage, with the reason for each candidate.
    pub hardware: Vec<Selection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InputSummary {
    pub path: String,
    pub format: &'static str,
    pub streams: Vec<StreamInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputSummary {
    pub path: String,
    pub format: &'static str,
    pub streams: Vec<StreamInfo>,
}

/// A validated, fully resolved execution plan. Producing one never touches the output path.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub input: InputSummary,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_inputs: Vec<InputSummary>,
    pub output: OutputSummary,
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
    /// The fragment length of a fragmented MP4 or MOV output (see `Job::fragment_seconds`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fragment_seconds: Option<f64>,
}

/// Bit depth of the coded output. FLAC keeps the source depth when it has one; from PCM it
/// uses 8/16 bits for 8/16-bit sources and 24 bits for wider ones (32-bit and float).
/// ALAC is 16 or 24 bits: a source of 16 bits or less codes at 16, anything wider at 24.
fn output_bits(to: CodecId, src: &StreamInfo) -> Option<u8> {
    use ironav_core::SampleFormat;
    if let Some(f) = to.sample_format() {
        return Some(f.bytes() as u8 * 8);
    }
    if to == CodecId::Alac {
        let bits = match (src.codec, src.codec.sample_format()) {
            (CodecId::Flac | CodecId::Alac, _) => src.bits.unwrap_or(16),
            (_, Some(SampleFormat::U8 | SampleFormat::S16)) => 16,
            _ => 24,
        };
        return Some(if bits <= 16 { 16 } else { 24 });
    }
    if to == CodecId::Flac {
        return Some(match (src.codec, src.codec.sample_format()) {
            (CodecId::Flac, _) => src.bits.unwrap_or(16),
            (_, Some(SampleFormat::U8)) => 8,
            (_, Some(SampleFormat::S16)) => 16,
            _ => 24,
        });
    }
    None
}

/// Rate control requested by a video stream's options, validated.
pub(crate) fn rate_control(opts: &StreamOpts) -> Result<RateControl> {
    match (opts.bitrate_kbps, opts.qp) {
        (Some(_), Some(_)) => Err(Error::invalid_arg("set either bitrate_kbps or qp, not both").hint("bitrate_kbps targets a size; qp targets a quality")),
        (Some(0), None) => Err(Error::invalid_arg("bitrate_kbps must be greater than zero")),
        (Some(k), None) => Ok(RateControl::Bitrate(k.saturating_mul(1000))),
        (None, Some(q)) if q > 51 => Err(Error::invalid_arg(format!("qp must be 0-51, got {q}")).hint("lower is better quality; 18-28 is the usual range")),
        (None, Some(q)) => Ok(RateControl::ConstQp(q)),
        (None, None) => Ok(RateControl::ConstQp(DEFAULT_QP)),
    }
}

fn is_software_encodable(c: CodecId) -> bool {
    ironav_codec::is_implemented(c, true)
}

impl Plan {
    /// The input file with index `file` (0 is the primary input, then the extras in order).
    pub fn input_at(&self, file: usize) -> &InputSummary {
        if file == 0 { &self.input } else { &self.extra_inputs[file - 1] }
    }
}

/// The refusal for an AV1-only setting (`name`) on audio, or on a video written as another codec.
fn av1_only(name: &str, media: MediaType, to: CodecId) -> Error {
    if media == MediaType::Audio {
        return Error::invalid_arg(format!("{name} applies to video streams only"));
    }
    Error::invalid_arg(format!("{name} applies to the av1 encoder, and this video is written as {}", to.name())).hint(format!("set the video codec to av1, or leave {name} out"))
}

pub fn plan(job: &Job) -> Result<Plan> {
    let out_path = Path::new(&job.output);
    let in_paths: Vec<&Path> = std::iter::once(job.input.as_str()).chain(job.extra_inputs.iter().map(String::as_str)).map(Path::new).collect();
    let demuxes = in_paths.iter().map(|p| open_demuxer(p)).collect::<Result<Vec<_>>>()?;
    let spec = match &job.format {
        Some(f) => find_format(f)?,
        None => format_for_path(out_path)?,
    };
    if !spec.mux {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("format '{}' cannot be written", spec.name)));
    }
    if let Some(s) = job.fragment_seconds {
        if !(s.is_finite() && s > 0.0) {
            return Err(Error::invalid_arg(format!("fragment_seconds must be a positive number of seconds, got {s}")));
        }
        if !matches!(spec.name, "mp4" | "mov") {
            return Err(Error::invalid_arg(format!("fragment_seconds applies to mp4 and mov output, not {}", spec.name)).hint("write the output as .mp4 or .mov, or leave fragment_seconds out"));
        }
    }
    if out_path.exists() {
        if in_paths.iter().any(|p| matches!((p.canonicalize(), out_path.canonicalize()), (Ok(a), Ok(b)) if a == b)) {
            return Err(Error::invalid_arg("an input and the output are the same file").hint("choose a different output path"));
        }
        if !job.overwrite {
            return Err(Error::new(ErrorCode::OutputExists, format!("output '{}' already exists", job.output))
                .hint("set \"overwrite\": true to replace it, or choose a new output path"));
        }
    }
    let policy = HwPolicy::parse(&job.hwaccel)?;
    let probes = probe_all();

    let mut warnings = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut out_streams: Vec<StreamInfo> = Vec::new();
    let mut used = vec![false; job.filters.len()];

    // (media, the file that supplied it): formats that hold several of a kind take them all from that one file
    let mut taken: Vec<(MediaType, usize)> = Vec::new();
    for (file, s) in demuxes.iter().enumerate().flat_map(|(f, d)| d.streams().iter().map(move |s| (f, s))) {
        let media = s.params.media_type();
        let at = if demuxes.len() > 1 { format!("input {file} stream {}", s.index) } else { format!("input stream {}", s.index) };
        if !spec.holds(media) {
            warnings.push(format!("{at} ({media:?}) dropped: '{}' cannot hold {media:?}", spec.name));
            continue;
        }
        if taken.iter().any(|&(m, f)| m == media && !(spec.holds_several() && f == file)) {
            warnings.push(format!("{at} ({media:?}) dropped: '{}' holds one {media:?} stream here and an earlier one was used", spec.name));
            continue;
        }
        if !taken.iter().any(|&(m, _)| m == media) {
            taken.push((media, file));
        }
        if (if media == MediaType::Audio { &job.audio } else { &job.video }).codec.as_deref() == Some("none") {
            warnings.push(format!("{at} ({media:?}) dropped: its codec is \"none\""));
            continue;
        }

        let mut chain: Vec<FilterCall> = Vec::new();
        let mut params = s.params.clone();
        for (i, fc) in job.filters.iter().enumerate() {
            let f = build_filter(&fc.name, &fc.args)?;
            let fspec = FILTERS.iter().find(|x| x.name == fc.name).expect("build_filter succeeded");
            if fspec.media != media {
                continue;
            }
            if let StreamParams::Video { pix_fmt, .. } = &params {
                if !fspec.input_pix_fmts.is_empty() && !fspec.input_pix_fmts.contains(pix_fmt) {
                    return Err(Error::invalid_arg(format!("filter '{}' cannot take {} input", fc.name, pix_fmt.name()))
                        .alternatives(fspec.input_pix_fmts.iter().map(|p| p.name())));
                }
            }
            used[i] = true;
            params = f.output_params(&params);
            chain.push(fc.clone());
        }

        let opts = if media == MediaType::Audio { &job.audio } else { &job.video };
        let mut to = match opts.codec.as_deref() {
            None | Some("copy") => s.codec,
            Some(name) => CodecId::parse(name)?,
        };
        // a format that holds only one codec of this kind (a .jpg) names the codec itself
        if opts.codec.is_none() && !spec.codecs.contains(&to) {
            let only: Vec<CodecId> = spec.codecs.iter().copied().filter(|c| c.media_type() == media).collect();
            if let [c] = only[..] {
                warnings.push(format!("{at}: '{}' holds only {}, so the stream is encoded as {} instead of {}", spec.name, c.name(), c.name(), to.name()));
                to = c;
            }
        }
        // a format that holds only PCM (a .wav) takes other audio as PCM: at the source's depth when
        // it says one (FLAC), else 16 bits
        if opts.codec.is_none() && media == MediaType::Audio && !spec.codecs.contains(&to) && spec.codecs.iter().filter(|c| c.media_type() == media).all(|c| c.sample_format().is_some()) {
            let pcm = CodecId::pcm_for(match s.bits {
                Some(b) if b > 24 => SampleFormat::S32,
                Some(b) if b > 16 => SampleFormat::S24,
                _ => SampleFormat::S16,
            });
            if spec.codecs.contains(&pcm) {
                warnings.push(format!("{at}: '{}' holds PCM only, so the stream is decoded to {}", spec.name, pcm.name()));
                to = pcm;
            }
        }
        if to.media_type() != media {
            return Err(Error::invalid_arg(format!("codec {} cannot be used for a {:?} stream", to.name(), media)));
        }
        if !spec.codecs.contains(&to) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("format '{}' cannot hold {}", spec.name, to.name()))
                .hint(if opts.codec.is_none() {
                    "the input codec does not fit the output format; set the stream's \"codec\" to one of the alternatives"
                } else {
                    "pick a codec the output format can hold"
                })
                .alternatives(spec.codecs.iter().map(|c| c.name())));
        }

        // Opus is coded at 48 kHz only, MP3 and MP2 (MPEG-1) at 32, 44.1 or 48: bring other rates there rather than refuse
        if media == MediaType::Audio && matches!(to, CodecId::Opus | CodecId::Mp3 | CodecId::Mp2) {
            let from = if let StreamParams::Audio { sample_rate, .. } = &params { *sample_rate } else { 0 };
            let want: &[u32] = if to == CodecId::Opus { &[48_000] } else { &[32_000, 44_100, 48_000] };
            if from > 0 && !want.contains(&from) {
                let rate = *want.iter().min_by_key(|&&r| r.abs_diff(from)).unwrap();
                let mut args = serde_json::Map::new();
                args.insert("rate".into(), rate.into());
                let call = FilterCall { name: "resample".into(), args };
                params = build_filter(&call.name, &call.args)?.output_params(&params);
                warnings.push(format!("{at}: resampled from {from} Hz to {rate} Hz, a rate {} is coded at", to.name()));
                chain.push(call);
            }
        }

        // ProRes is written as 10-bit 4:2:2, or as 4:4:4 (10 or 12 bit, kept as they are) for the 4444
        // profiles; other pictures are converted to the one the profile takes first
        if media == MediaType::Video && to == CodecId::ProRes {
            let is444 = matches!(opts.prores_profile, Some(ProResProfile::Fourfour | ProResProfile::FourfourXq));
            let fits = match &params {
                StreamParams::Video { pix_fmt, .. } if is444 => matches!(pix_fmt, PixelFormat::Yuv444p10 | PixelFormat::Yuv444p12),
                StreamParams::Video { pix_fmt, .. } => *pix_fmt == PixelFormat::Yuv422p10,
                _ => true,
            };
            if !fits {
                let mut args = serde_json::Map::new();
                args.insert("pix_fmt".into(), if is444 { "yuv444p10le" } else { "yuv422p10le" }.into());
                let call = FilterCall { name: "format".into(), args };
                params = build_filter(&call.name, &call.args)?.output_params(&params);
                chain.push(call);
            }
        }

        // asking for a bit rate from a codec that has one means re-encoding, not copying, and so
        // does asking a named video codec for low latency (the source may hold B pictures)
        let low = opts.latency == Some(Latency::Low);
        let named = opts.codec.as_deref().is_some_and(|c| c != "copy");
        let rate_wanted = (media == MediaType::Audio && opts.bitrate_kbps.is_some() && audio_bitrate_range(to).is_some())
            || (to == CodecId::Mjpeg && opts.qp.is_some())
            || (media == MediaType::Video && low && named)
            || (media == MediaType::Audio && opts.opus.is_some())
            || (media == MediaType::Video && opts.prores_profile.is_some())
            || (media == MediaType::Video && opts.keyint.is_some())
            || (media == MediaType::Video && opts.refs.is_some())
            || (media == MediaType::Video && (opts.bframes.is_some() || opts.tiles.is_some() || opts.quarter_mv || opts.altref.is_some()));
        let action = if to == s.codec && chain.is_empty() && !rate_wanted { Action::Copy } else { Action::Transcode };
        if rate_wanted && to == s.codec && chain.is_empty() {
            warnings.push(format!("{at}: a rate, quality, latency or encoder setting was set, so the {} stream is re-encoded instead of copied", to.name()));
        }
        if media == MediaType::Audio && opts.qp.is_some() {
            return Err(Error::invalid_arg("qp applies to video streams only"));
        }
        let codec_opus = opts.opus.as_ref().map(OpusJob::codec_options);
        let prores = opts.prores_profile.map(ProResProfile::codec_profile);
        if opts.opus.is_some() {
            if media == MediaType::Video {
                return Err(Error::invalid_arg("opus settings apply to audio streams only").hint("set the audio codec to opus instead"));
            }
            if to != CodecId::Opus {
                return Err(Error::invalid_arg(format!("opus settings apply to the opus encoder only, and this audio is written as {}", to.name()))
                    .hint("set the audio codec to opus, or leave the opus settings out"));
            }
        }
        if let Some(k) = opts.keyint {
            if media == MediaType::Audio {
                return Err(Error::invalid_arg("keyint applies to video streams only"));
            }
            if k == 0 {
                return Err(Error::invalid_arg("keyint must be at least 1 frame"));
            }
            if !matches!(to, CodecId::H264 | CodecId::Hevc | CodecId::Vp8 | CodecId::Vp9 | CodecId::Av1 | CodecId::Mpeg4 | CodecId::Mpeg2Video) {
                return Err(Error::invalid_arg(format!("keyint applies to the h264, hevc, vp8, vp9, av1, mpeg4 and mpeg2 encoders, and this video is written as {}", to.name()))
                    .hint("set the video codec to one of them, or leave keyint out"));
            }
        }
        if let Some(n) = opts.refs {
            if media == MediaType::Audio {
                return Err(Error::invalid_arg("refs applies to video streams only"));
            }
            if !(1..=16).contains(&n) {
                return Err(Error::invalid_arg(format!("refs must be from 1 to 16, not {n}")));
            }
            if to != CodecId::H264 {
                return Err(Error::invalid_arg(format!("refs applies to the h264 encoder, and this video is written as {}", to.name()))
                    .hint("set the video codec to h264, or leave refs out"));
            }
        }
        if let Some(n) = opts.bframes {
            if media == MediaType::Audio {
                return Err(Error::invalid_arg("bframes applies to video streams only"));
            }
            if n > 3 {
                return Err(Error::invalid_arg(format!("bframes must be from 0 to 3, not {n}")));
            }
            if !matches!(to, CodecId::Hevc | CodecId::Mpeg4 | CodecId::Mpeg2Video) {
                return Err(Error::invalid_arg(format!("bframes applies to the hevc, mpeg4 and mpeg2 encoders, and this video is written as {}", to.name()))
                    .hint("set the video codec to hevc, mpeg4 or mpeg2, or leave bframes out")
                    .alternatives(["hevc", "mpeg4", "mpeg2video"]));
            }
            if n > 0 && low {
                return Err(Error::invalid_arg("bframes conflicts with latency low, which codes no B pictures").hint("leave latency at normal, or set bframes to 0"));
            }
        }
        let tile_grid = match opts.tiles.as_deref() {
            None => None,
            Some(t) => {
                if media == MediaType::Audio || to != CodecId::Av1 {
                    return Err(av1_only("tiles", media, to));
                }
                Some(parse_tiles(t).ok_or_else(|| {
                    Error::invalid_arg(format!("tiles must be COLSxROWS, each a power of two from 1 to 64, got '{t}'"))
                        .hint("for example 2x1 for two tile columns and one row, or 2x2 for four tiles")
                })?)
            }
        };
        if opts.quarter_mv && (media == MediaType::Audio || to != CodecId::Av1) {
            return Err(av1_only("quarter_mv", media, to));
        }
        if let Some(lag) = opts.altref {
            if media == MediaType::Audio || to != CodecId::Av1 {
                return Err(av1_only("altref", media, to));
            }
            if !(2..=16).contains(&lag) {
                return Err(Error::invalid_arg(format!("altref must be a lag from 2 to 16 frames, not {lag}"))
                    .hint("the lag is how many frames each hidden ALTREF group spans; leave altref out for none"));
            }
            if low {
                return Err(Error::invalid_arg("altref conflicts with latency low, which holds no frames back").hint("leave latency at normal, or leave altref out"));
            }
        }
        if opts.prores_profile.is_some() {
            if media == MediaType::Audio {
                return Err(Error::invalid_arg("prores_profile applies to video streams only").hint("set the video codec to prores instead"));
            }
            if to != CodecId::ProRes {
                return Err(Error::invalid_arg(format!("prores_profile applies to the prores encoder only, and this video is written as {}", to.name()))
                    .hint("set the video codec to prores, or leave prores_profile out"));
            }
        }
        if let Some(p) = opts.opus.as_ref().and_then(|o| o.fec_loss_percent).filter(|&p| p > 100) {
            return Err(Error::invalid_arg(format!("fec_loss_percent must be 0 to 100, got {p}")));
        }
        if media == MediaType::Audio && opts.latency.is_some() {
            return Err(Error::invalid_arg("latency applies to video streams only").hint("audio encoders send each frame as soon as it is full"));
        }
        if media == MediaType::Video && low && action == Action::Copy {
            return Err(Error::invalid_arg("latency has no effect when the stream is copied").hint("set a video codec (for example h264) to re-encode"));
        }
        let video = (media == MediaType::Video).then(|| VideoOptions {
            bframes: opts.bframes.map(|n| n as usize),
            tiles: tile_grid.map(|(cols, rows)| (cols.trailing_zeros(), rows.trailing_zeros())),
            quarter_mv: opts.quarter_mv,
            altref: opts.altref.map(|n| n as usize),
        });
        let mut audio_kbps = None;
        if media == MediaType::Audio && opts.bitrate_kbps.is_some() {
            let Some((lo, hi)) = audio_bitrate_range(to).filter(|_| action == Action::Transcode) else {
                return Err(Error::invalid_arg(if action == Action::Copy {
                    "bitrate_kbps has no effect when the stream is copied".to_string()
                } else {
                    format!("{} has no bit rate to set", to.name())
                })
                .hint("audio bitrate_kbps applies to opus, vorbis, aac and mp3"));
            };
            let k = opts.bitrate_kbps.unwrap_or(0);
            if !(lo..=hi).contains(&k) {
                return Err(Error::invalid_arg(format!("bitrate_kbps for {} must be {lo} to {hi}, got {k}", to.name())));
            }
            audio_kbps = Some(k);
        }
        if media == MediaType::Video && action == Action::Transcode {
            rate_control(opts)?;
            if matches!(to, CodecId::Mpeg4 | CodecId::Mpeg2Video) && opts.qp.is_some_and(|q| !(1..=31).contains(&q)) {
                return Err(Error::invalid_arg(format!("qp for {} must be 1-31, got {}", to.name(), opts.qp.unwrap_or(0)))
                    .hint("the MPEG-4 and MPEG-2 quantiser scales run from 1 (best) to 31; use bitrate_kbps for a size target"));
            }
        } else if media == MediaType::Video && (opts.bitrate_kbps.is_some() || opts.qp.is_some()) {
            return Err(Error::invalid_arg("bitrate_kbps and qp have no effect when the stream is copied")
                .hint("set a video codec (for example h264) to re-encode"));
        }
        let bits = if action == Action::Copy { s.bits } else { output_bits(to, s) };
        let mut hardware = Vec::new();
        if action == Action::Transcode {
            // Hardware decode is used only for codecs with no software decoder (H.264, HEVC);
            // asking for it otherwise would make `require` fail for no reason.
            // H.264 has a software decoder but the GPU's is much faster, so hardware goes first
            // when the policy allows it and the software decoder is the fallback.
            let soft = ironav_codec::is_implemented(s.codec, false);
            let hw_capable = provider_codecs(Direction::Decode).contains(&s.codec);
            let hw_first = soft && ironav_codec::prefers_hardware_decode(s.codec, &s.params) && !matches!(policy, HwPolicy::Off | HwPolicy::Require(_) | HwPolicy::RequireAny);
            if (soft && !hw_first) || !hw_capable {
                ironav_codec::new_decoder_for(s)?;
            } else if soft && !matches!(policy, HwPolicy::Require(_) | HwPolicy::RequireAny) && select(s.codec, Direction::Decode, policy, &probes).is_err() {
                // the GPU rejects this stream outright (size, profile); the software decoder takes it
                ironav_codec::new_decoder_for(s)?;
            } else {
                let sel = select(s.codec, Direction::Decode, policy, &probes)?;
                if sel.chosen.is_none() && soft {
                    ironav_codec::new_decoder_for(s)?;
                } else if sel.chosen.is_none() {
                    let why = sel.considered.iter().map(|c| format!("{}: {}", c.backend.name(), c.reason)).collect::<Vec<_>>().join("; ");
                    let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
                    return Err(Error::new(
                        ErrorCode::HardwareUnavailable,
                        format!("{} has no software decoder in this build and no hardware backend is usable here{why}", s.codec.name()),
                    )
                    .hint(if matches!(policy, HwPolicy::Off) {
                        "hwaccel is \"off\"; set it to \"auto\" so the GPU decoder can be used"
                    } else {
                        "run `ironav hw probe`; NVDEC needs an NVIDIA GPU with a driver that supports it"
                    }));
                }
                if sel.chosen.is_some() {
                    let mut use_gpu = true;
                    #[cfg(any(windows, target_os = "linux"))]
                    if let Err(m) = ironav_hwaccel::nvdec::accepts(&s.params) {
                        // the GPU cannot take this stream; the software decoder can, unless hardware was required
                        if soft && !matches!(policy, HwPolicy::Require(_) | HwPolicy::RequireAny) {
                            ironav_codec::new_decoder_for(s)?;
                            use_gpu = false;
                        } else {
                            return Err(Error::new(ErrorCode::UnsupportedCodec, m).hint("only 8-bit and 10-bit 4:2:0 video can be decoded on the GPU (10-bit H.264 also needs a GPU whose NVDEC supports it)"));
                        }
                    }
                    if use_gpu {
                        hardware.push(sel);
                    }
                }
            }
            let hw_encodable = provider_codecs(Direction::Encode).contains(&to);
            if media == MediaType::Video || hw_encodable || matches!(policy, HwPolicy::RequireAny | HwPolicy::Require(_)) {
                hardware.push(select(to, Direction::Encode, policy, &probes)?);
            }
            let hw_chosen = hardware.iter().any(|h| h.direction == Direction::Encode && h.chosen.is_some());
            if hw_chosen && opts.bframes.is_some_and(|n| n > 0) {
                warnings.push(format!("{at}: the GPU encoder codes no B pictures, so bframes is ignored"));
            }
            if hw_chosen {
                #[cfg(any(windows, target_os = "linux"))]
                ironav_hwaccel::nvenc::accepts(&params, to).map_err(|m| {
                    Error::invalid_arg(m).hint("convert the input to yuv420p with even dimensions (10-bit yuv420p10le works for HEVC), or set hwaccel to \"off\" if a software encoder exists")
                })?;
            } else if hw_encodable && !is_software_encodable(to) {
                let why = hardware.last().map(|h| h.considered.iter().map(|c| format!("{}: {}", c.backend.name(), c.reason)).collect::<Vec<_>>().join("; ")).unwrap_or_default();
                return Err(Error::new(
                    ErrorCode::HardwareUnavailable,
                    format!("{} has no software encoder in this build and no hardware backend is usable here ({why})", to.name()),
                )
                .hint(if matches!(policy, HwPolicy::Off) {
                    "hwaccel is \"off\"; set it to \"auto\" so the GPU encoder can be used"
                } else {
                    "run `ironav hw probe`; NVENC needs an NVIDIA GPU with a driver that supports NVENC API 13.1"
                }));
            } else {
                new_encoder_with_opts(to, &params, bits, audio_kbps, codec_opus.as_ref(), prores, video.as_ref())?;
            }
        }

        // an audio encoder stamps its packets in samples of its own rate, which a resampling
        // filter may have changed
        let (time_base, duration) = match (&params, action) {
            (StreamParams::Audio { sample_rate, .. }, Action::Transcode) if *sample_rate > 0 => {
                let d = s.duration.map(|d| (d as i128 * s.time_base.num as i128 * *sample_rate as i128 + s.time_base.den as i128 / 2) / s.time_base.den as i128);
                (ironav_core::Rational::new(1, *sample_rate as i64), d.map(|d| d as i64))
            }
            _ => (s.time_base, s.duration),
        };
        out_streams.push(StreamInfo {
            index: steps.len(),
            codec: to,
            time_base,
            duration,
            bits,
            extradata: if action == Action::Copy { s.extradata.clone() } else { encoder_extradata_with_opts(to, &params, audio_kbps, codec_opus.as_ref(), prores, video.as_ref())? },
            lead_in: if action == Action::Copy { s.lead_in } else { ironav_codec::encoder_lead_in(to) },
            params,
        });
        steps.push(Step { input_file: file, input_stream: s.index, output_stream: steps.len(), action, from_codec: s.codec, to_codec: to, filters: chain, bitrate_kbps: audio_kbps, opus: opts.opus.clone(), prores_profile: opts.prores_profile, keyint: if media == MediaType::Video && action == Action::Transcode { opts.keyint } else { None }, refs: if media == MediaType::Video && action == Action::Transcode { opts.refs } else { None }, bframes: if media == MediaType::Video && action == Action::Transcode { opts.bframes } else { None }, tiles: if media == MediaType::Video && action == Action::Transcode { tile_grid } else { None }, quarter_mv: media == MediaType::Video && action == Action::Transcode && opts.quarter_mv, altref: if media == MediaType::Video && action == Action::Transcode { opts.altref } else { None }, qp: if media == MediaType::Video && action == Action::Transcode { opts.qp } else { None }, low_latency: media == MediaType::Video && action == Action::Transcode && low, hardware });
    }

    if let Some(i) = used.iter().position(|u| !u) {
        return Err(Error::invalid_arg(format!(
            "filter '{}' matched no stream that is being written to '{}'",
            job.filters[i].name, spec.name
        ))
        .hint("audio filters need an audio output format such as wav; video filters need a video format such as y4m"));
    }
    if steps.is_empty() {
        return Err(Error::invalid_input(format!("no input stream is one that '{}' can hold", spec.name))
            .hint("choose an output format that matches the input's media type"));
    }

    let summary = |path: &str, d: &dyn ironav_core::Demuxer| InputSummary { path: path.to_string(), format: d.format_name(), streams: d.streams().to_vec() };
    Ok(Plan {
        input: summary(&job.input, demuxes[0].as_ref()),
        extra_inputs: job.extra_inputs.iter().zip(&demuxes[1..]).map(|(p, d)| summary(p, d.as_ref())).collect(),
        output: OutputSummary { path: job.output.clone(), format: spec.name, streams: out_streams },
        steps,
        warnings,
        fragment_seconds: job.fragment_seconds,
    })
}
