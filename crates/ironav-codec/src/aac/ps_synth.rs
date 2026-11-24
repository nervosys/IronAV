//! Test-only: writes synthetic HE-AAC v2 streams, and checks that this decoder and FFmpeg's decode
//! them to the same samples. No encoder FFmpeg is built with writes parametric stereo (libfdk_aac is
//! the one that does), so the streams are made here: a mono AAC-LC core from this crate's encoder
//! (a tone with noise bursts), an SBR extension in a fill element (a header and one frequency-coded
//! envelope set per frame, random but valid envelope and noise values), and in its extension data a
//! `ps_data()` with random but valid parameters: every IID and ICC mode, fine and coarse IID
//! quantisation, envelopes of both frame classes (fixed and variable borders, with and without the
//! last envelope reaching the end of the frame), time- and frequency-differential coding across
//! frames, IPD and OPD, switches between the 20- and 34-band layouts, and frames without `ps_data`.
//! The mantissas of the core are real; what is random is the side information, which is what the
//! parametric stereo decoder has to get right. The stream goes to FFmpeg in a small MP4 (so that the
//! AudioSpecificConfig carries the signalling explicitly) and is decoded to f32 samples. Skipped
//! without `ffmpeg` on PATH.

use super::ps_tables::{HUFF, HUFF_OFFSETS, HUFF_SIZES};
use super::sbr_tables::{HUFF_COUNTS as SBR_COUNTS, HUFF_LENS as SBR_LENS, HUFF_OFFSETS as SBR_OFFS, HUFF_SYMBOLS as SBR_SYMS};
use super::{output_params, sbr::Sbr, AacDecoder};
use crate::aac::enc::AacEncoder;
use ironav_core::{AudioFrame, Decoder, Encoder, Frame, Packet, StreamParams};
use std::process::Command;

struct Bw {
    bits: Vec<bool>,
}

impl Bw {
    fn new() -> Self {
        Bw { bits: Vec::new() }
    }
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push((v >> i) & 1 != 0);
        }
    }
    fn flag(&mut self, b: bool) {
        self.bits.push(b);
    }
    fn code(&mut self, (code, len): (u32, u32)) {
        self.put(code, len);
    }
    fn bytes(&self) -> Vec<u8> {
        self.bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &b)| a | (b as u8) << (7 - i))).collect()
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as u32) as i32
    }
    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
}

/// The code (right-aligned, with its length) of `value` in a table given as `(symbol, length)` pairs
/// whose codes are the running sum of 2^-length.
fn code_of(table: impl Iterator<Item = (i32, u32)>, value: i32) -> (u32, u32) {
    let mut code = 0u64;
    for (sym, len) in table {
        if sym == value {
            return ((code >> (32 - len)) as u32, len);
        }
        code += 1u64 << (32 - len);
    }
    panic!("no code for {value}");
}

fn sbr_code(table: usize, value: i32) -> (u32, u32) {
    let at: usize = SBR_COUNTS[..table].iter().sum();
    code_of((0..SBR_COUNTS[table]).map(|i| (SBR_SYMS[at + i] as i32 + SBR_OFFS[table], SBR_LENS[at + i] as u32)), value)
}

fn ps_code(table: usize, value: i32) -> (u32, u32) {
    let at: usize = HUFF_SIZES[..table].iter().sum();
    code_of(HUFF[at..at + HUFF_SIZES[table]].iter().map(|&(s, l)| (s as i32 + HUFF_OFFSETS[table], l as u32)), value)
}

// ---- the SBR side ----------------------------------------------------------------------------------

/// The header fields that fix the band layout; amplitude resolution 1.5 dB, default scale and limiter.
struct SbrCfg {
    start_freq: u32,
    stop_freq: u32,
    xover: u32,
}

impl SbrCfg {
    fn header(&self) -> Vec<u8> {
        Bw { bits: self.header_bits() }.bytes()
    }

    fn header_bits(&self) -> Vec<bool> {
        let mut w = Bw::new();
        w.put(0, 1); // bs_amp_res
        w.put(self.start_freq, 4);
        w.put(self.stop_freq, 4);
        w.put(self.xover, 3);
        w.put(0, 2);
        w.put(0, 2); // no header_extra_1 or _2
        w.bits
    }

    /// `(n, n_q)` of the band layout at the core rate `core_rate`.
    fn bands(&self, core_rate: u32) -> Option<([i32; 2], i32)> {
        Sbr::new(core_rate).header_bands(&self.header())
    }
}

/// A random walk of Huffman-coded differences in `lo..=hi`, starting from a raw value `first` that the
/// caller has written; returns the codes of the differences.
fn walk(rng: &mut Rng, n: usize, first: i32, lo: i32, hi: i32, table: usize) -> Vec<(u32, u32)> {
    let mut v = first;
    let mut out = Vec::new();
    for _ in 1..n {
        let d = rng.range(-3, 3);
        let next = (v + d).clamp(lo, hi);
        out.push(sbr_code(table, next - v));
        v = next;
    }
    out
}

