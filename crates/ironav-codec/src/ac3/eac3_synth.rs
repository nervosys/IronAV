//! Test-only: writes synthetic E-AC-3 streams that use the tools FFmpeg's encoder does not write
//! (spectral extension, the adaptive hybrid transform with every gain adaptive quantisation mode,
//! coupling together with spectral extension, transient pre-noise data, ...), and checks that this
//! decoder and FFmpeg's decode them to the same samples. The mantissas are random bits; what makes
//! the streams valid is that the writer follows the bit allocation (it uses this decoder's) to know
//! how many mantissa bits each bin has. Skipped without `ffmpeg` on PATH.

use super::alloc::{self, AllocParams, Dba};
use super::eac3_tables::*;
use super::tables::*;
use super::{Ac3Decoder, BitReader};
use ironav_core::{Decoder, Frame, Packet};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// How often the writer used vector quantisation, plain mantissas, GAQ gains above 1, and large mantissas.
static USED: [AtomicUsize; 4] = [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];

struct Bw {
    bits: Vec<bool>,
}

impl Bw {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(i < 32 && (v >> i) & 1 != 0);
        }
    }
    fn flag(&mut self, b: bool) {
        self.bits.push(b);
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
}

#[derive(Clone, Copy)]
struct Spx {
    dst: u32,
    begin: u32,
    end: u32,
    custom_bands: bool,
}

#[derive(Clone, Copy)]
struct Cfg {
    acmod: usize,
    lfe: bool,
    aht: bool,
    spx: Option<Spx>,
    /// The coupling start subband, and which full-bandwidth channels are coupled (a bit mask, bit 0
    /// for channel 1; all of them in stereo).
    cpl: Option<(u32, u32)>,
    transient: bool,
    atten: bool,
    /// Writes the enhanced coupling flag, which this decoder refuses.
    ecpl: bool,
}

const FBW: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

fn spx_bands(start_sub: usize, end_sub: usize, st: &[u8; 17]) -> Vec<usize> {
    let n = end_sub - start_sub;
    let mut sizes = vec![12];
    for sub in 1..n {
        if st[start_sub + sub] != 0 {
            *sizes.last_mut().unwrap() += 12;
        } else {
            sizes.push(12);
        }
    }
    sizes
}

