use crate::job::Job;
use crate::plan::{plan, Action, Plan};
use ironav_codec::{build_filter, new_decoder, new_encoder};
use ironav_core::{Decoder, Encoder, Error, ErrorCode, Filter, Frame, Packet, Result};
use ironav_formats::{create_muxer, open_demuxer};
use serde::Serialize;
use std::path::Path;
use std::sync::mpsc::sync_channel;
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
        let mut cur = frames;
        for f in &mut self.filters {
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
    let t0 = Instant::now();
    let plan = plan(job)?;
    let demux = open_demuxer(Path::new(&job.input))?;
    let partial = format!("{}.partial", job.output);
    let mux = create_muxer(Path::new(&partial), plan.output.format)?;

    match execute(&plan, demux, mux, t0, on_progress) {
        Ok((steps, bytes_in, bytes_out)) => {
            std::fs::rename(&partial, &job.output)?;
            let wall = t0.elapsed();
            let secs = wall.as_secs_f64().max(1e-9);
            let media_seconds = plan.output.streams.first().and_then(|s| s.duration_secs());
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
    mut demux: Box<dyn ironav_core::Demuxer>,
    mut mux: Box<dyn ironav_core::Muxer>,
    t0: Instant,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Outcome> {
    let (tx_in, rx_in) = sync_channel::<Packet>(QUEUE_DEPTH);
    let (tx_out, rx_out) = sync_channel::<Packet>(QUEUE_DEPTH);
    let out_streams = &plan.output.streams;

    let mut runtimes: Vec<Runtime> = Vec::new();
    let mut by_input: Vec<Option<usize>> = vec![None; plan.input.streams.len()];
    for st in &plan.steps {
        let src = &plan.input.streams[st.input_stream];
        let dst = &plan.output.streams[st.output_stream];
        let (decoder, encoder, filters) = if st.action == Action::Transcode {
            let mut fs = Vec::new();
            for fc in &st.filters {
                fs.push(build_filter(&fc.name, &fc.args)?);
            }
            (Some(new_decoder(src.codec, &src.params, src.bits)?), Some(new_encoder(dst.codec, &dst.params, dst.bits)?), fs)
        } else {
            (None, None, Vec::new())
        };
        by_input[st.input_stream] = Some(runtimes.len());
        runtimes.push(Runtime {
            action: st.action,
            out_stream: st.output_stream,
            decoder,
            filters,
            encoder,
            rep: StepReport { input_stream: st.input_stream, ..Default::default() },
        });
    }

    thread::scope(|s| {
        let reader = s.spawn(move || -> Result<()> {
            while let Some(p) = demux.read_packet()? {
                if tx_in.send(p).is_err() {
                    break; // downstream failed; its error is reported instead
                }
            }
            Ok(())
        });
        let writer = s.spawn(move || -> Result<()> {
            mux.write_header(out_streams)?;
            for p in rx_out {
                mux.write_packet(&p)?;
            }
            mux.finish()
        });

        let processed = process(rx_in, tx_out, &mut runtimes, &by_input, t0, on_progress);

        let writer = writer.join().map_err(|_| Error::new(ErrorCode::Internal, "writer thread panicked"));
        let reader = reader.join().map_err(|_| Error::new(ErrorCode::Internal, "reader thread panicked"));
        let (bytes_in, bytes_out) = processed?;
        writer??;
        reader??;
        Ok((runtimes.into_iter().map(|r| r.rep).collect(), bytes_in, bytes_out))
    })
}

fn process(
    rx: std::sync::mpsc::Receiver<Packet>,
    tx: std::sync::mpsc::SyncSender<Packet>,
    runtimes: &mut [Runtime],
    by_input: &[Option<usize>],
    t0: Instant,
    on_progress: &mut dyn FnMut(&Progress),
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

    for p in rx {
        bytes_in += p.data.len() as u64;
        let Some(&Some(ri)) = by_input.get(p.stream) else { continue };
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
            out.extend(rt.encoder.as_mut().expect("encoder").flush()?);
            emit!(rt, out);
        }
    }
    Ok((bytes_in, bytes_out))
}