/// The body of `sbr_single_channel_element()` (after the header), with `ps` as extension data if given.
fn sbr_single(w: &mut Bw, n: [i32; 2], n_q: i32, rng: &mut Rng, ps: Option<&Bw>) {
    w.flag(false); // bs_data_extra
    // fixed-fixed grid with 1, 2 or 4 envelopes
    let log_env = rng.below(3);
    let num_env = 1usize << log_env;
    let freq_res = rng.below(2) as usize;
    w.put(0, 2);
    w.put(log_env, 2);
    w.put(freq_res as u32, 1);
    let num_noise = if num_env > 1 { 2 } else { 1 };
    for _ in 0..num_env {
        w.flag(false); // frequency-differential envelopes
    }
    for _ in 0..num_noise {
        w.flag(false);
    }
    for _ in 0..n_q {
        w.put(rng.below(4), 2);
    }
    // envelopes: 7 bits of the first value (1.5 dB steps), then differences (table F_ENV_1_5DB = 1)
    for _ in 0..num_env {
        let first = rng.range(28, 40);
        w.put(first as u32, 7);
        for c in walk(rng, n[freq_res] as usize, first, 20, 46, 1) {
            w.code(c);
        }
    }
    // noise floors: 5 bits, then differences in table F_ENV_3_0DB = 5
    for _ in 0..num_noise {
        let first = rng.range(6, 14);
        w.put(first as u32, 5);
        for c in walk(rng, n_q as usize, first, 2, 20, 5) {
            w.code(c);
        }
    }
    w.flag(false); // no added harmonics
    match ps {
        None => w.flag(false),
        Some(ps) => {
            w.flag(true);
            let bytes = (2 + ps.bits.len()).div_ceil(8);
            if bytes < 15 {
                w.put(bytes as u32, 4);
            } else {
                w.put(15, 4);
                w.put(bytes as u32 - 15, 8);
            }
            w.put(2, 2); // EXTENSION_ID_PS
            w.bits.extend(&ps.bits);
            let used = 2 + ps.bits.len();
            for _ in used..bytes * 8 {
                w.flag(false);
            }
        }
    }
}

// ---- the PS side -------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct PsCfg {
    iid_mode: Option<u32>,
    icc_mode: Option<u32>,
    ipdopd: bool,
}

/// What the decoder remembers between frames, as far as the coding of the next frame depends on it.
struct PsState {
    /// Parameters of the last envelope of the previous frame, by kind (IID, ICC, IPD, OPD).
    last: [[i32; 34]; 4],
    have_prev: bool,
}

impl Default for PsState {
    fn default() -> Self {
        PsState { last: [[0; 34]; 4], have_prev: false }
    }
}

fn par_bands(mode: u32) -> usize {
    [10, 20, 34][(mode % 3) as usize]
}

fn ipdopd_bands(mode: u32) -> usize {
    [5, 11, 17][(mode % 3) as usize]
}

/// One envelope of one kind of parameters (0 IID, 1 ICC, 2 IPD, 3 OPD): differential in time or in
/// frequency. Returns the values; `hi` is the largest IID magnitude.
fn ps_envelope(w: &mut Bw, rng: &mut Rng, kind: usize, n: usize, prev: &[i32; 34], dt_allowed: bool, quant: bool, hi: i32) -> [i32; 34] {
    let dt = dt_allowed && rng.chance(50);
    w.flag(dt);
    let (table, lo, hi, dmax) = match kind {
        0 => (
            match (dt, quant) {
                (false, false) => 2,
                (false, true) => 0,
                (true, false) => 3,
                (true, true) => 1,
            },
            -hi,
            hi,
            if quant { 20 } else { 14 },
        ),
        1 => (if dt { 5 } else { 4 }, 0, 7, 7),
        2 => (if dt { 7 } else { 6 }, 0, 7, 7),
        _ => (if dt { 9 } else { 8 }, 0, 7, 7),
    };
    let mut out = [0i32; 34];
    let mut run = 0;
    for b in 0..n {
        let base = if dt { prev[b] } else { run };
        let near = if dt { prev[b] } else if b > 0 { out[b - 1] } else { 0 };
        let target = if kind >= 2 {
            rng.range(0, 7)
        } else if rng.chance(15) {
            rng.range(lo, hi)
        } else {
            (near + rng.range(-2, 2)).clamp(lo, hi)
        };
        let (delta, value) = if kind >= 2 {
            let d = (target - base).rem_euclid(8);
            (d, (base + d) & 7)
        } else {
            let d = (target - base).clamp(-dmax, dmax);
            (d, base + d)
        };
        w.code(ps_code(table, delta));
        out[b] = value;
        run = value;
    }
    out
}

