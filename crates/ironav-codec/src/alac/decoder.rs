use super::{bad, frame_mult, lpc_adapt, lpc_base, parse_config, rice_k, run_k, sign_extend, unzigzag, update_history, Config, INITIAL_HISTORY, LIMIT};
use crate::flac::bits::BitReader;
use ironav_core::{AudioFrame, Decoder, Error, Frame, Packet, Result, StreamParams};

const END: u32 = 7;
const CPE: u32 = 1;

/// The fields a compressed channel's header gives: predictor order, quantizer, Rice multiplier
/// and the coefficients in filter order (the oldest sample's first).
struct Lpc {
    prediction: u32,
    quant: u32,
    mult: u32,
    coefs: Vec<i16>,
}

/// One Rice-coded value: a unary prefix and a k-bit suffix, or 16 or `bps` raw bits when the
/// prefix is 9 ones.
fn decode_scalar(r: &mut BitReader, k: u32, bps: u32) -> Result<u32> {
    let mut x = 0u32;
    while x < 9 && r.read(1)? == 1 {
        x += 1;
    }
    if x > 8 {
        return r.read(bps);
    }
    if k != 1 {
        let extra = r.peek(k);
        x = (x << k) - x;
        if extra > 1 {
            x += extra - 1;
            r.read(k)?;
        } else {
            r.read(k - 1)?;
        }
    }
    Ok(x)
}

/// The residuals of one channel: adaptive Rice codes, where a low history is followed by a run
/// of zero samples (possibly empty) and the sample after the run is coded one lower.
fn read_residuals(r: &mut BitReader, n: usize, bps: u32, mult: u32) -> Result<Vec<i32>> {
    let mut out = vec![0i32; n];
    let mut history = INITIAL_HISTORY;
    let mut bump = 0u32;
    let mut i = 0;
    while i < n {
        let x = decode_scalar(r, rice_k(history, LIMIT), bps)?.wrapping_add(bump);
        bump = 0;
        out[i] = unzigzag(x);
        history = update_history(history, x, mult);
        if history < 128 && i + 1 < n {
            let run = decode_scalar(r, run_k(history, LIMIT), 16)? as usize;
            if run > 0 {
                if run >= n - i {
                    return Err(bad("zero run reaches past the end of the frame"));
                }
                i += run;
            }
            if run <= 0xffff {
                bump = 1;
            }
            history = 0;
        }
        i += 1;
    }
    Ok(out)
}

/// Run the predictor over a channel's residuals. Order 31 is a plain first difference.
fn synthesize(res: &[i32], bps: u32, order: usize, quant: u32, coefs: &mut [i16]) -> Vec<i32> {
    let n = res.len();
    let mut out = vec![0i32; n];
    if n == 0 {
        return out;
    }
    out[0] = res[0];
    if order == 0 {
        out.copy_from_slice(res);
        return out;
    }
    if order == 31 {
        for i in 1..n {
            out[i] = sign_extend(out[i - 1].wrapping_add(res[i]), bps);
        }
        return out;
    }
    for i in 1..n.min(order + 1) {
        out[i] = sign_extend(out[i - 1].wrapping_add(res[i]), bps);
    }
    for i in order + 1..n {
        out[i] = sign_extend(lpc_base(&out, i, coefs, quant).wrapping_add(res[i]), bps);
        lpc_adapt(&out, i, coefs, quant, res[i]);
    }
    out
}

/// Decode one channel's samples from its header and residuals.
fn channel(res: &[i32], bps: u32, lpc: &mut Lpc) -> Vec<i32> {
    if lpc.prediction == 15 {
        // prediction type 15 runs the first-difference pass before the coded predictor
        let first = synthesize(res, bps, 31, 0, &mut []);
        return synthesize(&first, bps, lpc.coefs.len(), lpc.quant, &mut lpc.coefs);
    }
    synthesize(res, bps, lpc.coefs.len(), lpc.quant, &mut lpc.coefs)
}

