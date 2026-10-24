use super::bits::BitReader;
use super::crc::crc16;
use super::header::{parse_header, Assignment, HeaderError};
use ironav_core::{AudioFrame, Decoder, Error, Frame, Packet, Result};

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("FLAC: {msg}"))
}

/// One decoded frame: per-channel integer samples at `bits_per_sample`.
pub struct Decoded {
    pub channels: Vec<Vec<i64>>,
    pub bits_per_sample: u32,
    pub sample_rate: Option<u32>,
}

/// Decode one complete FLAC frame (header through CRC-16). `default_bps` is STREAMINFO's depth,
/// used when the frame header defers to it.
pub fn decode_frame(data: &[u8], default_bps: u32) -> Result<Decoded> {
    let h = parse_header(data).map_err(|e| match e {
        HeaderError::Incomplete => bad("truncated frame header"),
        HeaderError::Invalid(m) => bad(m),
    })?;
    if data.len() < h.len + 2 {
        return Err(bad("frame too short"));
    }
    let body_end = data.len() - 2;
    if crc16(&data[..body_end]) != u16::from_be_bytes([data[body_end], data[body_end + 1]]) {
        return Err(bad("frame CRC-16 mismatch (corrupt data)"));
    }
    let bps = h.bits_per_sample.map(u32::from).unwrap_or(default_bps);
    if !(4..=32).contains(&bps) {
        return Err(bad("unsupported bits per sample"));
    }
    let n = h.block_size as usize;
    let mut br = BitReader::new(&data[h.len..body_end]);
    let nch = h.assignment.channels();
    let mut chans = Vec::with_capacity(nch);
    for ch in 0..nch {
        let side = match h.assignment {
            Assignment::LeftSide => ch == 1,
            Assignment::SideRight => ch == 0,
            Assignment::MidSide => ch == 1,
            Assignment::Independent(_) => false,
        };
        chans.push(decode_subframe(&mut br, n, bps + side as u32)?);
    }
    match h.assignment {
        Assignment::Independent(_) => {}
        Assignment::LeftSide => {
            for i in 0..n {
                chans[1][i] = chans[0][i] - chans[1][i];
            }
        }
        Assignment::SideRight => {
            for i in 0..n {
                chans[0][i] += chans[1][i];
            }
        }
        Assignment::MidSide => {
            for i in 0..n {
                let side = chans[1][i];
                let mid = (chans[0][i] << 1) | (side & 1);
                chans[0][i] = (mid + side) >> 1;
                chans[1][i] = (mid - side) >> 1;
            }
        }
    }
    Ok(Decoded { channels: chans, bits_per_sample: bps, sample_rate: h.sample_rate })
}

fn decode_subframe(br: &mut BitReader, n: usize, bps: u32) -> Result<Vec<i64>> {
    if br.read(1)? != 0 {
        return Err(bad("subframe padding bit set"));
    }
    let kind = br.read(6)?;
    let wasted = if br.read(1)? == 1 { br.read_unary()? + 1 } else { 0 };
    if wasted >= bps {
        return Err(bad("wasted bits exceed sample size"));
    }
    let eff = bps - wasted;
    let mut out = vec![0i64; n];
    match kind {
        0 => {
            let v = br.read_signed(eff)?;
            out.fill(v);
        }
        1 => {
            for s in &mut out {
                *s = br.read_signed(eff)?;
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            if order > n {
                return Err(bad("predictor order exceeds block size"));
            }
            for s in out.iter_mut().take(order) {
                *s = br.read_signed(eff)?;
            }
            read_residual(br, n, order, &mut out)?;
            restore_fixed(&mut out, order);
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            if order > n {
                return Err(bad("predictor order exceeds block size"));
            }
            for s in out.iter_mut().take(order) {
                *s = br.read_signed(eff)?;
            }
            let precision = br.read(4)? + 1;
            if precision == 16 {
                return Err(bad("invalid LPC precision"));
            }
            let shift = br.read_signed(5)?;
            if shift < 0 {
                return Err(bad("negative LPC shift"));
            }
            let coefs: Vec<i64> = (0..order).map(|_| br.read_signed(precision)).collect::<Result<_>>()?;
            read_residual(br, n, order, &mut out)?;
            for i in order..n {
                let mut sum = 0i64;
                for (j, &c) in coefs.iter().enumerate() {
                    sum += c * out[i - 1 - j];
                }
                out[i] += sum >> shift;
            }
        }
        _ => return Err(bad("reserved subframe type")),
    }
    if wasted > 0 {
        for s in &mut out {
            *s <<= wasted;
        }
    }
    Ok(out)
}

fn read_residual(br: &mut BitReader, n: usize, order: usize, out: &mut [i64]) -> Result<()> {
    let method = br.read(2)?;
    if method > 1 {
        return Err(bad("reserved residual coding method"));
    }
    let param_bits = 4 + method;
    let escape = (1u32 << param_bits) - 1;
    let porder = br.read(4)?;
    let parts = 1usize << porder;
    if n % parts != 0 || (n >> porder) < order {
        return Err(bad("invalid residual partition order"));
    }
    let psize = n >> porder;
    let mut idx = order;
    for p in 0..parts {
        let count = if p == 0 { psize - order } else { psize };
        let k = br.read(param_bits)?;
        if k == escape {
            let bits = br.read(5)?;
            for _ in 0..count {
                out[idx] = br.read_signed(bits)?;
                idx += 1;
            }
        } else {
            for _ in 0..count {
                let q = br.read_unary()? as u64;
                let r = if k > 0 { br.read(k)? as u64 } else { 0 };
                let u = (q << k) | r;
                out[idx] = (u >> 1) as i64 ^ -((u & 1) as i64);
                idx += 1;
            }
        }
    }
    Ok(())
}

fn restore_fixed(out: &mut [i64], order: usize) {
    let n = out.len();
    match order {
        0 => {}
        1 => (1..n).for_each(|i| out[i] += out[i - 1]),
        2 => (2..n).for_each(|i| out[i] += 2 * out[i - 1] - out[i - 2]),
        3 => (3..n).for_each(|i| out[i] += 3 * out[i - 1] - 3 * out[i - 2] + out[i - 3]),
        _ => (4..n).for_each(|i| out[i] += 4 * out[i - 1] - 6 * out[i - 2] + 4 * out[i - 3] - out[i - 4]),
    }
}

/// Packet-level decoder: each packet is exactly one FLAC frame.
pub struct FlacDecoder {
    sample_rate: u32,
    channels: u16,
    default_bps: u32,
}

impl FlacDecoder {
    pub fn new(sample_rate: u32, channels: u16, bits: Option<u8>) -> Self {
        Self { sample_rate, channels, default_bps: bits.unwrap_or(16) as u32 }
    }
}

impl Decoder for FlacDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let d = decode_frame(&packet.data, self.default_bps)?;
        if d.channels.len() != self.channels as usize {
            return Err(bad("frame channel count differs from the stream's"));
        }
        let n = d.channels[0].len();
        let scale = 1.0 / (1u64 << (d.bits_per_sample - 1)) as f64;
        let mut samples = Vec::with_capacity(n * d.channels.len());
        for i in 0..n {
            for ch in &d.channels {
                samples.push(ch[i] as f64 * scale);
            }
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: d.sample_rate.unwrap_or(self.sample_rate), channels: self.channels, pts: packet.pts, samples })])
    }
}