/// Write one `ps_data()`. `header` says whether it carries the header (always when `st` has nothing yet).
fn ps_data(cfg: PsCfg, st: &mut PsState, header: bool, rng: &mut Rng) -> Bw {
    let mut w = Bw::new();
    w.flag(header);
    if header {
        w.flag(cfg.iid_mode.is_some());
        if let Some(m) = cfg.iid_mode {
            w.put(m, 3);
        }
        w.flag(cfg.icc_mode.is_some());
        if let Some(m) = cfg.icc_mode {
            w.put(m, 3);
        }
        w.flag(cfg.ipdopd); // enable_ext
    }
    let class = rng.chance(40);
    w.flag(class);
    let (code, num_env) = if class {
        let c = rng.below(4);
        (c, c as usize + 1)
    } else {
        let c = rng.below(4);
        (c, [0usize, 1, 2, 4][c as usize])
    };
    w.put(code, 2);
    if class {
        let mut b: Vec<u32> = (0..num_env).map(|_| rng.below(32)).collect();
        b.sort_unstable();
        if rng.chance(50) {
            *b.last_mut().unwrap() = 31;
        }
        for v in b {
            w.put(v, 5);
        }
    }
    let quant = cfg.iid_mode.is_some_and(|m| m > 2);
    let dt_allowed = st.have_prev;
    let mut last = st.last;
    let mut all: [Vec<[i32; 34]>; 4] = Default::default();
    let mut ipd_on = false;
    if let Some(m) = cfg.iid_mode {
        for e in 0..num_env {
            let prev = if e > 0 { all[0][e - 1] } else { st.last[0] };
            let v = ps_envelope(&mut w, rng, 0, par_bands(m), &prev, dt_allowed || e > 0, quant, if quant { 15 } else { 7 });
            all[0].push(v);
        }
    }
    if let Some(m) = cfg.icc_mode {
        for e in 0..num_env {
            let prev = if e > 0 { all[1][e - 1] } else { st.last[1] };
            let v = ps_envelope(&mut w, rng, 1, par_bands(m), &prev, dt_allowed || e > 0, false, 7);
            all[1].push(v);
        }
    }
    if cfg.ipdopd {
        // the extension: its size in bytes, then id 0, the IPD / OPD enable flag and the data
        let mut x = Bw::new();
        x.put(0, 2);
        let on = rng.chance(85);
        ipd_on = on;
        x.flag(on);
        if on {
            let bands = ipdopd_bands(cfg.iid_mode.unwrap_or(0));
            for e in 0..num_env {
                let prev = if e > 0 { all[2][e - 1] } else { st.last[2] };
                all[2].push(ps_envelope(&mut x, rng, 2, bands, &prev, dt_allowed || e > 0, false, 7));
                let prev = if e > 0 { all[3][e - 1] } else { st.last[3] };
                all[3].push(ps_envelope(&mut x, rng, 3, bands, &prev, dt_allowed || e > 0, false, 7));
            }
        }
        x.flag(false); // reserved_ps
        let bytes = x.bits.len().div_ceil(8);
        if bytes < 15 {
            w.put(bytes as u32, 4);
        } else {
            w.put(15, 4);
            w.put(bytes as u32 - 15, 8);
        }
        w.bits.extend(&x.bits);
        for _ in x.bits.len()..bytes * 8 {
            w.flag(false);
        }
    }
    // what the next frame's time-differential coding starts from: this frame's last envelope, or zeros
    // for a kind that is not coded here
    let coded = [cfg.iid_mode.is_some(), cfg.icc_mode.is_some(), ipd_on, ipd_on];
    for k in 0..4 {
        if !coded[k] {
            last[k] = [0; 34];
        } else if let Some(l) = all[k].last() {
            last[k] = *l;
        }
    }
    st.last = last;
    st.have_prev = true;
    w
}

// ---- assembling frames ---------------------------------------------------------------------------------

/// The core's packet with a fill element carrying `sbr` (the bits of the SBR data, after the header
/// flag... which `payload` includes) inserted before the end marker.
fn with_fill(core: &[u8], payload: &Bw) -> Vec<u8> {
    let mut bits: Vec<bool> = core.iter().flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1 != 0)).collect();
    // the end element is the last three set bits
    let last = bits.iter().rposition(|&b| b).expect("a core frame ends in the end marker");
    bits.truncate(last - 2);
    let mut w = Bw { bits };
    let cnt = (4 + payload.bits.len()).div_ceil(8);
    w.put(6, 3);
    if cnt < 15 {
        w.put(cnt as u32, 4);
    } else {
        w.put(15, 4);
        w.put(cnt as u32 - 14, 8);
    }
    w.put(0xD, 4); // SBR extension data without a CRC
    w.bits.extend(&payload.bits);
    for _ in 4 + payload.bits.len()..cnt * 8 {
        w.flag(false);
    }
    w.put(7, 3);
    w.bytes()
}

/// A mono AAC-LC stream at `rate` (a tone with bursts of noise), as packets.
fn core_packets(rate: u32, frames: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = Rng(seed);
    let mut enc = AacEncoder::new(rate, 1, 48000).unwrap();
    let n = (frames + 2) * 1024;
    let samples: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let burst = if (i / 3000) % 3 == 1 { (rng.below(2000) as f64 / 1000.0 - 1.0) * 0.15 } else { 0.0 };
            0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.2 * (2.0 * std::f64::consts::PI * 1750.0 * t).sin() * (0.5 + 0.5 * (t * 3.0).sin()) + burst
        })
        .collect();
    let mut packets = enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: 1, pts: 0, samples })).unwrap();
    packets.extend(enc.flush().unwrap());
    packets.into_iter().skip(1).take(frames).map(|p| p.data).collect()
}