/// One 6-block E-AC-3 independent frame of 48 kHz, as bytes.
fn frame(cfg: &Cfg, gaq_mode: u32, rng: &mut Rng, csnr_max: u32) -> Vec<u8> {
    let fbw = FBW[cfg.acmod];
    let channels = fbw + cfg.lfe as usize;
    let lfe_ch = fbw + 1;
    let mut w = Bw { bits: Vec::new() };
    w.put(0x0B77, 16);
    w.put(0, 2); // strmtyp
    w.put(0, 3); // substreamid
    w.put(0, 11); // frmsiz, patched below
    w.put(0, 2); // fscod 48 kHz
    w.put(3, 2); // numblkscod 6 blocks
    w.put(cfg.acmod as u32, 3);
    w.flag(cfg.lfe);
    w.put(16, 5); // bsid
    for _ in 0..if cfg.acmod == 0 { 2 } else { 1 } {
        w.put(20, 5); // dialnorm
        w.flag(false); // compre
    }
    w.flag(false); // mixmdate
    w.flag(false); // infomdate
    w.flag(false); // addbsie

    // the audio frame
    let cpl_blocks = cfg.cpl.is_some();
    w.flag(false); // expstre: the exponent strategy of each channel is a code for the frame
    w.flag(cfg.aht); // ahte
    w.put(0, 2); // snroffststr: one SNR offset for the frame
    w.flag(cfg.transient); // transproce
    w.flag(false); // blkswe
    w.flag(false); // dithflage: dither whenever a mantissa has no bits
    w.flag(false); // bamode: the default bit allocation parameters
    w.flag(false); // frmfgaincode
    w.flag(false); // dbaflde
    w.flag(false); // skipflde
    w.flag(cfg.atten); // spxattene
    if cfg.acmod > 1 {
        w.flag(cpl_blocks); // cplinu of block 0
        for _ in 1..6 {
            w.flag(false); // cplstre
        }
    }
    let first = !(cfg.acmod > 1 && cpl_blocks) as usize;
    for _ in first..=fbw {
        w.put(0, 5); // frmchexpstr 0: new exponents (D15) in block 0, reused after
    }
    if cfg.lfe {
        for blk in 0..6 {
            w.flag(blk == 0); // lfeexpstr
        }
    }
    w.put(0, 5 * fbw as u32); // convexpstr
    if cfg.aht {
        for _ in (!cpl_blocks) as usize..=channels {
            w.flag(true); // chahtinu
        }
    }
    let csnr = 12 + rng.below(csnr_max - 12 + 1);
    let fsnr = rng.below(16);
    w.put(csnr, 6);
    w.put(fsnr, 4);
    let snr_offset = (((csnr as i32) - 15) * 16 + fsnr as i32) << 2;
    if cfg.transient {
        for ch in 1..=fbw {
            let on = ch == 1;
            w.flag(on);
            if on {
                w.put(rng.below(1024), 10);
                w.put(rng.below(256), 8);
            }
        }
    }
    if cfg.atten {
        for ch in 1..=fbw {
            let on = ch % 2 == 1;
            w.flag(on);
            if on {
                w.put(rng.below(32), 5);
            }
        }
    }
    w.flag(false); // blkstrtinfoe

    // what the decoder derives from the blocks' side information
    let (spx_start_sub, spx_end_sub) = cfg
        .spx
        .map(|s| {
            let mut a = s.begin as usize + 2;
            if a > 7 {
                a += a - 7;
            }
            let mut b = s.end as usize + 5;
            if b > 7 {
                b += b - 7;
            }
            (a, b)
        })
        .unwrap_or((0, 0));
    let spx_src = spx_start_sub * 12 + 25;
    let cpl_start_sub = cfg.cpl.map_or(0, |c| c.0 as usize);
    let cpl_end_sub = if cfg.spx.is_some() { (spx_src - 37) / 12 } else { 8 };
    let cpl_range = (cpl_start_sub * 12 + 37, cpl_end_sub * 12 + 37);
    let in_cpl = |ch: usize| cfg.cpl.is_some_and(|c| cfg.acmod == 2 || (c.1 >> (ch - 1)) & 1 != 0);
    let uses_spx = |ch: usize| cfg.spx.is_some() && (!ch.is_multiple_of(3) || fbw == 1);
    let mut start_freq = [0usize; 7];
    let mut end_freq = [0usize; 7];
    let bw_code = 30 + rng.below(20) as usize;
    for ch in 1..=fbw {
        end_freq[ch] = if in_cpl(ch) {
            cpl_range.0
        } else if uses_spx(ch) {
            spx_src
        } else {
            bw_code * 3 + 73
        };
    }
    if cfg.lfe {
        end_freq[lfe_ch] = 7;
    }
    if cpl_blocks {
        start_freq[0] = cpl_range.0;
        end_freq[0] = cpl_range.1;
    }

    // the exponents, and the bit allocation they lead to
    let mut exps = vec![[0i8; 256]; 7];
    let mut abs = [0u32; 7];
    let ch_first = if cpl_blocks { 0 } else { 1 };
    for ch in ch_first..=channels {
        let (s, e) = (start_freq[ch], end_freq[ch]);
        let mut cur = 6 + rng.below(8) as i32;
        abs[ch] = if ch == 0 { (cur as u32) & !1 } else { cur as u32 };
        if ch == 0 {
            cur = abs[ch] as i32;
        }
        let base = if ch == 0 { s } else { s + 1 };
        if ch != 0 {
            exps[ch][0] = cur as i8;
        }
        for b in base..e {
            let d = rng.below(5) as i32 - 2;
            let n = (cur + d).clamp(0, 24);
            cur = n;
            exps[ch][b] = n as i8;
        }
    }
    let mut params = AllocParams { sr_code: 0, sr_shift: 0, slow_gain: SLOW_GAIN[1] as i32, slow_decay: SLOW_DECAY[2] as i32, fast_decay: FAST_DECAY[1] as i32, db_per_bit: DB_PER_BIT[2] as i32, floor: FLOOR[7] as i32, cpl_fast_leak: 0, cpl_slow_leak: 0 };
    let cpl_fast_leak = rng.below(8) as i32;
    let cpl_slow_leak = rng.below(8) as i32;
    params.cpl_fast_leak = cpl_fast_leak;
    params.cpl_slow_leak = cpl_slow_leak;
    let mut bap = vec![[0u8; 256]; 7];
    for ch in ch_first..=channels {
        let (s, e) = (start_freq[ch], end_freq[ch]);
        let mut psd = [0i16; 256];
        let mut band_psd = [0i16; 50];
        let mut mask = [0i16; 50];
        alloc::psd(&exps[ch], s, e, &mut psd, &mut band_psd);
        let (offsets, lengths, values) = ([0u8; 8], [0u8; 8], [0u8; 8]);
        let dba = Dba { mode: 2, nsegs: 0, offsets: &offsets, lengths: &lengths, values: &values };
        assert!(alloc::mask(&params, &band_psd, s, e, FAST_GAIN[4] as i32, ch == lfe_ch && cfg.lfe, &dba, &mut mask));
        let table = if cfg.aht { &HEBAP_TAB } else { &BAP_TAB };
        alloc::bap(&mask, &psd, s, e, snr_offset, FLOOR[7] as i32, table, &mut bap[ch]);
    }

    let num_cpl_bands = {
        let st = DEFAULT_CPL_BAND_STRUCT;
        let n = cpl_end_sub - cpl_start_sub;
        (1..n).filter(|&sub| st[cpl_start_sub + sub] == 0).count() + 1
    };
    let spx_st: [u8; 17] = {
        let mut st = DEFAULT_SPX_BAND_STRUCT;
        if cfg.spx.is_some_and(|s| s.custom_bands) {
            // a structure of one band per three subbands, then one more
            for (i, v) in st.iter_mut().enumerate() {
                *v = (i % 3 != 0) as u8;
            }
        }
        st
    };
    let spx_band_sizes = if cfg.spx.is_some() { spx_bands(spx_start_sub, spx_end_sub, &spx_st) } else { Vec::new() };

    for blk in 0..6 {
        w.flag(false); // dynrnge
        if cfg.acmod == 0 {
            w.flag(false);
        }
        // spectral extension
        if blk == 0 {
            w.flag(cfg.spx.is_some()); // spxinu
            if let Some(s) = cfg.spx {
                if cfg.acmod != 1 {
                    for ch in 1..=fbw {
                        w.flag(uses_spx(ch)); // chinspx
                    }
                }
                w.put(s.dst, 2);
                w.put(s.begin, 3);
                w.put(s.end, 3);
                w.flag(s.custom_bands); // spxbndstrce
                if s.custom_bands {
                    for sub in 0..spx_end_sub - spx_start_sub - 1 {
                        w.flag(spx_st[spx_start_sub + 1 + sub] != 0);
                    }
                }
            }
        } else {
            w.flag(false); // spxstre
        }
        if cfg.spx.is_some() {
            for ch in 1..=fbw {
                if !uses_spx(ch) {
                    continue;
                }
                if blk == 0 {
                    w.put(rng.below(32), 5); // spxblnd
                    w.put(rng.below(4), 2); // mspxcoord
                    for _ in 0..spx_band_sizes.len() {
                        w.put(3 + rng.below(6), 4); // spxcoexp
                        w.put(rng.below(4), 2); // spxcomant
                    }
                } else if blk == 3 {
                    // new coordinates
                    w.flag(true);
                    w.put(rng.below(32), 5);
                    w.put(rng.below(4), 2);
                    for _ in 0..spx_band_sizes.len() {
                        w.put(3 + rng.below(6), 4);
                        w.put(rng.below(4), 2);
                    }
                } else {
                    w.flag(false);
                }
            }
        }
        // coupling
        let mut phase_flags_in_use = false;
        if cfg.acmod > 1 && blk == 0 && cpl_blocks {
            w.flag(cfg.ecpl); // ecplinuse
            if cfg.acmod != 2 {
                for ch in 1..=fbw {
                    w.flag(in_cpl(ch));
                }
            }
            if cfg.acmod == 2 {
                phase_flags_in_use = true;
                w.flag(true); // phsflginu
            }
            w.put(cpl_start_sub as u32, 4);
            if cfg.spx.is_none() {
                w.put(cpl_end_sub as u32 - 3, 4);
            }
            w.flag(false); // cplbndstrce
        }
        if cpl_blocks {
            let mut any = false;
            for ch in 1..=fbw {
                if !in_cpl(ch) {
                    continue;
                }
                if blk == 0 {
                    any = true;
                    w.put(rng.below(4), 2); // mstrcplco
                    for _ in 0..num_cpl_bands {
                        w.put(rng.below(10), 4); // cplcoexp
                        w.put(rng.below(16), 4); // cplcomant
                    }
                } else {
                    w.flag(false); // cplcoe
                }
            }
            if cfg.acmod == 2 && any && phase_flags_in_use {
                for _ in 0..num_cpl_bands {
                    w.flag(rng.below(2) == 1);
                }
            }
        }
        // rematrixing
        if cfg.acmod == 2 {
            if blk == 0 {
                let mut n = 4;
                if cpl_blocks && start_freq[0] <= 61 {
                    n -= 1 + (start_freq[0] == 37) as u32;
                } else if cfg.spx.is_some() && spx_src <= 61 {
                    n -= 1;
                }
                for _ in 0..n {
                    w.flag(rng.below(2) == 1);
                }
            } else {
                w.flag(false); // rematstr
            }
        }
        if blk == 0 {
            // channel bandwidth
            for ch in 1..=fbw {
                if !in_cpl(ch) && !uses_spx(ch) {
                    w.put(bw_code as u32, 6);
                }
            }
            // exponents
            for ch in ch_first..=channels {
                w.put(abs[ch] >> (ch == 0) as u32, 4);
                let (s, e) = (start_freq[ch], end_freq[ch]);
                let base = if ch == 0 { s } else { s + 1 };
                let ngrps = (e - base) / 3;
                let mut prev = if ch == 0 { abs[0] as i32 } else { exps[ch][0] as i32 };
                for g in 0..ngrps {
                    let mut code = 0;
                    for k in 0..3 {
                        let cur = exps[ch][base + g * 3 + k] as i32;
                        code = code * 5 + (cur - prev + 2) as u32;
                        prev = cur;
                    }
                    w.put(code, 7);
                }
                if ch != 0 && ch != lfe_ch {
                    w.put(0, 2); // gainrng
                }
            }
        }
        w.flag(false); // convsnroffste
        if cpl_blocks {
            if blk == 0 {
                w.put(cpl_fast_leak as u32, 3);
                w.put(cpl_slow_leak as u32, 3);
            } else {
                w.flag(false); // cplleake
            }
        }

        // the mantissas
        let (mut b1, mut b2, mut b4) = (0, 0, false);
        let mut got_cpl = false;
        let order: Vec<usize> = {
            let mut o = Vec::new();
            for ch in 1..=channels {
                o.push(ch);
                if ch <= fbw && in_cpl(ch) && !got_cpl {
                    o.push(0);
                    got_cpl = true;
                }
            }
            o
        };
        for ch in order {
            if cfg.aht {
                if blk == 0 {
                    aht_channel(&mut w, rng, &bap[ch], start_freq[ch], end_freq[ch], gaq_mode);
                }
                continue;
            }
            for bin in start_freq[ch]..end_freq[ch] {
                match bap[ch][bin] {
                    0 => {}
                    1 => {
                        if b1 > 0 {
                            b1 -= 1;
                        } else {
                            w.put(rng.below(27), 5);
                            b1 = 2;
                        }
                    }
                    2 => {
                        if b2 > 0 {
                            b2 -= 1;
                        } else {
                            w.put(rng.below(125), 7);
                            b2 = 2;
                        }
                    }
                    3 => w.put(rng.below(7), 3),
                    4 => {
                        if b4 {
                            b4 = false;
                        } else {
                            w.put(rng.below(121), 7);
                            b4 = true;
                        }
                    }
                    5 => w.put(rng.below(15), 4),
                    n => {
                        let bits = QUANTIZATION[n as usize] as u32;
                        w.put(rng.next(), bits);
                    }
                }
            }
        }
    }
    w.put(0, 16); // crc2, which is not checked

    // pad to a whole number of 16-bit words, and patch the frame size
    while !w.bits.len().is_multiple_of(16) {
        w.bits.push(false);
    }
    let words = w.bits.len() / 16;
    for i in 0..11 {
        w.bits[21 + i] = ((words - 1) >> (10 - i)) & 1 != 0;
    }
    w.bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i))).collect()
}

