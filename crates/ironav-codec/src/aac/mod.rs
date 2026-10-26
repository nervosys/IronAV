//! AAC-LC decoder (ISO/IEC 14496-3): single and paired channel elements, M/S and intensity
//! stereo, PNS, TNS, pulses, all window shapes and sequences. Not decoded: SBR/PS (HE-AAC),
//! Main-profile prediction, LTP, coupling channels, 960-sample frames and gain control; streams
//! that need them are refused with an error rather than decoded wrongly.

mod imdct;
mod tables;

use imdct::Imdct;
use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};
use std::sync::OnceLock;
use tables::*;

const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

const LONG_START: u8 = 1;
const EIGHT_SHORT: u8 = 2;
const LONG_STOP: u8 = 3;

const ZERO_BT: u8 = 0;
const NOISE_BT: u8 = 13;
const INTENSITY_BT2: u8 = 14;
const INTENSITY_BT: u8 = 15;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("AAC: {msg}"))
}

fn unsupported(msg: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, format!("AAC: {msg}")).hint("this AAC tool is not implemented; AAC-LC stereo/mono/5.1 without SBR is")
}

// ---- bit reader ------------------------------------------------------------------------------

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn left(&self) -> isize {
        self.data.len() as isize * 8 - self.pos as isize
    }

    /// The next `n` (at most 25) bits without consuming them; zeros past the end.
    fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let v = match self.data.get(byte..byte + 8) {
            Some(w) => u64::from_be_bytes(w.try_into().unwrap()),
            None => {
                let mut w = [0u8; 8];
                let avail = self.data.len().saturating_sub(byte).min(8);
                w[..avail].copy_from_slice(&self.data[byte..byte + avail]);
                u64::from_be_bytes(w)
            }
        };
        ((v << (self.pos & 7)) >> (64 - n)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    fn flag(&mut self) -> bool {
        self.read(1) != 0
    }

    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    fn check(&self) -> Result<()> {
        if self.left() < 0 {
            Err(bad("the frame ends in the middle of an element"))
        } else {
            Ok(())
        }
    }
}

// ---- Huffman tables --------------------------------------------------------------------------

struct Vlc {
    bits: u32,
    /// Indexed by the next `bits` bits: `length << 16 | symbol`, or 0 for a bit pattern no code uses.
    lut: Vec<u32>,
}

impl Vlc {
    fn build(codes: impl Iterator<Item = (u32, u32)>, bits: u32) -> Self {
        let mut lut = vec![0u32; 1 << bits];
        for (sym, (code, len)) in codes.enumerate() {
            let span = 1usize << (bits - len);
            let at = (code as usize) << (bits - len);
            lut[at..at + span].fill(len << 16 | sym as u32);
        }
        Self { bits, lut }
    }

    fn get(&self, br: &mut Bits) -> Result<u32> {
        let e = self.lut[br.peek(self.bits) as usize];
        if e == 0 {
            return Err(bad("invalid Huffman code"));
        }
        br.skip(e >> 16);
        Ok(e & 0xFFFF)
    }
}

struct Codebooks {
    spectral: Vec<Vlc>,
    scalefactor: Vlc,
}

fn codebooks() -> &'static Codebooks {
    static CB: OnceLock<Codebooks> = OnceLock::new();
    CB.get_or_init(|| {
        let spectral = (0..11)
            .map(|i| {
                let bits = SPECTRAL_BITS[i];
                let max = *bits.iter().max().unwrap() as u32;
                Vlc::build(SPECTRAL_CODES[i].iter().zip(bits).map(|(&c, &b)| (c as u32, b as u32)), max)
            })
            .collect();
        let max = *SCALEFACTOR_BITS.iter().max().unwrap() as u32;
        let scalefactor = Vlc::build(SCALEFACTOR_CODE.iter().zip(&SCALEFACTOR_BITS).map(|(&c, &b)| (c, b as u32)), max);
        Codebooks { spectral, scalefactor }
    })
}

/// `|q|^(4/3)` for every quantised magnitude an escape sequence can produce.
fn pow43() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| (0..8192).map(|q| (q as f64).powf(4.0 / 3.0) as f32).collect())
}