/// One step of a test stream: the PS configuration from this frame on, and whether the frame carries
/// a header, or no `ps_data` at all.
#[derive(Clone, Copy)]
enum Step {
    Cfg(PsCfg),
    Data,
    NoData,
    Header,
}

struct Stream {
    asc: Vec<u8>,
    packets: Vec<Vec<u8>>,
    rate: u32,
}

fn explicit_asc(core_rate: u32, out_rate: u32) -> Vec<u8> {
    let sfi = |r: u32| super::SAMPLE_RATES.iter().position(|&x| x == r).unwrap() as u32;
    let mut w = Bw::new();
    w.put(29, 5);
    w.put(sfi(core_rate), 4);
    w.put(1, 4);
    w.put(sfi(out_rate), 4);
    w.put(2, 5);
    w.put(0, 3);
    w.bytes()
}

/// The backward-compatible signalling: AAC-LC, then the sync extension with SBR and PS.
fn compatible_asc(core_rate: u32, out_rate: u32) -> Vec<u8> {
    let sfi = |r: u32| super::SAMPLE_RATES.iter().position(|&x| x == r).unwrap() as u32;
    let mut w = Bw::new();
    w.put(2, 5);
    w.put(sfi(core_rate), 4);
    w.put(1, 4);
    w.put(0, 3);
    w.put(0x2b7, 11);
    w.put(5, 5);
    w.flag(true);
    w.put(sfi(out_rate), 4);
    w.put(0x548, 11);
    w.flag(true);
    w.bytes()
}

fn make_stream(core_rate: u32, sbr: &SbrCfg, steps: &[Step], seed: u64, asc: fn(u32, u32) -> Vec<u8>) -> Stream {
    let core = core_packets(core_rate, steps.len(), seed);
    assert_eq!(core.len(), steps.len(), "core frames");
    let (n, n_q) = sbr.bands(core_rate).expect("a valid band layout");
    let mut rng = Rng(seed ^ 0xA5A5_5A5A_1234_5678);
    let mut cfg = PsCfg { iid_mode: Some(1), icc_mode: Some(1), ipdopd: false };
    let mut st = PsState::default();
    let mut packets = Vec::new();
    let mut header_due = true;
    for (i, (c, step)) in core.iter().zip(steps).enumerate() {
        let (ps, header) = match *step {
            Step::Cfg(new) => {
                cfg = new;
                st = PsState::default();
                (true, true)
            }
            Step::Header => (true, true),
            Step::Data => (true, false),
            Step::NoData => (false, false),
        };
        let header = header || (header_due && ps);
        let mut w = Bw::new();
        w.flag(true); // the SBR header goes in every frame
        w.bits.extend(sbr.header_bits());
        let mut body = Bw::new();
        let mut frame_rng = Rng(seed.wrapping_add(i as u64 * 7919 + 1));
        let data = ps.then(|| {
            let d = ps_data(cfg, &mut st, header, &mut rng);
            header_due = false;
            d
        });
        sbr_single(&mut body, n, n_q, &mut frame_rng, data.as_ref());
        w.bits.extend(body.bits);
        packets.push(with_fill(c, &w));
    }
    Stream { asc: asc(core_rate, core_rate * 2), packets, rate: core_rate * 2 }
}

// ---- decoding and comparing ---------------------------------------------------------------------------