/// The AHT data of one channel in block 0: the gain adaptive quantisation mode and gain codes, then
/// each bin's six mantissas (vector quantised or gain adaptively quantised).
fn aht_channel(w: &mut Bw, rng: &mut Rng, hebap: &[u8; 256], start: usize, end: usize, gaq_mode: u32) {
    w.put(gaq_mode, 2);
    let end_bap: u8 = if gaq_mode < 2 { 12 } else { 17 };
    let mut gains = Vec::new();
    if gaq_mode == 1 || gaq_mode == 2 {
        for &h in &hebap[start..end] {
            if h > 7 && h < end_bap {
                let g = rng.below(2);
                w.put(g, 1);
                gains.push(g << (gaq_mode - 1));
            }
        }
    } else if gaq_mode == 3 {
        let qualifying: Vec<u8> = hebap[start..end].iter().copied().filter(|&h| h > 7 && h < 17).collect();
        for chunk in qualifying.chunks(3) {
            let g = [rng.below(3), rng.below(3), rng.below(3)];
            w.put(g[0] * 9 + g[1] * 3 + g[2], 5);
            gains.extend_from_slice(&g[..chunk.len()]);
        }
    }
    let mut gs = 0;
    for &h in &hebap[start..end] {
        let h = h as usize;
        let bits = BITS_VS_HEBAP[h] as u32;
        if h == 0 {
            continue;
        }
        if h < 8 {
            USED[0].fetch_add(1, Relaxed);
            w.put(rng.below(1 << bits), bits);
            continue;
        }
        let log_gain = if gaq_mode != 0 && h < end_bap as usize {
            gs += 1;
            gains[gs - 1]
        } else {
            0
        };
        let gbits = bits - log_gain;
        USED[if log_gain == 0 { 1 } else { 2 }].fetch_add(1, Relaxed);
        for _ in 0..6 {
            // now and then the code that announces a large mantissa
            let escape = log_gain != 0 && rng.below(4) == 0;
            if escape {
                USED[3].fetch_add(1, Relaxed);
                w.put(1 << (gbits - 1), gbits);
                let mbits = bits - (2 - log_gain);
                w.put(rng.next(), mbits);
            } else {
                let mut v = rng.next() & ((1 << gbits) - 1);
                if log_gain != 0 && v == 1 << (gbits - 1) {
                    v = 0;
                }
                w.put(v, gbits);
            }
        }
    }
}