struct Windows {
    long_rise: Vec<f32>,
    short_rise: Vec<f32>,
    kbd_long_rise: Vec<f32>,
    kbd_short_rise: Vec<f32>,
    long_fall: Vec<f32>,
    short_fall: Vec<f32>,
    kbd_long_fall: Vec<f32>,
    kbd_short_fall: Vec<f32>,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..60 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

/// Rising half (`n` values) of a Kaiser-Bessel-derived window of `2n` samples.
fn kbd(alpha: f64, n: usize) -> Vec<f32> {
    let alpha2 = 4.0 * (alpha * std::f64::consts::PI / n as f64).powi(2);
    let temp: Vec<f64> = (0..=n / 2).map(|i| bessel_i0((i as f64 * (n - i) as f64 * alpha2).sqrt())).collect();
    let scale = 1.0 / (temp.iter().enumerate().map(|(i, &t)| t * if i > 0 && i < n / 2 { 2.0 } else { 1.0 }).sum::<f64>() + 1.0);
    let mut sum = 0.0;
    (0..n)
        .map(|i| {
            sum += if i <= n / 2 { temp[i] } else { temp[n - i] };
            (sum * scale).sqrt() as f32
        })
        .collect()
}

fn windows() -> &'static Windows {
    static W: OnceLock<Windows> = OnceLock::new();
    W.get_or_init(|| {
        let sine = |n: usize| (0..n).map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / (2 * n) as f64).sin() as f32).collect();
        let rev = |v: &Vec<f32>| v.iter().rev().copied().collect::<Vec<f32>>();
        let (long_rise, short_rise, kbd_long_rise, kbd_short_rise): (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) = (sine(1024), sine(128), kbd(4.0, 1024), kbd(6.0, 128));
        Windows {
            long_fall: rev(&long_rise),
            short_fall: rev(&short_rise),
            kbd_long_fall: rev(&kbd_long_rise),
            kbd_short_fall: rev(&kbd_short_rise),
            long_rise,
            short_rise,
            kbd_long_rise,
            kbd_short_rise,
        }
    })
}

// ---- stream structure ------------------------------------------------------------------------

#[derive(Clone)]
struct IcsInfo {
    window_sequence: u8,
    use_kbd: bool,
    max_sfb: usize,
    num_window_groups: usize,
    group_len: [usize; 8],
    num_windows: usize,
    swb: &'static [u16],
    num_swb: usize,
    tns_max_bands: usize,
}

#[derive(Default, Clone)]
struct Tns {
    present: bool,
    /// Per window: filters of (length, order, direction, coefficients).
    filters: Vec<Vec<(usize, usize, bool, [f32; 20])>>,
}

struct Channel {
    info: IcsInfo,
    band_type: Vec<u8>,
    /// Per band: `sfo` as in the bitstream with 100 removed (scalefactor), intensity position, or noise energy.
    sfo: Vec<i32>,
    coefs: Vec<f32>,
    tns: Tns,
}

/// Overlap-add memory of one output channel.
struct ChannelState {
    saved: Vec<f32>,
    prev_kbd: bool,
}

struct Config {
    sfi: usize,
    channels: u16,
    sample_rate: u32,
    /// Output channel for the n-th channel decoded in a frame.
    map: &'static [usize],
}