/// One element (a mono or a stereo pair) of a frame. Returns its channels' samples.
fn element(r: &mut BitReader, cfg: &Config, count: u32) -> Result<Vec<Vec<i32>>> {
    r.read(4)?; // element instance tag
    r.read(12)?; // unused
    let has_size = r.read(1)? == 1;
    let extra = r.read(2)? * 8;
    let compressed = r.read(1)? == 0;
    let n = if has_size { r.read(32)? } else { cfg.frame_length };
    if n == 0 || n > cfg.frame_length {
        return Err(bad("invalid number of samples in a frame"));
    }
    let n = n as usize;
    let bps = (cfg.bits as i64 - extra as i64) + count as i64 - 1;
    if !(1..=32).contains(&bps) {
        return Err(bad("sample size out of range"));
    }
    let bps = bps as u32;
    if !compressed {
        let mut out = vec![vec![0i32; n]; count as usize];
        for i in 0..n {
            for ch in out.iter_mut() {
                ch[i] = r.read_signed(cfg.bits)? as i32;
            }
        }
        return Ok(out);
    }

    let mix_shift = r.read(8)?;
    let mix_weight = r.read(8)?;
    if count == 2 && mix_weight != 0 && mix_shift > 31 {
        return Err(bad("stereo decorrelation shift out of range"));
    }
    let mut lpcs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let prediction = r.read(4)?;
        let quant = r.read(4)?;
        let mult = frame_mult(r.read(3)?, cfg.pb);
        let order = r.read(5)? as usize;
        if order >= cfg.frame_length as usize || quant == 0 {
            return Err(bad("invalid predictor in a frame"));
        }
        let mut coefs = vec![0i16; order];
        for c in coefs.iter_mut().rev() {
            *c = r.read_signed(16)? as i16;
        }
        lpcs.push(Lpc { prediction, quant, mult, coefs });
    }

    // low bits that the predictor does not code, interleaved per sample
    let mut low = vec![vec![0u32; n]; count as usize];
    if extra > 0 {
        for i in 0..n {
            for ch in low.iter_mut() {
                ch[i] = r.read(extra)?;
            }
        }
    }

    let mut out = Vec::with_capacity(count as usize);
    for lpc in lpcs.iter_mut() {
        let res = read_residuals(r, n, bps, lpc.mult)?;
        out.push(channel(&res, bps, lpc));
    }

    if count == 2 && mix_weight != 0 {
        let w = mix_weight as i32;
        for i in 0..n {
            let (x, y) = (out[0][i], out[1][i]);
            let a = x.wrapping_sub(y.wrapping_mul(w) >> mix_shift);
            out[0][i] = y.wrapping_add(a);
            out[1][i] = a;
        }
    }
    if extra > 0 {
        for (ch, l) in out.iter_mut().zip(&low) {
            for (s, &b) in ch.iter_mut().zip(l) {
                *s = ((*s as u32) << extra | b) as i32;
            }
        }
    }
    Ok(out)
}

/// One ALAC frame as per-channel samples at the configured bit depth.
pub(crate) fn decode_frame(data: &[u8], cfg: &Config) -> Result<Vec<Vec<i32>>> {
    let mut r = BitReader::new(data);
    let mut chans: Vec<Vec<i32>> = Vec::new();
    loop {
        let tag = r.read(3)?;
        if tag == END {
            break;
        }
        let count = if tag == CPE { 2 } else { 1 };
        if tag > CPE && tag != 3 {
            return Err(bad("unsupported syntax element"));
        }
        if chans.len() + count > cfg.channels as usize {
            return Err(bad("frame has more channels than the stream"));
        }
        chans.extend(element(&mut r, cfg, count as u32)?);
    }
    if chans.len() != cfg.channels as usize {
        return Err(bad("frame has fewer channels than the stream"));
    }
    if chans.iter().any(|c| c.len() != chans[0].len()) {
        return Err(bad("channels of a frame differ in length"));
    }
    Ok(chans)
}

/// Packet-level decoder: each packet is exactly one ALAC frame.
pub struct AlacDecoder {
    cfg: Config,
    sample_rate: u32,
    channels: u16,
}

impl AlacDecoder {
    pub fn new(extradata: &[u8], params: &StreamParams) -> Result<Self> {
        let StreamParams::Audio { sample_rate, channels } = params else {
            return Err(bad("the stream is not audio"));
        };
        let cfg = parse_config(extradata)?;
        if cfg.channels != *channels as u32 {
            return Err(bad("the configuration's channel count differs from the stream's"));
        }
        if cfg.channels > 2 {
            return Err(Error::new(ironav_core::ErrorCode::UnsupportedCodec, "ALAC with more than two channels is not implemented").hint("the stream is not decoded; convert it to stereo or mono first"));
        }
        if !matches!(cfg.bits, 16 | 20 | 24 | 32) {
            return Err(bad("sample size must be 16, 20, 24 or 32 bits"));
        }
        Ok(Self { cfg, sample_rate: *sample_rate, channels: *channels })
    }
}

impl Decoder for AlacDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let chans = decode_frame(&packet.data, &self.cfg)?;
        let n = chans[0].len();
        let scale = 1.0 / (1u64 << (self.cfg.bits - 1)) as f64;
        let mut samples = Vec::with_capacity(n * chans.len());
        for i in 0..n {
            for ch in &chans {
                samples.push(ch[i] as f64 * scale);
            }
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.sample_rate, channels: self.channels, pts: packet.pts, samples })])
    }
}