/// A frame that fits the 11-bit frame size field (4096 bytes): the SNR offset is lowered until it does.
fn frame_fit(cfg: &Cfg, gaq_mode: u32, rng: &mut Rng) -> Vec<u8> {
    let mut max = 36;
    loop {
        let f = frame(cfg, gaq_mode, rng, max);
        if f.len() <= 4096 {
            return f;
        }
        max -= 2;
        assert!(max >= 12, "no SNR offset makes a frame that fits");
    }
}

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Decodes `frames` with this decoder and with FFmpeg; the SNR (dB) of ours against FFmpeg's, and
/// the reference's peak.
fn compare(name: &str, cfg: &Cfg, frames: usize, seed: u64) -> (f64, f32) {
    let mut rng = Rng(seed);
    let mut stream = Vec::new();
    let mut dec = Ac3Decoder::new();
    let mut got = Vec::new();
    for i in 0..frames {
        let f = frame_fit(cfg, i as u32 % 4, &mut rng);
        let mut r = BitReader::new(&f);
        let hdr = super::header::parse(&mut r).unwrap_or_else(|e| panic!("{name}: header: {e}"));
        assert_eq!(hdr.frame_size, f.len(), "{name}: the frame size");
        for fr in dec.decode(&Packet { stream: 0, pts: 0, duration: 1536, data: f.clone() }).unwrap_or_else(|e| panic!("{name}: frame {i}: {e}")) {
            let Frame::Audio(a) = fr else { unreachable!() };
            got.extend(a.samples.iter().map(|&x| x as f32));
        }
        stream.extend_from_slice(&f);
    }
    let dir = std::env::temp_dir().join(format!("ironav-eac3-synth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (input, output) = (dir.join(format!("{name}.eac3")), dir.join(format!("{name}.f32")));
    std::fs::write(&input, &stream).unwrap();
    let ok = Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "eac3", "-i"]).arg(&input).args(["-f", "f32le", "-acodec", "pcm_f32le"]).arg(&output).status().is_ok_and(|s| s.success());
    assert!(ok, "{name}: FFmpeg did not decode the stream");
    let want: Vec<f32> = std::fs::read(&output).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(&want) {
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
    (10.0 * (sig / noise.max(1e-300)).log10(), peak)
}

#[test]
fn synthetic_streams_decode_like_ffmpeg() {
    if !ffmpeg_ok() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let spx = |dst, begin, end, custom_bands| Some(Spx { dst, begin, end, custom_bands });
    let base = Cfg { acmod: 2, lfe: false, aht: false, spx: None, cpl: None, transient: false, atten: false, ecpl: false };
    let cases: Vec<(&str, Cfg)> = vec![
        ("plain_stereo", base),
        ("plain_surround_lfe", Cfg { acmod: 7, lfe: true, ..base }),
        ("cpl_stereo", Cfg { cpl: Some((2, 3)), ..base }),
        ("cpl_surround", Cfg { acmod: 7, lfe: true, cpl: Some((3, 0b10101)), ..base }),
        ("transient_atten", Cfg { transient: true, atten: true, ..base }),
        ("aht_stereo", Cfg { aht: true, ..base }),
        ("aht_mono", Cfg { acmod: 1, aht: true, ..base }),
        ("aht_surround_lfe", Cfg { acmod: 7, lfe: true, aht: true, ..base }),
        ("aht_cpl_stereo", Cfg { aht: true, cpl: Some((2, 3)), ..base }),
        ("aht_cpl_surround", Cfg { acmod: 7, lfe: true, aht: true, cpl: Some((4, 0b11011)), ..base }),
        ("spx_stereo", Cfg { spx: spx(0, 2, 3, false), ..base }),
        ("spx_stereo_wide", Cfg { spx: spx(1, 1, 6, true), atten: true, ..base }),
        ("spx_mono", Cfg { acmod: 1, spx: spx(0, 1, 5, false), ..base }),
        ("spx_surround_lfe", Cfg { acmod: 7, lfe: true, spx: spx(2, 3, 7, false), atten: true, ..base }),
        ("spx_cpl_stereo", Cfg { spx: spx(0, 3, 6, false), cpl: Some((1, 3)), ..base }),
        ("spx_cpl_surround", Cfg { acmod: 7, lfe: true, spx: spx(1, 4, 7, true), cpl: Some((2, 0b01111)), transient: true, ..base }),
        ("aht_spx_stereo", Cfg { aht: true, spx: spx(0, 2, 5, false), ..base }),
        ("aht_spx_surround", Cfg { acmod: 7, lfe: true, aht: true, spx: spx(1, 3, 6, true), atten: true, ..base }),
        ("aht_spx_cpl_surround", Cfg { acmod: 7, lfe: true, aht: true, spx: spx(0, 4, 7, false), cpl: Some((3, 0b11111)), atten: true, transient: true, ..base }),
    ];
    let mut failures = Vec::new();
    for (i, (name, cfg)) in cases.iter().enumerate() {
        let (snr, peak) = compare(name, cfg, 8, 0x9E3779B97F4A7C15 ^ ((i as u64 + 1) * 0x1000193));
        eprintln!("{name:>22}: SNR {snr:6.1} dB, peak {peak:.3}");
        if !(snr > 60.0 && peak > 1e-4) {
            failures.push(format!("{name}: SNR {snr:.1} dB, peak {peak}"));
        }
    }
    eprintln!("AHT bins: {} vector quantised, {} without a gain, {} with a gain above 1; {} large mantissas", USED[0].load(Relaxed), USED[1].load(Relaxed), USED[2].load(Relaxed), USED[3].load(Relaxed));
    assert!(failures.is_empty(), "differences from FFmpeg:\n{}", failures.join("\n"));
}

/// A frame header of another substream or a dependent frame: `strmtyp`, `substreamid`, and `words`
/// 16-bit words in all, the rest zeros.
fn other_frame(strmtyp: u32, substream: u32, words: usize) -> Vec<u8> {
    let mut w = Bw { bits: Vec::new() };
    w.put(0x0B77, 16);
    w.put(strmtyp, 2);
    w.put(substream, 3);
    w.put(words as u32 - 1, 11);
    w.put(0, 2);
    w.put(3, 2);
    w.put(2, 3);
    w.flag(false);
    w.put(16, 5);
    while w.bits.len() < words * 16 {
        w.bits.push(false);
    }
    w.bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i))).collect()
}