fn parse_asc(asc: &[u8], declared_rate: u32, declared_channels: u16) -> Result<Config> {
    let mut b = Bits::new(asc);
    let mut aot = b.read(5);
    if aot == 31 {
        aot = 32 + b.read(6);
    }
    if aot == 5 || aot == 29 {
        return Err(unsupported("HE-AAC (SBR/PS) is not decoded"));
    }
    if aot != 1 && aot != 2 {
        return Err(unsupported(format!("audio object type {aot}")));
    }
    let mut sfi = b.read(4) as usize;
    let mut rate = 0;
    if sfi == 15 {
        rate = b.read(24);
        sfi = SAMPLE_RATES.iter().position(|&r| r == rate).unwrap_or_else(|| {
            SAMPLE_RATES.iter().enumerate().min_by_key(|(_, &r)| (r as i64 - rate as i64).abs()).map(|(i, _)| i).unwrap()
        });
    } else if sfi >= SAMPLE_RATES.len() {
        return Err(bad("reserved sampling frequency index"));
    }
    let channel_config = b.read(4) as usize;
    if b.flag() {
        return Err(unsupported("960-sample frames"));
    }
    if b.flag() {
        b.skip(14); // core coder delay
    }
    let ext = b.flag();
    if ext {
        // version-3 extension flags would follow for the ER object types only
    }
    // Backward-compatible signalling of SBR / PS after the GA specific config.
    if b.left() >= 16 && b.peek(11) == 0x2b7 {
        b.skip(11);
        let ext_aot = b.read(5);
        if ext_aot == 5 && b.flag() {
            return Err(unsupported("HE-AAC (SBR) is not decoded"));
        }
    }
    let (channels, map): (u16, &'static [usize]) = match channel_config {
        1 => (1, &[0]),
        2 => (2, &[0, 1]),
        3 => (3, &[2, 0, 1]),
        4 => (4, &[2, 0, 1, 3]),
        5 => (5, &[2, 0, 1, 3, 4]),
        6 => (6, &[2, 0, 1, 4, 5, 3]),
        7 => (8, &[2, 0, 1, 6, 7, 4, 5, 3]),
        _ => return Err(unsupported("a channel layout given by a program config element")),
    };
    if declared_channels != 0 && declared_channels != channels {
        return Err(bad(format!("the stream says {declared_channels} channels but its AudioSpecificConfig says {channels}")));
    }
    let sample_rate = if rate != 0 { rate } else { SAMPLE_RATES[sfi] };
    if declared_rate != 0 && declared_rate != sample_rate {
        // The container's rate field is only a hint (HE-AAC doubles it); the config is authoritative.
    }
    Ok(Config { sfi, channels, sample_rate, map })
}

pub struct AacDecoder {
    cfg: Config,
    long: Imdct,
    short: Imdct,
    z: Vec<f32>,
    y: Vec<f32>,
    state: Vec<ChannelState>,
    random: u32,
}

