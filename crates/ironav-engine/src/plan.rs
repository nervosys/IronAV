use crate::job::{FilterCall, Job};
use ironav_codec::{build_filter, new_decoder, new_encoder, FILTERS};
use ironav_core::{CodecId, Error, ErrorCode, MediaType, Result, StreamInfo, StreamParams};
use ironav_formats::{find_format, format_for_path, open_demuxer};
use crate::job::StreamOpts;
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
    pub input_stream: usize,
    pub output_stream: usize,
    pub action: Action,
    pub from_codec: CodecId,
    pub to_codec: CodecId,
    pub filters: Vec<FilterCall>,
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
    pub output: OutputSummary,
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
}

/// Bit depth of the coded output. FLAC keeps the source depth when it has one; from PCM it
/// uses 8/16 bits for 8/16-bit sources and 24 bits for wider ones (32-bit and float).
fn output_bits(to: CodecId, src: &StreamInfo) -> Option<u8> {
    use ironav_core::SampleFormat;
    if let Some(f) = to.sample_format() {
        return Some(f.bytes() as u8 * 8);
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

pub fn plan(job: &Job) -> Result<Plan> {
    let in_path = Path::new(&job.input);
    let out_path = Path::new(&job.output);
    let demux = open_demuxer(in_path)?;
    let spec = match &job.format {
        Some(f) => find_format(f)?,
        None => format_for_path(out_path)?,
    };
    if !spec.mux {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("format '{}' cannot be written", spec.name)));
    }
    if out_path.exists() {
        if matches!((in_path.canonicalize(), out_path.canonicalize()), (Ok(a), Ok(b)) if a == b) {
            return Err(Error::invalid_arg("input and output are the same file").hint("choose a different output path"));
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

    for s in demux.streams() {
        let media = s.params.media_type();
        if media != spec.media {
            warnings.push(format!("input stream {} ({:?}) dropped: '{}' only holds {:?}", s.index, media, spec.name, spec.media));
            continue;
        }
        if !steps.is_empty() {
            warnings.push(format!("input stream {} dropped: '{}' holds a single stream", s.index, spec.name));
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
        let to = match opts.codec.as_deref() {
            None | Some("copy") => s.codec,
            Some(name) => CodecId::parse(name)?,
        };
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

        let action = if to == s.codec && chain.is_empty() { Action::Copy } else { Action::Transcode };
        if media == MediaType::Audio && (opts.bitrate_kbps.is_some() || opts.qp.is_some()) {
            return Err(Error::invalid_arg("bitrate_kbps and qp apply to video streams only"));
        }
        if media == MediaType::Video && action == Action::Transcode {
            rate_control(opts)?;
        } else if media == MediaType::Video && (opts.bitrate_kbps.is_some() || opts.qp.is_some()) {
            return Err(Error::invalid_arg("bitrate_kbps and qp have no effect when the stream is copied")
                .hint("set a video codec (for example h264) to re-encode"));
        }
        let bits = if action == Action::Copy { s.bits } else { output_bits(to, s) };
        let mut hardware = Vec::new();
        if action == Action::Transcode {
            // Every source codec IronAV can read has a software decoder, so hardware decode is
            // never needed; asking for it would make `require` fail for no reason.
            new_decoder(s.codec, &s.params, s.bits)?;
            let hw_encodable = provider_codecs(Direction::Encode).contains(&to);
            if media == MediaType::Video || hw_encodable || matches!(policy, HwPolicy::RequireAny | HwPolicy::Require(_)) {
                hardware.push(select(to, Direction::Encode, policy, &probes)?);
            }
            let hw_chosen = hardware.iter().any(|h| h.direction == Direction::Encode && h.chosen.is_some());
            if hw_chosen {
                #[cfg(any(windows, target_os = "linux"))]
                ironav_hwaccel::nvenc::accepts(&params).map_err(|m| {
                    Error::invalid_arg(m).hint("convert the input to 8-bit yuv420p with even dimensions, or set hwaccel to \"off\" if a software encoder exists")
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
                new_encoder(to, &params, bits)?;
            }
        }

        out_streams.push(StreamInfo {
            index: steps.len(),
            codec: to,
            time_base: s.time_base,
            duration: s.duration,
            bits,
            params,
        });
        steps.push(Step { input_stream: s.index, output_stream: steps.len(), action, from_codec: s.codec, to_codec: to, filters: chain, hardware });
    }

    if let Some(i) = used.iter().position(|u| !u) {
        return Err(Error::invalid_arg(format!(
            "filter '{}' matched no stream that is being written to '{}'",
            job.filters[i].name, spec.name
        ))
        .hint("audio filters need an audio output format such as wav; video filters need a video format such as y4m"));
    }
    if steps.is_empty() {
        return Err(Error::invalid_input(format!("input has no {:?} stream that '{}' can hold", spec.media, spec.name))
            .hint("choose an output format that matches the input's media type"));
    }

    Ok(Plan {
        input: InputSummary { path: job.input.clone(), format: demux.format_name(), streams: demux.streams().to_vec() },
        output: OutputSummary { path: job.output.clone(), format: spec.name, streams: out_streams },
        steps,
        warnings,
    })
}