fn every_config() -> Vec<Cfg> {
    let spx = |dst, begin, end, custom_bands| Some(Spx { dst, begin, end, custom_bands });
    let base = Cfg { acmod: 2, lfe: false, aht: false, spx: None, cpl: None, transient: false, atten: false, ecpl: false };
    vec![
        base,
        Cfg { acmod: 1, aht: true, spx: spx(0, 1, 5, false), ..base },
        Cfg { acmod: 7, lfe: true, aht: true, spx: spx(0, 4, 7, false), cpl: Some((3, 0b11111)), atten: true, transient: true, ..base },
        Cfg { acmod: 7, lfe: true, spx: spx(1, 4, 7, true), cpl: Some((2, 0b01111)), ..base },
    ]
}

fn decode_all(dec: &mut Ac3Decoder, data: &[u8]) -> ironav_core::Result<Vec<Frame>> {
    dec.decode(&Packet { stream: 0, pts: 0, duration: 1536, data: data.to_vec() })
}

#[test]
fn synthetic_frames_decode_to_finite_samples() {
    let mut rng = Rng(7);
    for (i, cfg) in every_config().iter().enumerate() {
        let f = frame_fit(cfg, i as u32, &mut rng);
        let frames = decode_all(&mut Ac3Decoder::new(), &f).unwrap();
        let [Frame::Audio(a)] = &frames[..] else { panic!("one audio frame expected") };
        let channels = FBW[cfg.acmod] + cfg.lfe as usize;
        assert_eq!((a.sample_rate, a.channels as usize, a.samples.len()), (48000, channels, 1536 * channels));
        assert!(a.samples.iter().all(|s| s.is_finite()));
        assert!(a.samples.iter().any(|&s| s != 0.0));
    }
}