impl AacDecoder {
    pub fn new(extradata: &[u8], params: &StreamParams) -> Result<Self> {
        let StreamParams::Audio { sample_rate, channels } = params else {
            return Err(Error::invalid_arg("AAC decoder needs an audio stream"));
        };
        if extradata.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "AAC decoding needs the stream's AudioSpecificConfig")
                .hint("the container did not give the codec configuration; MP4, Matroska and ADTS files do"));
        }
        let cfg = parse_asc(extradata, *sample_rate, *channels)?;
        let state = (0..cfg.channels).map(|_| ChannelState { saved: vec![0.0; 1024], prev_kbd: false }).collect();
        Ok(Self { cfg, long: Imdct::new(1024), short: Imdct::new(128), z: vec![0.0; 2048], y: vec![0.0; 2048], state, random: 0x1f2e3d4c })
    }

    fn ics_info(&self, br: &mut Bits) -> Result<IcsInfo> {
        br.read(1); // reserved
        let window_sequence = br.read(2) as u8;
        let use_kbd = br.flag();
        let sfi = self.cfg.sfi;
        let mut group_len = [0usize; 8];
        group_len[0] = 1;
        let mut groups = 1;
        let info = if window_sequence == EIGHT_SHORT {
            let max_sfb = br.read(4) as usize;
            for _ in 0..7 {
                if br.flag() {
                    group_len[groups - 1] += 1;
                } else {
                    groups += 1;
                    group_len[groups - 1] = 1;
                }
            }
            let swb = SWB_OFFSET_128[sfi];
            IcsInfo { window_sequence, use_kbd, max_sfb, num_window_groups: groups, group_len, num_windows: 8, swb, num_swb: swb.len() - 1, tns_max_bands: TNS_MAX_BANDS_128[sfi] as usize }
        } else {
            let max_sfb = br.read(6) as usize;
            if br.flag() {
                return Err(unsupported("prediction (AAC Main / LTP)"));
            }
            let swb = SWB_OFFSET_1024[sfi];
            IcsInfo { window_sequence, use_kbd, max_sfb, num_window_groups: 1, group_len, num_windows: 1, swb, num_swb: swb.len() - 1, tns_max_bands: TNS_MAX_BANDS_1024[sfi] as usize }
        };
        if info.max_sfb > info.num_swb {
            return Err(bad(format!("{} scalefactor bands exceed the limit of {}", info.max_sfb, info.num_swb)));
        }
        Ok(info)
    }

    /// One individual channel stream. `info` is given when a channel pair shares it.
    fn ics(&mut self, br: &mut Bits, shared: Option<&IcsInfo>) -> Result<Channel> {
        let global_gain = br.read(8) as i32;
        let info = match shared {
            Some(i) => i.clone(),
            None => self.ics_info(br)?,
        };
        let short = info.window_sequence == EIGHT_SHORT;
        let n_bands = info.num_window_groups * info.max_sfb;

        // section data
        let mut band_type = vec![ZERO_BT; n_bands];
        let (len_bits, esc) = if short { (3, 7) } else { (5, 31) };
        for g in 0..info.num_window_groups {
            let mut k = 0;
            while k < info.max_sfb {
                let bt = br.read(4) as u8;
                if bt == 12 {
                    return Err(bad("reserved band type"));
                }
                let mut end = k;
                loop {
                    let inc = br.read(len_bits) as usize;
                    end += inc;
                    if end > info.max_sfb {
                        return Err(bad("a section runs past the last scalefactor band"));
                    }
                    br.check()?;
                    if inc != esc {
                        break;
                    }
                }
                for b in &mut band_type[g * info.max_sfb + k..g * info.max_sfb + end] {
                    *b = bt;
                }
                k = end;
            }
        }

        // scalefactors
        let cb = codebooks();
        let mut sfo = vec![0i32; n_bands];
        let mut offset = [global_gain, global_gain - 90, 0];
        let mut first_noise = true;
        for i in 0..n_bands {
            match band_type[i] {
                ZERO_BT => {}
                INTENSITY_BT | INTENSITY_BT2 => {
                    offset[2] += cb.scalefactor.get(br)? as i32 - 60;
                    sfo[i] = offset[2].clamp(-155, 100);
                }
                NOISE_BT => {
                    if first_noise {
                        offset[1] += br.read(9) as i32 - 256;
                        first_noise = false;
                    } else {
                        offset[1] += cb.scalefactor.get(br)? as i32 - 60;
                    }
                    sfo[i] = offset[1].clamp(-100, 155);
                }
                _ => {
                    offset[0] += cb.scalefactor.get(br)? as i32 - 60;
                    if !(0..=255).contains(&offset[0]) {
                        return Err(bad(format!("scalefactor {} out of range", offset[0])));
                    }
                    sfo[i] = offset[0] - 100;
                }
            }
        }
        br.check()?;

        // pulses, TNS, gain control
        let mut pulses: Vec<(usize, i32)> = Vec::new();
        if br.flag() {
            if short {
                return Err(bad("the pulse tool is not allowed in short windows"));
            }
            let n = br.read(2) as usize + 1;
            let swb = br.read(6) as usize;
            if swb >= info.num_swb {
                return Err(bad("pulse start band out of range"));
            }
            let mut pos = info.swb[swb] as usize + br.read(5) as usize;
            for i in 0..n {
                if i > 0 {
                    pos += br.read(5) as usize;
                }
                if pos >= info.swb[info.num_swb] as usize {
                    return Err(bad("pulse position out of range"));
                }
                pulses.push((pos, br.read(4) as i32));
            }
        }
        let mut tns = Tns::default();
        if br.flag() {
            tns.present = true;
            for _ in 0..info.num_windows {
                let n_filt = br.read(2 - short as u32) as usize;
                let mut filters = Vec::new();
                if n_filt > 0 {
                    let coef_res = br.read(1) as usize;
                    for _ in 0..n_filt {
                        let length = br.read(6 - 2 * short as u32) as usize;
                        let order = br.read(5 - 2 * short as u32) as usize;
                        let max_order = if short { 7 } else { 12 };
                        if order > max_order {
                            return Err(bad(format!("TNS order {order} exceeds {max_order}")));
                        }
                        let mut coefs = [0f32; 20];
                        let mut direction = false;
                        if order > 0 {
                            direction = br.flag();
                            let compress = br.read(1) as usize;
                            let len = (coef_res + 3 - compress) as u32;
                            let map = TNS_MAP[2 * compress + coef_res];
                            for c in coefs.iter_mut().take(order) {
                                *c = map[br.read(len) as usize];
                            }
                        }
                        filters.push((length, order, direction, coefs));
                    }
                }
                tns.filters.push(filters);
            }
        }
        if br.flag() {
            return Err(unsupported("gain control (SSR profile)"));
        }

        let mut ch = Channel { info, band_type, sfo, coefs: vec![0.0; 1024], tns };
        self.spectrum(br, &mut ch, &pulses)?;
        br.check()?;
        Ok(ch)
    }

    fn spectrum(&mut self, br: &mut Bits, ch: &mut Channel, pulses: &[(usize, i32)]) -> Result<()> {
        let info = &ch.info;
        let books = codebooks();
        let p43 = pow43();
        let mut idx = 0;
        let mut win0 = 0;
        for g in 0..info.num_window_groups {
            let g_len = info.group_len[g];
            for sfb in 0..info.max_sfb {
                let bt = ch.band_type[idx];
                let (lo, hi) = (info.swb[sfb] as usize, info.swb[sfb + 1] as usize);
                let width = hi - lo;
                match bt {
                    ZERO_BT | INTENSITY_BT | INTENSITY_BT2 => {}
                    NOISE_BT => {
                        let amp = (ch.sfo[idx] as f64 * 0.25).exp2() as f32;
                        for w in 0..g_len {
                            let base = (win0 + w) * 128 + lo;
                            let band = &mut ch.coefs[base..base + width];
                            let mut energy = 0f64;
                            for c in band.iter_mut() {
                                self.random = self.random.wrapping_mul(1664525).wrapping_add(1013904223);
                                *c = self.random as i32 as f32;
                                energy += *c as f64 * *c as f64;
                            }
                            let scale = amp as f64 / energy.sqrt();
                            for c in band.iter_mut() {
                                *c = (*c as f64 * scale) as f32;
                            }
                        }
                    }
                    cbk => {
                        let gain = (ch.sfo[idx] as f64 * 0.25).exp2() as f32;
                        let vlc = &books.spectral[cbk as usize - 1];
                        let (dim, modulus, signed_off) = match cbk {
                            1 | 2 => (4, 3, 1),
                            3 | 4 => (4, 3, 0),
                            5 | 6 => (2, 9, 4),
                            7 | 8 => (2, 8, 0),
                            9 | 10 => (2, 13, 0),
                            _ => (2, 17, 0),
                        };
                        let unsigned = signed_off == 0;
                        for w in 0..g_len {
                            let base = (win0 + w) * 128 + lo;
                            let mut at = 0;
                            while at < width {
                                let mut sym = vlc.get(br)? as usize;
                                let mut vals = [0i32; 4];
                                for d in (0..dim).rev() {
                                    vals[d] = (sym % modulus) as i32 - signed_off;
                                    sym /= modulus;
                                }
                                if unsigned {
                                    // One sign bit per non-zero value, all together after the codeword.
                                    let nonzero = vals.iter().take(dim).filter(|&&v| v != 0).count() as u32;
                                    let mut signs = br.read(nonzero) << (32 - nonzero.max(1));
                                    for v in vals.iter_mut().take(dim) {
                                        if *v != 0 {
                                            if signs & 0x8000_0000 != 0 {
                                                *v = -*v;
                                            }
                                            signs <<= 1;
                                        }
                                    }
                                    if cbk == 11 {
                                        for v in vals.iter_mut().take(dim) {
                                            if v.abs() == 16 {
                                                let mut n = 0;
                                                while br.flag() {
                                                    n += 1;
                                                    if n > 8 {
                                                        return Err(bad("escape sequence too long"));
                                                    }
                                                }
                                                let mag = (1i32 << (n + 4)) + br.read(n + 4) as i32;
                                                *v = if *v < 0 { -mag } else { mag };
                                            }
                                        }
                                    }
                                }
                                for (j, &v) in vals.iter().take(dim).enumerate() {
                                    let m = p43[v.unsigned_abs() as usize] * gain;
                                    ch.coefs[base + at + j] = if v < 0 { -m } else { m };
                                }
                                at += dim;
                            }
                            br.check()?;
                        }
                    }
                }
                idx += 1;
            }
            win0 += g_len;
        }

        // pulses (long windows only): add to the quantised value, then requantise
        for &(pos, amp) in pulses {
            let sfb = (0..info.num_swb).find(|&b| info.swb[b + 1] as usize > pos).unwrap_or(0);
            if sfb >= info.max_sfb || ch.band_type[sfb] == NOISE_BT || ch.band_type[sfb] == ZERO_BT {
                continue;
            }
            let gain = (ch.sfo[sfb] as f64 * 0.25).exp2();
            let co = ch.coefs[pos] as f64;
            let amp = amp as f64;
            let q = if co != 0.0 {
                let q = co / gain;
                q / q.abs().sqrt().sqrt() + if co > 0.0 { amp } else { -amp }
            } else {
                -amp
            };
            ch.coefs[pos] = (q.abs().cbrt() * q * gain) as f32;
        }
        Ok(())
    }

    fn apply_tns(ch: &mut Channel) {
        let info = &ch.info;
        let mmm = info.tns_max_bands.min(info.max_sfb);
        if mmm == 0 {
            return;
        }
        for (w, filters) in ch.tns.filters.iter().enumerate() {
            let mut bottom = info.num_swb;
            for &(length, order, direction, ref coef) in filters {
                let top = bottom;
                bottom = top.saturating_sub(length);
                if order == 0 {
                    continue;
                }
                let mut lpc = [0f32; 20];
                for i in 0..order {
                    let r = -coef[i];
                    lpc[i] = r;
                    for j in 0..(i + 1) / 2 {
                        let (f, b) = (lpc[j], lpc[i - 1 - j]);
                        lpc[j] = f + r * b;
                        lpc[i - 1 - j] = b + r * f;
                    }
                }
                let start = info.swb[bottom.min(mmm)] as usize;
                let end = info.swb[top.min(mmm)] as usize;
                if end <= start {
                    continue;
                }
                let size = end - start;
                let (mut pos, inc): (isize, isize) = if direction { (end as isize - 1, -1) } else { (start as isize, 1) };
                pos += (w * 128) as isize;
                for m in 0..size {
                    let mut acc = ch.coefs[pos as usize];
                    for i in 1..=m.min(order) {
                        acc -= ch.coefs[(pos - i as isize * inc) as usize] * lpc[i - 1];
                    }
                    ch.coefs[pos as usize] = acc;
                    pos += inc;
                }
            }
        }
    }

    /// Spectrum to 1024 time samples, overlapping with the previous frame.
    fn synthesize(&mut self, ch: &Channel, state: &mut ChannelState, out: &mut [f32]) {
        let info = &ch.info;
        let w = windows();
        let (long_fall, short_rise, short_fall) =
            if info.use_kbd { (&w.kbd_long_fall, &w.kbd_short_rise, &w.kbd_short_fall) } else { (&w.long_fall, &w.short_rise, &w.short_fall) };
        let (prev_long, prev_short) = if state.prev_kbd { (&w.kbd_long_rise, &w.kbd_short_rise) } else { (&w.long_rise, &w.short_rise) };
        let sample_scale = 1.0 / 32768.0;
        let (z, y) = (&mut self.z, &mut self.y);
        if info.window_sequence == EIGHT_SHORT {
            z.fill(0.0);
            for j in 0..8 {
                self.short.run(&ch.coefs[j * 128..(j + 1) * 128], &mut y[..256], sample_scale / 128.0);
                let rise = if j == 0 { prev_short } else { short_rise };
                let at = 448 + 128 * j;
                for i in 0..128 {
                    z[at + i] += y[i] * rise[i];
                    z[at + 128 + i] += y[128 + i] * short_fall[i];
                }
            }
        } else {
            self.long.run(&ch.coefs, y, sample_scale / 1024.0);
            // Left half: the previous frame's slope, or after a stop window zeros, a short slope and ones.
            if info.window_sequence == LONG_STOP {
                z[..448].fill(0.0);
                for i in 0..128 {
                    z[448 + i] = y[448 + i] * prev_short[i];
                }
                z[576..1024].copy_from_slice(&y[576..1024]);
            } else {
                for i in 0..1024 {
                    z[i] = y[i] * prev_long[i];
                }
            }
            // Right half: this frame's slope, or before a start window ones, a short slope and zeros.
            if info.window_sequence == LONG_START {
                z[1024..1472].copy_from_slice(&y[1024..1472]);
                for i in 0..128 {
                    z[1472 + i] = y[1472 + i] * short_fall[i];
                }
                z[1600..2048].fill(0.0);
            } else {
                for i in 0..1024 {
                    z[1024 + i] = y[1024 + i] * long_fall[i];
                }
            }
        }
        for i in 0..1024 {
            out[i] = state.saved[i] + z[i];
        }
        state.saved.copy_from_slice(&z[1024..2048]);
        state.prev_kbd = info.use_kbd;
    }

    fn mid_side(a: &mut Channel, b: &mut Channel, mask: &[bool]) {
        let info = &a.info;
        for g in 0..info.num_window_groups {
            let first_window = info.group_len[..g].iter().sum::<usize>();
            for sfb in 0..info.max_sfb {
                let idx = g * info.max_sfb + sfb;
                if mask[idx] && a.band_type[idx] < NOISE_BT && b.band_type[idx] < NOISE_BT {
                    for w in 0..info.group_len[g] {
                        let base = (first_window + w) * 128;
                        for k in info.swb[sfb] as usize..info.swb[sfb + 1] as usize {
                            let (l, r) = (a.coefs[base + k], b.coefs[base + k]);
                            a.coefs[base + k] = l + r;
                            b.coefs[base + k] = l - r;
                        }
                    }
                }
            }
        }
    }

    fn intensity(a: &Channel, b: &mut Channel, ms_present: bool, mask: &[bool]) {
        let info = &b.info;
        for g in 0..info.num_window_groups {
            let first_window = info.group_len[..g].iter().sum::<usize>();
            for sfb in 0..info.max_sfb {
                let idx = g * info.max_sfb + sfb;
                let bt = b.band_type[idx];
                if bt == INTENSITY_BT || bt == INTENSITY_BT2 {
                    let mut c = -1.0 + 2.0 * (bt as f64 - 14.0);
                    if ms_present && mask[idx] {
                        c = -c;
                    }
                    let scale = (c * (-b.sfo[idx] as f64 * 0.25).exp2()) as f32;
                    for w in 0..info.group_len[g] {
                        let base = (first_window + w) * 128;
                        for k in info.swb[sfb] as usize..info.swb[sfb + 1] as usize {
                            b.coefs[base + k] = a.coefs[base + k] * scale;
                        }
                    }
                }
            }
        }
    }

    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<Vec<f32>>> {
        let n_out = self.cfg.channels as usize;
        let mut out = vec![vec![0f32; 1024]; n_out];
        let mut br = Bits::new(data);
        let mut next = 0usize; // channels decoded so far
        loop {
            let id = br.read(3);
            match id {
                0 | 3 => {
                    br.read(4);
                    let mut ch = self.ics(&mut br, None)?;
                    self.finish_channel(&mut ch, &mut next, &mut out)?;
                }
                1 => {
                    br.read(4);
                    let common = br.flag();
                    let (mut a, mut b);
                    if common {
                        let info = self.ics_info(&mut br)?;
                        let ms = br.read(2);
                        if ms == 3 {
                            return Err(bad("reserved mid/side mode"));
                        }
                        let n = info.num_window_groups * info.max_sfb;
                        let mask: Vec<bool> = match ms {
                            1 => (0..n).map(|_| br.flag()).collect(),
                            2 => vec![true; n],
                            _ => vec![false; n],
                        };
                        a = self.ics(&mut br, Some(&info))?;
                        b = self.ics(&mut br, Some(&info))?;
                        if ms != 0 {
                            Self::mid_side(&mut a, &mut b, &mask);
                        }
                        Self::intensity(&a, &mut b, ms != 0, &mask);
                    } else {
                        a = self.ics(&mut br, None)?;
                        b = self.ics(&mut br, None)?;
                    }
                    self.finish_channel(&mut a, &mut next, &mut out)?;
                    self.finish_channel(&mut b, &mut next, &mut out)?;
                }
                2 => return Err(unsupported("coupling channel elements")),
                4 => {
                    br.read(4);
                    let align = br.flag();
                    let mut cnt = br.read(8) as usize;
                    if cnt == 255 {
                        cnt += br.read(8) as usize;
                    }
                    if align {
                        br.align();
                    }
                    br.skip(cnt as u32 * 8);
                }
                5 => {
                    br.read(10); // tag, object type, sampling index
                    let front = br.read(4);
                    let side = br.read(4);
                    let back = br.read(4);
                    let lfe = br.read(2);
                    let assoc = br.read(3);
                    let cc = br.read(4);
                    if br.flag() {
                        br.skip(4);
                    }
                    if br.flag() {
                        br.skip(4);
                    }
                    if br.flag() {
                        br.skip(3);
                    }
                    br.skip(5 * (front + side + back) + 4 * lfe + 4 * assoc + 5 * cc);
                    br.align();
                    let comment = br.read(8);
                    br.skip(comment * 8);
                }
                6 => {
                    let mut cnt = br.read(4) as usize;
                    if cnt == 15 {
                        cnt += br.read(8) as usize - 1;
                    }
                    if cnt > 0 && matches!(br.peek(4), 0xD | 0xE) {
                        return Err(unsupported("HE-AAC (SBR) is not decoded"));
                    }
                    br.skip(cnt as u32 * 8);
                }
                _ => break,
            }
            br.check()?;
        }
        Ok(out)
    }

    fn finish_channel(&mut self, ch: &mut Channel, next: &mut usize, out: &mut [Vec<f32>]) -> Result<()> {
        let i = *next;
        if i >= self.cfg.map.len() {
            return Err(bad("more channels in the frame than the AudioSpecificConfig declares"));
        }
        *next += 1;
        if ch.tns.present {
            Self::apply_tns(ch);
        }
        let mut state = std::mem::replace(&mut self.state[i], ChannelState { saved: Vec::new(), prev_kbd: false });
        self.synthesize(ch, &mut state, &mut out[self.cfg.map[i]]);
        self.state[i] = state;
        Ok(())
    }
}

