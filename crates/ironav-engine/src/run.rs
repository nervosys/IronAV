use crate::job::{Job, OpusJob, ProResProfile};
use crate::plan::{plan, rate_control, Action, Plan};
use ironav_codec::h264::H264Encoder;
use ironav_codec::hevc::HevcEncoder;
use ironav_codec::jpeg::JpegEncoder;
use ironav_codec::mpeg2::enc::Mpeg2Encoder;
use ironav_codec::mpeg4::enc::Mpeg4Encoder;
use ironav_codec::vp8::Vp8Encoder;
use ironav_codec::vp9::Vp9Encoder;
use ironav_codec::av1::enc::Av1Encoder;
use ironav_codec::{build_filter, new_encoder_with_opts, VideoOptions};
use ironav_core::{CodecId, Decoder, Encoder, Error, ErrorCode, Filter, Frame, MediaType, Packet, Rational, Result};
use ironav_hwaccel::{new_video_decoder, new_video_encoder, Direction, RateControl};
use ironav_formats::{create_muxer_new, open_demuxer};
use serde::Serialize;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver};
use std::thread;
use std::time::Instant;

/// Bounded queue depth between stages: deep enough to hide I/O jitter, shallow enough to
/// keep memory and added latency small.
const QUEUE_DEPTH: usize = 64;
const PROGRESS_EVERY: u64 = 256;

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub packets: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StepReport {
    pub input_stream: usize,
    pub packets_in: u64,
    pub packets_out: u64,
    pub frames: u64,
    pub bytes_out: u64,
    /// Hardware backend that encoded this stream, or `None` for software / stream copy.
    pub hardware_encoder: Option<&'static str>,
    /// Hardware backend that decoded this stream, or `None` for software / stream copy.
    pub hardware_decoder: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub output: String,
    pub format: &'static str,
    pub wall_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub media_seconds: Option<f64>,
    /// Media duration processed per wall-clock second (higher is faster; 1.0 = real time).
    pub realtime_factor: Option<f64>,
    pub throughput_mib_s: f64,
    pub steps: Vec<StepReport>,
    pub plan: Plan,
}

struct Runtime {
    action: Action,
    out_stream: usize,
    decoder: Option<Box<dyn Decoder>>,
    filters: Vec<Box<dyn Filter>>,
    encoder: Option<Box<dyn Encoder>>,
    rep: StepReport,
}

impl Runtime {
    fn through(&mut self, frames: Vec<Frame>) -> Result<Vec<Packet>> {
        self.through_from(0, frames)
    }

    /// Run `frames` through the filters from the `start`th on, then the encoder.
    fn through_from(&mut self, start: usize, frames: Vec<Frame>) -> Result<Vec<Packet>> {
        let mut cur = frames;
        for f in &mut self.filters[start..] {
            let mut next = Vec::with_capacity(cur.len());
            for fr in cur {
                next.extend(f.process(fr)?);
            }
            cur = next;
        }
        let enc = self.encoder.as_mut().expect("transcode steps have an encoder");
        let mut out = Vec::new();
        for fr in &cur {
            self.rep.frames += 1;
            out.extend(enc.encode(fr)?);
        }
        Ok(out)
    }
}

/// Execute `job`: plan, then run reader → process → writer as three pipelined threads.
/// Output is written to `<output>.partial` and renamed on success, so a failed or
/// interrupted run never leaves a truncated file at the real path.
pub fn run(job: &Job, on_progress: &mut dyn FnMut(&Progress)) -> Result<Report> {
    run_with(job, on_progress, None)
}