#[test]
fn dependent_and_other_substreams_are_skipped() {
    let mut rng = Rng(11);
    let cfg = every_config()[2];
    let ind = frame_fit(&cfg, 1, &mut rng);
    let alone = decode_all(&mut Ac3Decoder::new(), &ind).unwrap();
    // an independent frame, its dependent frame, then an independent frame of substream 1
    let dep = other_frame(1, 0, 40);
    let sub1 = other_frame(0, 1, 30);
    let mut packet = ind.clone();
    packet.extend_from_slice(&dep);
    packet.extend_from_slice(&sub1);
    let with = decode_all(&mut Ac3Decoder::new(), &packet).unwrap();
    let (Frame::Audio(a), Frame::Audio(b)) = (&alone[0], &with[0]) else { panic!() };
    assert_eq!(with.len(), 1, "only the independent frame of substream 0 is decoded");
    assert_eq!(a.samples, b.samples);
    // a packet of nothing but a dependent frame, even one cut short, has nothing in it
    assert!(decode_all(&mut Ac3Decoder::new(), &dep).unwrap().is_empty());
    assert!(decode_all(&mut Ac3Decoder::new(), &dep[..20]).unwrap().is_empty());

    let info = super::any_frame_info(&packet).unwrap();
    assert!(info.eac3 && !info.skipped && info.samples == 1536 && info.channels == 6);
    let info = super::any_frame_info(&packet[ind.len()..]).unwrap();
    assert!(info.eac3 && info.skipped);
    assert!(super::any_frame_info(&sub1).unwrap().skipped);
    // the AC-3 reader says the frame is not AC-3
    assert_eq!(super::frame_info(&ind).unwrap_err().code, ironav_core::ErrorCode::UnsupportedCodec);
    assert!(super::eac3_frame_info(&ind).is_ok());

    // the dec3 box describes the independent substream
    let dec3 = super::dec3_body(&ind, 640).unwrap();
    assert_eq!(dec3, [(640 >> 5) as u8, ((640 & 31) << 3) as u8, 16 << 1, (7 << 1) | 1, 0]);
    assert_eq!(super::dec3_channels(&dec3), Some(6));
    assert_eq!(super::dec3_channels(&dec3[..3]), None);
    assert!(super::dec3_body(&dep, 640).is_none());
    assert!(super::dac3_body(&ind).is_none());
}