impl Decoder for AacDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let planes = self.decode_frame(&packet.data)?;
        let channels = planes.len();
        let mut samples = vec![0f64; 1024 * channels];
        for (c, plane) in planes.iter().enumerate() {
            for (i, &s) in plane.iter().enumerate() {
                samples[i * channels + c] = s as f64;
            }
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.cfg.sample_rate, channels: self.cfg.channels, pts: packet.pts, samples })])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codebook_is_a_complete_prefix_code() {
        for i in 0..11 {
            let kraft: f64 = SPECTRAL_BITS[i].iter().map(|&b| 0.5f64.powi(b as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-9, "spectral book {} has Kraft sum {kraft}", i + 1);
        }
        let kraft: f64 = SCALEFACTOR_BITS.iter().map(|&b| 0.5f64.powi(b as i32)).sum();
        assert!((kraft - 1.0).abs() < 1e-9, "scalefactor book has Kraft sum {kraft}");
    }

    #[test]
    fn kbd_window_is_power_complementary() {
        let w = windows();
        for rise in [&w.kbd_long_rise, &w.kbd_short_rise, &w.long_rise, &w.short_rise] {
            let n = rise.len();
            for i in 0..n {
                let s = rise[i] * rise[i] + rise[n - 1 - i] * rise[n - 1 - i];
                assert!((s - 1.0).abs() < 1e-4, "window of {n} at {i}: {s}");
            }
        }
    }

    #[test]
    fn parses_a_common_config() {
        let cfg = parse_asc(&[0x12, 0x10], 44100, 2).unwrap();
        assert_eq!((cfg.sfi, cfg.channels, cfg.sample_rate), (4, 2, 44100));
        assert!(parse_asc(&[0x2b, 0x11, 0x88, 0x00], 0, 0).is_err(), "SBR object type is refused");
    }
}