/// [`run`], stopping with a `cancelled` error (and no output) once `cancel` is set.
pub fn run_with(job: &Job, on_progress: &mut dyn FnMut(&Progress), cancel: Option<&std::sync::atomic::AtomicBool>) -> Result<Report> {
    let t0 = Instant::now();
    crate::policy::check_write(&job.output)?;
    let plan = plan(job)?;
    let demuxes = std::iter::once(&job.input).chain(&job.extra_inputs).map(|p| open_demuxer(Path::new(p))).collect::<Result<Vec<_>>>()?;
    let partial = format!("{}.partial", job.output);
    // exclusive: a leftover or planted <output>.partial is refused, and then not ours to delete
    let mux = create_muxer_new(Path::new(&partial), plan.output.format, plan.fragment_seconds)?;

    let rate = rate_control(&job.video)?;
    match execute(&plan, rate, demuxes, mux, t0, on_progress, cancel) {
        Ok((steps, bytes_in, bytes_out)) => {
            ironav_core::replace_file(Path::new(&partial), Path::new(&job.output))?;
            let wall = t0.elapsed();
            let secs = wall.as_secs_f64().max(1e-9);
            // Y4M and raw streams carry no duration; derive it from the frames actually encoded.
            // With several streams the media lasts as long as the longest.
            let media_seconds = plan
                .output
                .streams
                .iter()
                .zip(&steps)
                .filter_map(|(st, step)| {
                    st.duration_secs().or_else(|| match st.params {
                        ironav_core::StreamParams::Video { frame_rate, .. } if frame_rate.num > 0 => {
                            Some(step.frames.max(step.packets_in) as f64 * frame_rate.den as f64 / frame_rate.num as f64)
                        }
                        _ => None,
                    })
                })
                .max_by(f64::total_cmp);
            Ok(Report {
                output: job.output.clone(),
                format: plan.output.format,
                wall_ms: wall.as_millis() as u64,
                bytes_in,
                bytes_out,
                media_seconds,
                realtime_factor: media_seconds.map(|m| m / secs),
                throughput_mib_s: bytes_in as f64 / (1024.0 * 1024.0) / secs,
                steps,
                plan,
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

type Outcome = (Vec<StepReport>, u64, u64);

fn execute(
    plan: &Plan,
    rate: RateControl,
    mut demuxes: Vec<Box<dyn ironav_core::Demuxer>>,
    mut mux: Box<dyn ironav_core::Muxer>,
    t0: Instant,
    on_progress: &mut dyn FnMut(&Progress),
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Outcome> {
    let (tx_out, rx_out) = sync_channel::<Packet>(QUEUE_DEPTH);
    let out_streams = &plan.output.streams;

    let mut runtimes: Vec<Runtime> = Vec::new();
    let mut by_input: Vec<Vec<Option<usize>>> = (0..demuxes.len()).map(|f| vec![None; plan.input_at(f).streams.len()]).collect();
    for st in &plan.steps {
        let src = &plan.input_at(st.input_file).streams[st.input_stream];
        let dst = &plan.output.streams[st.output_stream];
        let (mut hw_name, mut hw_dec_name) = (None, None);
        let (decoder, encoder, filters) = if st.action == Action::Transcode {
            let mut fs = Vec::new();
            for fc in &st.filters {
                fs.push(build_filter(&fc.name, &fc.args)?);
            }
            let video = VideoOptions {
                bframes: st.bframes.map(|n| n as usize),
                tiles: st.tiles.map(|(cols, rows)| (cols.trailing_zeros(), rows.trailing_zeros())),
                quarter_mv: st.quarter_mv,
                altref: st.altref.map(|n| n as usize),
            };
            let hw = st.hardware.iter().find(|h| h.direction == Direction::Encode).and_then(|h| h.chosen);
            let encoder = match hw {
                Some(backend) if dst.params.media_type() == MediaType::Video => {
                    hw_name = Some(backend.name());
                    new_video_encoder(backend, dst.codec, &dst.params, rate, st.low_latency, st.keyint)?
                }
                _ if dst.codec == CodecId::Mjpeg && dst.params.media_type() == MediaType::Video => {
                    let quality = st.qp.map_or(JpegEncoder::default_quality(), JpegEncoder::quality_for_qp);
                    Box::new(JpegEncoder::new(quality))
                }
                _ if dst.codec == CodecId::Hevc && dst.params.media_type() == MediaType::Video => {
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = video.apply_hevc(HevcEncoder::new(&dst.params, qp, bitrate)?);
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::H264 && dst.params.media_type() == MediaType::Video => {
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = H264Encoder::new(&dst.params, qp, bitrate)?;
                    let e = if st.low_latency { e.low_latency() } else { e };
                    let e = match st.refs {
                        Some(n) => e.with_refs(n as usize),
                        None => e,
                    };
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::Vp9 && dst.params.media_type() == MediaType::Video => {
                    let RateControl::ConstQp(q) = rate else {
                        return Err(Error::invalid_arg("the software VP9 encoder is constant quality: set qp, not a bit rate").hint("qp 18-28 is the usual range"));
                    };
                    let e = Vp9Encoder::new(&dst.params, Some(q))?;
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::Av1 && dst.params.media_type() == MediaType::Video => {
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = video.apply_av1(Av1Encoder::new(&dst.params, qp, bitrate)?);
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::Vp8 && dst.params.media_type() == MediaType::Video => {
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = Vp8Encoder::new(&dst.params, qp, bitrate)?;
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::Mpeg4 && dst.params.media_type() == MediaType::Video => {
                    // for MPEG-4 a constant quality is the quantiser itself, 1 to 31
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = video.apply_mpeg4(Mpeg4Encoder::new(&dst.params, qp, bitrate)?);
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ if dst.codec == CodecId::Mpeg2Video && dst.params.media_type() == MediaType::Video => {
                    // for MPEG-2 too a constant quality is the quantiser code itself, 1 to 31 (the scale is twice it)
                    let (qp, bitrate) = match rate {
                        RateControl::ConstQp(q) => (Some(q), None),
                        RateControl::Bitrate(b) => (None, Some(b)),
                    };
                    let e = video.apply_mpeg2(Mpeg2Encoder::new(&dst.params, qp, bitrate)?);
                    Box::new(match st.keyint {
                        Some(k) => e.with_keyint(k.into()),
                        None => e,
                    })
                }
                _ => {
                    let opus = st.opus.as_ref().map(OpusJob::codec_options);
                    new_encoder_with_opts(dst.codec, &dst.params, dst.bits, st.bitrate_kbps, opus.as_ref(), st.prores_profile.map(ProResProfile::codec_profile), Some(&video))?
                }
            };
            let hw_dec = st.hardware.iter().find(|h| h.direction == Direction::Decode).and_then(|h| h.chosen);
            let decoder = match hw_dec {
                Some(backend) => {
                    crate::policy::current().check_decode(src)?;
                    hw_dec_name = Some(backend.name());
                    new_video_decoder(backend, src.codec, &src.params, &src.extradata)?
                }
                None => crate::policy::new_decoder_for(src)?,
            };
            (Some(decoder), Some(encoder), fs)
        } else {
            (None, None, Vec::new())
        };
        by_input[st.input_file][st.input_stream] = Some(runtimes.len());
        runtimes.push(Runtime {
            action: st.action,
            out_stream: st.output_stream,
            decoder,
            filters,
            encoder,
            rep: StepReport { input_stream: st.input_stream, hardware_encoder: hw_name, hardware_decoder: hw_dec_name, ..Default::default() },
        });
    }

    // Streams the plan does not use need not be read from disk.
    for (file, d) in demuxes.iter_mut().enumerate() {
        for (stream, used) in by_input[file].iter().enumerate() {
            if used.is_none() {
                d.discard(stream);
            }
        }
    }

    thread::scope(|s| {
        // One reader thread per input file, each feeding its own bounded queue; `process`
        // merges the queues by timestamp so the muxer sees the streams interleaved.
        let mut rx_in = Vec::new();
        let mut readers = Vec::new();
        for mut demux in demuxes {
            let (tx, rx) = sync_channel::<Packet>(QUEUE_DEPTH);
            rx_in.push(rx);
            readers.push(s.spawn(move || -> Result<()> {
                while let Some(p) = demux.read_packet()? {
                    if tx.send(p).is_err() {
                        break; // downstream failed; its error is reported instead
                    }
                }
                Ok(())
            }));
        }
        let bases: Vec<Vec<Rational>> = (0..rx_in.len()).map(|f| plan.input_at(f).streams.iter().map(|s| s.time_base).collect()).collect();
        let writer = s.spawn(move || -> Result<()> {
            mux.write_header(out_streams)?;
            for p in rx_out {
                mux.write_packet(&p)?;
            }
            mux.finish()
        });

        let processed = process(Merge::new(rx_in, bases), tx_out, &mut runtimes, &by_input, t0, on_progress, cancel);

        let writer = writer.join().map_err(|_| Error::new(ErrorCode::Internal, "writer thread panicked"));
        let readers: Vec<_> = readers.into_iter().map(|r| r.join().map_err(|_| Error::new(ErrorCode::Internal, "reader thread panicked"))).collect();
        let (bytes_in, bytes_out) = processed?;
        writer??;
        for r in readers {
            r??;
        }
        Ok((runtimes.into_iter().map(|r| r.rep).collect(), bytes_in, bytes_out))
    })
}

/// Takes packets from several inputs in timestamp order. Each input is already ordered, so it
/// is enough to hold one packet per input and always release the earliest; ties go to the
/// earlier input. A single input passes straight through.
struct Merge {
    rxs: Vec<Receiver<Packet>>,
    heads: Vec<Option<Packet>>,
    done: Vec<bool>,
    bases: Vec<Vec<Rational>>,
}

impl Merge {
    fn new(rxs: Vec<Receiver<Packet>>, bases: Vec<Vec<Rational>>) -> Self {
        let n = rxs.len();
        Self { rxs, heads: (0..n).map(|_| None).collect(), done: vec![false; n], bases }
    }

    fn next(&mut self) -> Option<(usize, Packet)> {
        for i in 0..self.rxs.len() {
            if self.heads[i].is_none() && !self.done[i] {
                match self.rxs[i].recv() {
                    Ok(p) => self.heads[i] = Some(p),
                    Err(_) => self.done[i] = true, // reader finished (or failed; join reports it)
                }
            }
        }
        let seconds = |i: usize, p: &Packet| p.pts as f64 * self.bases[i].get(p.stream).map_or(0.0, |b| b.as_f64());
        let pick = (0..self.heads.len())
            .filter_map(|i| self.heads[i].as_ref().map(|p| (i, seconds(i, p))))
            .min_by(|a, b| a.1.total_cmp(&b.1))?
            .0;
        self.heads[pick].take().map(|p| (pick, p))
    }
}

fn process(
    mut merge: Merge,
    tx: std::sync::mpsc::SyncSender<Packet>,
    runtimes: &mut [Runtime],
    by_input: &[Vec<Option<usize>>],
    t0: Instant,
    on_progress: &mut dyn FnMut(&Progress),
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(u64, u64)> {
    let (mut bytes_in, mut bytes_out, mut count) = (0u64, 0u64, 0u64);

    macro_rules! emit {
        ($rt:expr, $packets:expr) => {
            for mut q in $packets {
                q.stream = $rt.out_stream;
                $rt.rep.packets_out += 1;
                $rt.rep.bytes_out += q.data.len() as u64;
                bytes_out += q.data.len() as u64;
                if tx.send(q).is_err() {
                    return Ok((bytes_in, bytes_out)); // writer failed; its error is reported instead
                }
            }
        };
    }

    while let Some((file, p)) = merge.next() {
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(Error::new(ErrorCode::Cancelled, "the job was cancelled; nothing was written"));
        }
        bytes_in += p.data.len() as u64;
        let Some(&Some(ri)) = by_input[file].get(p.stream) else { continue };
        let rt = &mut runtimes[ri];
        rt.rep.packets_in += 1;
        let out = match rt.action {
            Action::Copy => vec![p],
            Action::Transcode => {
                let frames = rt.decoder.as_mut().expect("transcode steps have a decoder").decode(&p)?;
                rt.through(frames)?
            }
        };
        emit!(rt, out);
        count += 1;
        if count % PROGRESS_EVERY == 0 {
            on_progress(&Progress { packets: count, bytes_in, bytes_out, elapsed_ms: t0.elapsed().as_millis() as u64 });
        }
    }

    for rt in runtimes.iter_mut() {
        if rt.action == Action::Transcode {
            let frames = rt.decoder.as_mut().expect("decoder").flush()?;
            let mut out = rt.through(frames)?;
            // a filter with look-ahead hands over what it held back, for the filters after it
            for i in 0..rt.filters.len() {
                let held = rt.filters[i].flush()?;
                out.extend(rt.through_from(i + 1, held)?);
            }
            out.extend(rt.encoder.as_mut().expect("encoder").flush()?);
            emit!(rt, out);
        }
    }
    Ok((bytes_in, bytes_out))
}