#[test]
fn refused_features_are_named() {
    let mut rng = Rng(5);
    // enhanced coupling
    let cfg = Cfg { cpl: Some((2, 3)), ecpl: true, ..every_config()[0] };
    let f = frame_fit(&cfg, 0, &mut rng);
    let e = decode_all(&mut Ac3Decoder::new(), &f).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::UnsupportedCodec);
    assert!(e.message.contains("enhanced coupling"), "{}", e.message);
    // the reduced sample rates: fscod 3 with fscod2 0
    let mut g = frame_fit(&every_config()[0], 0, &mut rng);
    // bits 16 + 2 + 3 + 11 = 32 is fscod; set it to 3, then fscod2 (the next two bits) to 0
    g[4] |= 0xC0;
    g[4] &= !0x30;
    let e = decode_all(&mut Ac3Decoder::new(), &g).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::UnsupportedCodec);
    assert!(e.message.contains("reduced sample rates"), "{}", e.message);
    assert_eq!(super::any_frame_info(&g).unwrap().sample_rate, 24000);
}

/// Damaged frames of every kind decode or fail; none panics. Frames cut short fail.
#[test]
fn damaged_synthetic_frames_do_not_panic() {
    let mut rng = Rng(99);
    for (i, cfg) in every_config().iter().enumerate() {
        let f = frame_fit(cfg, i as u32, &mut rng);
        for n in 0..f.len() {
            assert!(decode_all(&mut Ac3Decoder::new(), &f[..n]).is_err(), "config {i}: {n} bytes decoded");
        }
        // one decoder takes all the damaged frames, so its state carries from one to the next
        let mut dec = Ac3Decoder::new();
        for byte in 0..f.len().min(500) {
            for bit in 0..8 {
                let mut g = f.clone();
                g[byte] ^= 1 << bit;
                let _ = decode_all(&mut dec, &g);
            }
        }
        for _ in 0..300 {
            let mut g = f.clone();
            for _ in 0..8 {
                let at = 4 + rng.below(g.len() as u32 - 4) as usize;
                g[at] = rng.next() as u8;
            }
            let _ = decode_all(&mut dec, &g);
        }
    }
}