fn decode_ours(s: &Stream) -> Vec<f32> {
    let mut dec = AacDecoder::new(&s.asc, &StreamParams::Audio { sample_rate: s.rate, channels: 0 }).unwrap();
    let mut out = Vec::new();
    for (i, p) in s.packets.iter().enumerate() {
        for f in dec.decode(&Packet { stream: 0, pts: i as i64 * 1024, duration: 1024, data: p.clone() }).unwrap_or_else(|e| panic!("frame {i}: {e}")) {
            let Frame::Audio(a) = f else { unreachable!() };
            assert_eq!((a.sample_rate, a.channels), (s.rate, 2), "frame {i}");
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    }
    out
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = be32(body.len() as u32 + 8).to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

/// A minimal MP4 holding the AAC packets, with the AudioSpecificConfig in its `esds`.
fn mp4(s: &Stream) -> Vec<u8> {
    let timescale = s.rate / 2;
    let n = s.packets.len() as u32;
    let mut esds = vec![0u8; 4];
    let dsi = [&[5u8, s.asc.len() as u8][..], &s.asc].concat();
    let dcd_len = 13 + dsi.len();
    esds.extend([3, (3 + 2 + dcd_len + 3) as u8, 0, 1, 0, 4, dcd_len as u8, 0x40, 0x15, 0, 0, 0]);
    esds.extend(be32(0));
    esds.extend(be32(0));
    esds.extend(&dsi);
    esds.extend([6, 1, 2]);
    let mut mp4a = vec![0u8; 6];
    mp4a.extend([0, 1]);
    mp4a.extend([0u8; 8]);
    mp4a.extend([0, 2, 0, 16, 0, 0, 0, 0]);
    mp4a.extend(be32(timescale << 16));
    mp4a.extend(boxed(b"esds", &esds));
    let mut stsd = vec![0, 0, 0, 0];
    stsd.extend(be32(1));
    stsd.extend(boxed(b"mp4a", &mp4a));
    let mut stts = vec![0u8; 4];
    stts.extend(be32(1));
    stts.extend(be32(n));
    stts.extend(be32(1024));
    let mut stsc = vec![0u8; 4];
    stsc.extend(be32(1));
    stsc.extend([be32(1), be32(n), be32(1)].concat());
    let mut stsz = vec![0u8; 4];
    stsz.extend(be32(0));
    stsz.extend(be32(n));
    for p in &s.packets {
        stsz.extend(be32(p.len() as u32));
    }
    let ftyp = boxed(b"ftyp", &[b"mp42".as_slice(), &be32(0), b"mp42isom"].concat());
    let mdat_at = ftyp.len() as u32 + 8;
    // chunk offset: patched once the moov size is known; moov goes after the mdat
    let mut mdat_body = Vec::new();
    for p in &s.packets {
        mdat_body.extend(p);
    }
    let mut stco = vec![0u8; 4];
    stco.extend(be32(1));
    stco.extend(be32(mdat_at));
    let stbl = boxed(b"stbl", &[boxed(b"stsd", &stsd), boxed(b"stts", &stts), boxed(b"stsc", &stsc), boxed(b"stsz", &stsz), boxed(b"stco", &stco)].concat());
    let url = boxed(b"url ", &[0, 0, 0, 1]);
    let dref = boxed(b"dref", &[&[0u8, 0, 0, 0][..], &be32(1), &url].concat());
    let dinf = boxed(b"dinf", &dref);
    let smhd = boxed(b"smhd", &[0u8; 8]);
    let minf = boxed(b"minf", &[smhd, dinf, stbl].concat());
    let mut hdlr = vec![0u8; 8];
    hdlr.extend(b"soun");
    hdlr.extend([0u8; 12]);
    hdlr.push(0);
    let hdlr = boxed(b"hdlr", &hdlr);
    let mut mdhd = vec![0u8; 12];
    mdhd.extend(be32(timescale));
    mdhd.extend(be32(n * 1024));
    mdhd.extend([0x55, 0xc4, 0, 0]);
    let mdhd = boxed(b"mdhd", &mdhd);
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    let mut tkhd = vec![0, 0, 0, 3];
    tkhd.extend([0u8; 8]);
    tkhd.extend(be32(1));
    tkhd.extend(be32(0));
    tkhd.extend(be32(n * 1024));
    tkhd.extend([0u8; 8]);
    tkhd.extend([0, 0, 0, 0, 1, 0, 0, 0]);
    for m in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000] {
        tkhd.extend(be32(m));
    }
    tkhd.extend([0u8; 8]);
    let trak = boxed(b"trak", &[boxed(b"tkhd", &tkhd), mdia].concat());
    let mut mvhd = vec![0u8; 12];
    mvhd.extend(be32(timescale));
    mvhd.extend(be32(n * 1024));
    mvhd.extend(be32(0x10000));
    mvhd.extend([1, 0, 0, 0]);
    mvhd.extend([0u8; 8]);
    for m in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000] {
        mvhd.extend(be32(m));
    }
    mvhd.extend([0u8; 24]);
    mvhd.extend(be32(2));
    let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), trak].concat());
    [ftyp, boxed(b"mdat", &mdat_body), moov].concat()
}

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// FFmpeg's decode of the stream as interleaved f32 stereo, or `None` if it did not decode it.
fn decode_ffmpeg(name: &str, s: &Stream) -> Option<Vec<f32>> {
    let dir = std::env::temp_dir().join(format!("ironav-ps-synth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (input, output) = (dir.join(format!("{name}.mp4")), dir.join(format!("{name}.f32")));
    std::fs::write(&input, mp4(s)).unwrap();
    let ok = Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(&input).args(["-f", "f32le", "-acodec", "pcm_f32le"]).arg(&output).status().is_ok_and(|st| st.success());
    let res = ok.then(|| std::fs::read(&output).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
    let _ = std::fs::remove_dir_all(&dir);
    res
}

/// SNR (dB) of `got` against `want` over the common length, per channel of an interleaved stereo pair.
fn snr(got: &[f32], want: &[f32]) -> [f64; 2] {
    let mut out = [0.0; 2];
    for (c, o) in out.iter_mut().enumerate() {
        let (mut sig, mut noise) = (0f64, 0f64);
        for (a, b) in got.iter().skip(c).step_by(2).zip(want.iter().skip(c).step_by(2)) {
            sig += (*b as f64).powi(2);
            noise += ((a - b) as f64).powi(2);
        }
        *o = 10.0 * (sig / noise.max(1e-300)).log10();
    }
    out
}

fn sbr_cfg(core_rate: u32) -> SbrCfg {
    for (start_freq, stop_freq) in [(5, 9), (4, 8), (6, 10), (3, 7), (7, 11), (2, 6)] {
        let c = SbrCfg { start_freq, stop_freq, xover: 0 };
        if c.bands(core_rate).is_some() {
            return c;
        }
    }
    panic!("no valid SBR band layout at {core_rate} Hz");
}

fn cfg(iid: Option<u32>, icc: Option<u32>, ipdopd: bool) -> Step {
    Step::Cfg(PsCfg { iid_mode: iid, icc_mode: icc, ipdopd })
}

/// `frames` frames of `first`, with the occasional header, silent frame and absent `ps_data`.
fn plain(first: Step, frames: usize) -> Vec<Step> {
    let mut v = vec![first];
    for i in 1..frames {
        v.push(if i % 11 == 0 { Step::Header } else if i % 13 == 5 { Step::NoData } else { Step::Data });
    }
    v
}

fn cases() -> Vec<(&'static str, u32, Vec<Step>)> {
    let mut v: Vec<(&'static str, u32, Vec<Step>)> = vec![
        ("iid10_icc10", 22050, plain(cfg(Some(0), Some(0), false), 30)),
        ("iid20_icc20", 22050, plain(cfg(Some(1), Some(1), false), 30)),
        ("iid34_icc34", 22050, plain(cfg(Some(2), Some(2), false), 30)),
        ("fine10_icc10", 22050, plain(cfg(Some(3), Some(0), false), 30)),
        ("fine20_icc20", 24000, plain(cfg(Some(4), Some(1), false), 30)),
        ("fine34_icc34", 22050, plain(cfg(Some(5), Some(2), false), 30)),
        ("rotation_b_icc10", 22050, plain(cfg(Some(1), Some(3), false), 30)),
        ("rotation_b_icc20", 22050, plain(cfg(Some(1), Some(4), false), 30)),
        ("rotation_b_icc34", 22050, plain(cfg(Some(2), Some(5), false), 30)),
        ("iid_only", 22050, plain(cfg(Some(1), None, false), 24)),
        ("icc_only", 22050, plain(cfg(None, Some(1), false), 24)),
        ("mixed_layouts", 22050, plain(cfg(Some(0), Some(2), false), 24)),
        ("mixed_layouts_2", 22050, plain(cfg(Some(2), Some(1), false), 24)),
        ("ipdopd10", 22050, plain(cfg(Some(0), Some(0), true), 30)),
        ("ipdopd20", 22050, plain(cfg(Some(1), Some(1), true), 30)),
        ("ipdopd34", 22050, plain(cfg(Some(2), Some(2), true), 30)),
        ("ipdopd_fine34_b", 24000, plain(cfg(Some(5), Some(4), true), 30)),
    ];
    // no ps_data at all: the SBR signal on both channels, which is the baseline the others add to
    v.push(("no_ps_data", 22050, (0..16).map(|_| Step::NoData).collect()));
    // a stream that switches between the 20- and 34-band layouts, and between IPD / OPD and none.
    // (A header that turns the extension off after it was on is left out: FFmpeg then goes on using
    // the old IPD and OPD values, and this decoder, as the standard has it, does not.)
    let mut sw = plain(cfg(Some(1), Some(1), false), 8);
    sw.extend(plain(cfg(Some(2), Some(2), false), 8));
    sw.extend(plain(cfg(Some(1), Some(0), true), 8));
    sw.extend(plain(cfg(Some(5), Some(3), true), 8));
    sw.extend(plain(cfg(Some(0), Some(1), true), 8));
    v.push(("switching", 22050, sw));
    v
}

#[test]
fn synthetic_streams_decode_like_ffmpeg() {
    if !ffmpeg_ok() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let mut failures = Vec::new();
    let mut worst = f64::MAX;
    for (i, (name, rate, steps)) in cases().into_iter().enumerate() {
        let stream = make_stream(rate, &sbr_cfg(rate), &steps, 0x9E37_79B9_7F4A_7C15 ^ ((i as u64 + 1) * 0x1000193), explicit_asc);
        assert_eq!(output_params(&stream.asc), Some((rate * 2, 2)), "{name}: the config says two channels up front");
        let got = decode_ours(&stream);
        let Some(want) = decode_ffmpeg(name, &stream) else {
            failures.push(format!("{name}: FFmpeg did not decode the stream"));
            continue;
        };
        if got.len() != want.len() {
            failures.push(format!("{name}: {} samples, FFmpeg has {}", got.len(), want.len()));
            continue;
        }
        let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
        let s = snr(&got, &want);
        // the two channels must differ, or the test is not testing the stereo processing
        let (mut diff, mut sum) = (0f64, 0f64);
        for p in want.chunks(2) {
            diff += ((p[0] - p[1]) as f64).powi(2);
            sum += (p[0] as f64).powi(2) + (p[1] as f64).powi(2);
        }
        eprintln!("{name:>18}: SNR L {:6.1} dB, R {:6.1} dB, peak {peak:.3}, side/total {:.3}", s[0], s[1], diff / sum.max(1e-30));
        worst = worst.min(s[0]).min(s[1]);
        if s[0] < 55.0 || s[1] < 55.0 {
            let per_frame: Vec<String> = got.chunks(4096).zip(want.chunks(4096)).map(|(g, w)| format!("{:.0}", snr(g, w)[0].min(snr(g, w)[1]))).collect();
            eprintln!("{name}: SNR per frame (dB): {}", per_frame.join(" "));
        }
        let stereo = name != "no_ps_data";
        if !(s[0] > 55.0 && s[1] > 55.0 && peak > 0.05 && (diff / sum > 0.01) == stereo) {
            failures.push(format!("{name}: SNR {:.1} / {:.1} dB, peak {peak}, side {:.4}", s[0], s[1], diff / sum.max(1e-30)));
        }
    }
    eprintln!("worst SNR against FFmpeg: {worst:.1} dB");
    assert!(failures.is_empty(), "differences from FFmpeg:\n{}", failures.join("\n"));
}

/// The stream's frames in ADTS (AAC-LC, mono, at the core's rate), as an internet radio would send them.
fn refs(f: &[Vec<u8>]) -> Vec<&[u8]> {
    f.iter().map(|v| v.as_slice()).collect()
}

fn adts(s: &Stream) -> Vec<u8> {
    let sfi = super::SAMPLE_RATES.iter().position(|&r| r == s.rate / 2).unwrap() as u8;
    let mut out = Vec::new();
    for p in &s.packets {
        let len = p.len() + 7;
        out.extend([0xFF, 0xF1, 1 << 6 | sfi << 2, 1 << 6 | (len >> 11) as u8 & 3, (len >> 3) as u8, (len as u8 & 7) << 5 | 0x1F, 0xFC]);
        out.extend(p);
    }
    out
}

#[test]
fn implicit_signalling_is_found_in_the_frames() {
    let lc = [0x13, 0x88]; // AAC-LC, 22.05 kHz, mono: all an ADTS header says
    let frames = |s: &Stream, n: usize| -> Vec<Vec<u8>> { s.packets.iter().take(n).cloned().collect() };
    // HE-AAC v2: found with a few frames, and the config says two channels
    let v2 = make_stream(22050, &sbr_cfg(22050), &plain(cfg(Some(1), Some(1), true), 20), 21, explicit_asc);
    let f = frames(&v2, 12);
    let found = super::implicit_config(&lc, &refs(&f)).expect("parametric stereo in the first frames");
    assert_eq!(found, compatible_asc(22050, 44100));
    assert_eq!(output_params(&found), Some((44100, 2)));
    // the same without ps_data is HE-AAC v1: mono
    let v1 = make_stream(22050, &sbr_cfg(22050), &(0..12).map(|_| Step::NoData).collect::<Vec<_>>(), 22, explicit_asc);
    let f = frames(&v1, 12);
    let found = super::implicit_config(&lc, &refs(&f)).expect("SBR in the first frames");
    assert_eq!(output_params(&found), Some((44100, 1)));
    // plain AAC-LC frames show nothing
    let core: Vec<Vec<u8>> = core_packets(22050, 12, 23);
    assert_eq!(super::implicit_config(&lc, &refs(&core)), None);
    // a config that already says SBR is left alone
    assert_eq!(super::implicit_config(&compatible_asc(22050, 44100), &refs(&frames(&v2, 12))), None);
    // and the stream decodes with what was found, to the same samples as with the explicit config
    let mut implicit = make_stream(22050, &sbr_cfg(22050), &plain(cfg(Some(1), Some(1), true), 20), 21, explicit_asc);
    implicit.asc = super::implicit_config(&lc, &refs(&frames(&implicit, 12))).unwrap();
    assert_eq!(decode_ours(&implicit), decode_ours(&v2));
}

/// Writes the fixtures of `ironav-formats`' tests: `cargo test -p ironav-codec --lib write_fixtures -- --ignored`
/// with `IRONAV_FIXTURE_DIR` set to the directory.
#[test]
#[ignore]
fn write_fixtures() {
    let dir = std::path::PathBuf::from(std::env::var("IRONAV_FIXTURE_DIR").expect("IRONAV_FIXTURE_DIR"));
    let s = make_stream(22050, &sbr_cfg(22050), &plain(cfg(Some(1), Some(1), true), 24), 21, explicit_asc);
    std::fs::write(dir.join("he-aac-v2-implicit.aac"), adts(&s)).unwrap();
    std::fs::write(dir.join("he-aac-v2-explicit.mp4"), mp4(&s)).unwrap();
}

#[test]
fn the_backward_compatible_signalling_decodes_the_same() {
    let steps = plain(cfg(Some(1), Some(1), true), 12);
    let a = make_stream(22050, &sbr_cfg(22050), &steps, 7, explicit_asc);
    let b = make_stream(22050, &sbr_cfg(22050), &steps, 7, compatible_asc);
    assert_eq!(output_params(&a.asc), output_params(&b.asc));
    assert_eq!(decode_ours(&a), decode_ours(&b));
}

#[test]
fn parametric_stereo_makes_a_real_stereo_image() {
    let steps = plain(cfg(Some(1), Some(1), false), 20);
    let s = make_stream(22050, &sbr_cfg(22050), &steps, 99, explicit_asc);
    let out = decode_ours(&s);
    assert_eq!(out.len(), 20 * 2048 * 2);
    assert!(out.iter().all(|x| x.is_finite()));
    let (mut diff, mut sum) = (0f64, 0f64);
    for p in out.chunks(2).skip(4096) {
        diff += ((p[0] - p[1]) as f64).powi(2);
        sum += (p[0] as f64).powi(2) + (p[1] as f64).powi(2);
    }
    assert!(sum > 1.0, "silent output");
    assert!(diff / sum > 0.005, "the channels are the same signal: side/total {}", diff / sum);
}

#[test]
fn without_ps_data_the_mono_signal_is_on_both_channels() {
    let steps: Vec<Step> = (0..8).map(|_| Step::NoData).collect();
    let s = make_stream(22050, &sbr_cfg(22050), &steps, 5, explicit_asc);
    let out = decode_ours(&s);
    assert!(out.chunks(2).all(|p| p[0] == p[1]));
    assert!(out.iter().any(|&x| x != 0.0));
}

/// Damaged streams give an error or output, and never panic.
#[test]
fn damaged_streams_do_not_panic() {
    let steps = plain(cfg(Some(2), Some(2), true), 10);
    let s = make_stream(22050, &sbr_cfg(22050), &steps, 3, explicit_asc);
    let params = StreamParams::Audio { sample_rate: 44100, channels: 2 };
    let mut rng = Rng(0xDEAD_BEEF);
    let mut decoded = 0;
    let mut refused = 0;
    for trial in 0..400 {
        let mut dec = AacDecoder::new(&s.asc, &params).unwrap();
        for (i, p) in s.packets.iter().enumerate() {
            let mut d = p.clone();
            match trial % 4 {
                // cut anywhere
                0 => d.truncate(rng.below(d.len() as u32 + 1) as usize),
                // flip bits in the tail, where the fill element is
                1 => {
                    for _ in 0..3 {
                        let at = d.len() - 1 - rng.below((d.len() as u32).min(40)) as usize;
                        d[at] ^= 1 << rng.below(8);
                    }
                }
                // flip bits anywhere
                2 => {
                    let at = rng.below(d.len() as u32) as usize;
                    d[at] ^= 1 << rng.below(8);
                }
                // zeros over the extension
                _ => {
                    let from = d.len() - 1 - rng.below((d.len() as u32).min(30)) as usize;
                    d[from..].fill(if rng.chance(50) { 0 } else { 0xFF });
                }
            }
            match dec.decode(&Packet { stream: 0, pts: i as i64 * 1024, duration: 1024, data: d }) {
                Ok(frames) => {
                    decoded += 1;
                    for f in frames {
                        let Frame::Audio(a) = f else { unreachable!() };
                        assert_eq!(a.channels, 2);
                        assert!(a.samples.iter().all(|x| x.is_finite()), "non-finite output after damage");
                    }
                }
                Err(_) => refused += 1,
            }
        }
    }
    eprintln!("{decoded} damaged frames decoded, {refused} refused");
    assert!(decoded > 0 && refused > 0);
}

#[test]
fn bad_parameter_data_falls_back_to_mono_on_both_channels() {
    let (n, n_q) = sbr_cfg(22050).bands(22050).unwrap();
    let core = core_packets(22050, 6, 11);
    let mut rng = Rng(77);
    let sbr = sbr_cfg(22050);
    let mut packets = Vec::new();
    for (i, c) in core.iter().enumerate() {
        let mut w = Bw::new();
        w.flag(true);
        w.bits.extend(sbr.header_bits());
        // a ps_data with a reserved iid_mode (6 and 7), or a border that goes backwards
        let mut ps = Bw::new();
        ps.flag(true);
        ps.flag(true);
        ps.put(if i % 2 == 0 { 6 } else { 7 }, 3);
        ps.flag(false);
        ps.flag(false);
        ps.flag(true);
        ps.put(1, 2);
        ps.put(20, 5);
        let mut body = Bw::new();
        sbr_single(&mut body, n, n_q, &mut rng, Some(&ps));
        w.bits.extend(body.bits);
        packets.push(with_fill(c, &w));
    }
    let s = Stream { asc: explicit_asc(22050, 44100), packets, rate: 44100 };
    let out = decode_ours(&s);
    assert_eq!(out.len(), 6 * 2048 * 2);
    assert!(out.chunks(2).all(|p| p[0] == p[1]), "mono on both channels");
    assert!(out.iter().any(|&x| x != 0.0));
}
