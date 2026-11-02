//! An Opus encoder at 48 kHz, mono or stereo: CELT mode (the default) with frames of 2.5, 5, 10 or
//! 20 ms at a constant bit rate, or SILK mode for speech, wideband, with frames of 10 or 20 ms at a
//! variable rate around the one asked for (stereo as mid and side). Packets of 40 or
//! 60 ms hold two or three 20 ms frames: one SILK frame of 40 or 60 ms in SILK mode, a code 3
//! packet in the others. With discontinuous transmission ([`OpusEncoder::with_dtx`]) a pause
//! sends packets of a TOC byte alone, and with in-band FEC ([`OpusEncoder::with_fec`]) each
//! SILK packet carries a coarser copy of the speech of the one before. In automatic mode
//! ([`OpusMode::Auto`]) the encoder picks SILK, hybrid or CELT for each packet by the bit rate and
//! whether the input sounds like speech, and smooths each change between CELT and the others
//! with a redundant 5 ms CELT frame, as libopus does.
//!
//! The frame coding is in `celt/enc.rs` and `silk_enc.rs`; this file turns a stream of samples into
//! packets: the pre-emphasis filter, or for SILK the resampling to 16 kHz, buffering for the
//! look-ahead, the TOC byte, and the end of the stream. Besides the [`Encoder`] trait, which takes frames of any length, a caller that has its
//! own framing (a voice link, say) codes one frame at a time into its own buffer with
//! [`OpusEncoder::encode_frame_into`], which allocates nothing.

use super::celt::enc::{FRAME, LOOKAHEAD};
use super::silk_resampler;
use super::celt::prefilter::Prefilter;
use super::celt::Celt;
use super::rc::{Coder, RangeEncoder};
use super::silk_enc::{self, SilkEncoder, SilkStereo};
use crate::options::{OpusMode, OpusSignal};
use crate::resample::StreamResampler;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};

const RATE: u32 = 48_000;
/// The high-pass pre-emphasis the decoder undoes with its de-emphasis filter.
const EMPH_COEFF: f32 = 0.850_006_1;
/// The first frame and every this many after it code their band energies without prediction, so a
/// player that starts in the middle recovers.
const INTRA_PERIOD: u64 = 50;
/// Samples the decoder must drop from the start: the first frame's window has nothing to overlap with.
pub const PRE_SKIP: usize = LOOKAHEAD;

/// Samples (at 16 kHz) of zeros before the input in SILK mode, so that each frame of input brings
/// a whole frame with its look-ahead: the look-ahead and the resampler's 1 ms.
const SILK_LEAD: usize = silk_enc::LOOKAHEAD + 16;
/// Samples (at 48 kHz) a decoder must drop from the start of a SILK stream: the lead above, the
/// decoder's own sample, the resampler's delay (seven samples at 16 kHz) and 13 more samples at
/// 48 kHz, which decoding our streams with libopus showed the lead does not cover.
pub const SILK_PRE_SKIP: usize = 3 * (SILK_LEAD + 1 + silk_resampler::input_delay(16)) + 13;

/// How much the CELT input of a hybrid frame waits, so that its delay is the SILK side's.
const HYBRID_SHIFT: usize = SILK_PRE_SKIP - PRE_SKIP;
/// The first CELT band of a hybrid frame: 8 kHz, where SILK's wideband ends.
const HYBRID_START: usize = 17;

/// Discontinuous transmission: after this long without activity frames are not sent...
const DTX_AFTER_MS: f64 = 200.0;
/// ...but for one this often, so that the decoder's idea of the background keeps up.
const DTX_REFRESH_MS: f64 = 400.0;

/// Tells activity from background noise by the level of each frame against the background's,
/// and decides which frames a pause sends.
struct Dtx {
    on: bool,
    /// The background's level, in dB of full scale: it follows the quietest frames down at once
    /// and rises slowly, and never above -45 dB, so anything louder than -36 dB counts.
    floor: f64,
    /// Milliseconds without activity, and since the last frame sent in a pause.
    quiet: f64,
    since: f64,
    /// Whether the last frame was not sent.
    skipped: bool,
    /// The de-emphasis of the CELT input, to measure its level.
    deemph: f32,
}

impl Dtx {
    fn new() -> Self {
        Self { on: false, floor: -60.0, quiet: 0.0, since: 0.0, skipped: false, deemph: 0.0 }
    }

    /// Whether to send a frame of `ms` milliseconds whose mean square is `energy`.
    fn send(&mut self, energy: f64, ms: f64) -> bool {
        let db = 10.0 * (energy + 1e-12).log10();
        self.floor = if db < self.floor { db } else { (self.floor + 0.003 * ms).min(-45.0) };
        let active = db > (self.floor + 9.0).max(-65.0);
        self.quiet = if active { 0.0 } else { self.quiet + ms };
        let send = if self.quiet <= DTX_AFTER_MS {
            true
        } else {
            self.since += ms;
            self.since >= DTX_REFRESH_MS
        };
        if send {
            self.since = 0.0;
        }
        send
    }
}

/// A guess at whether the input is speech, from how its level moves: speech comes in syllables
/// and words, with dips of 20 dB and more between them several times a second, where music
/// mostly holds its level. The spread of the 10 ms levels over the last two seconds, 4 dB or
/// less for music and 10 or more for speech, smoothed over half a second.
struct VoiceEst {
    db: [f32; 200],
    at: usize,
    filled: usize,
    /// 0 for music to 1 for speech.
    voice: f64,
}

impl VoiceEst {
    fn new() -> Self {
        Self { db: [0.0; 200], at: 0, filled: 0, voice: 0.5 }
    }

    /// Takes the next frame of 16 kHz input.
    fn update(&mut self, x: &[f32]) {
        for block in x.as_chunks::<160>().0.iter() {
            let e = block.iter().map(|&v| (v * v) as f64).sum::<f64>() / 160.0;
            self.db[self.at] = (10.0 * (e + 1e-12).log10()).max(-60.0) as f32;
            self.at = (self.at + 1) % self.db.len();
            self.filled = (self.filled + 1).min(self.db.len());
            if self.filled >= 50 {
                let w = &self.db[..self.filled];
                let mean = w.iter().map(|&v| v as f64).sum::<f64>() / w.len() as f64;
                let sd = (w.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / w.len() as f64).sqrt();
                let raw = ((sd - 4.0) / 6.0).clamp(0.0, 1.0);
                self.voice += 0.02 * (raw - self.voice);
            }
        }
    }
}

/// The SILK side: the resampler to 16 kHz and the signal waiting to be coded.
struct SilkPath {
    enc: SilkEncoder,
    /// The side channel's coder, for stereo.
    stereo: Option<SilkStereo>,
    rs: StreamResampler,
    /// 16 kHz samples (the mid, for stereo), from `silk_enc::PAST` before the next frame, and
    /// the side.
    buf: Vec<f32>,
    side: Vec<f32>,
    /// The input on its way to the resampler (mixed to mono for a mono stream), and out of it.
    mono: Vec<f32>,
    out: Vec<f32>,
}

impl SilkPath {
    fn new(stereo: bool) -> Result<Self> {
        // three frames and their look-ahead, and one more arriving
        let lead = |on: bool| {
            let mut v = Vec::with_capacity(if on { silk_enc::PAST + 4 * 320 + SILK_LEAD } else { 0 });
            v.resize(if on { silk_enc::PAST + SILK_LEAD } else { 0 }, 0.0);
            v
        };
        Ok(Self {
            enc: SilkEncoder::new(),
            stereo: stereo.then(SilkStereo::new),
            rs: StreamResampler::new(RATE, 16_000, 1 + stereo as usize, FRAME)?,
            side: lead(stereo),
            buf: lead(true),
            mono: Vec::with_capacity(2 * FRAME),
            out: Vec::with_capacity(2 * FRAME),
        })
    }

    /// Takes `n` (at most a 20 ms frame) samples per channel of `channels` interleaved from `get`,
    /// and moves what the resampler has ready to the buffers, within full scale: the decoder's
    /// output stops there, and an input it cannot follow would make the coder chase it.
    fn feed(&mut self, n: usize, channels: usize, get: impl Fn(usize) -> f32) -> Result<()> {
        self.mono.clear();
        if self.stereo.is_some() {
            self.mono.extend((0..n * channels).map(&get));
        } else {
            for i in 0..n {
                self.mono.push((0..channels).map(|c| get(i * channels + c)).sum::<f32>() / channels as f32);
            }
        }
        self.rs.push(&self.mono)?;
        let ch = self.rs.channels();
        self.out.resize(self.rs.ready() * ch, 0.0);
        let got = self.rs.pull(&mut self.out);
        for f in self.out[..got].chunks_exact(ch) {
            if let [l, r] = f {
                let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
                self.buf.push(0.5 * (l + r));
                self.side.push(0.5 * (l - r));
            } else {
                self.buf.push(f[0].clamp(-1.0, 1.0));
            }
        }
        Ok(())
    }

    /// Codes `n` frames of `fl` samples at the front of the buffers into `rc`, as
    /// [`SilkEncoder::encode`] does.
    fn code(&mut self, rc: &mut RangeEncoder, fl: usize, n: usize, target_bits: f64, max_bits: i32, keep: bool) -> i32 {
        let need = silk_enc::PAST + n * fl + silk_enc::LOOKAHEAD;
        match &mut self.stereo {
            None => self.enc.encode(rc, &self.buf[..need], n, fl / 80, target_bits, max_bits, keep),
            Some(st) => st.encode(&mut self.enc, rc, &self.buf[..need], &self.side[..need], n, fl / 80, target_bits, max_bits, keep),
        }
    }

    fn set_fec(&mut self, loss: u32) {
        self.enc.set_fec(loss);
        if let Some(st) = &mut self.stereo {
            st.set_fec(loss);
        }
    }

    /// Starts the coders again, as a decoder does after CELT frames.
    fn restart(&mut self) {
        self.enc.restart();
        if let Some(st) = &mut self.stereo {
            st.restart();
        }
    }

    /// Moves past a frame of `fl` samples.
    fn advance(&mut self, fl: usize) {
        self.buf.drain(..fl);
        if self.stereo.is_some() {
            self.side.drain(..fl);
        }
    }

    /// Notes the coders' state after a packet that is sent.
    fn mark_sent(&mut self) {
        self.enc.mark_sent();
        if let Some(st) = &mut self.stereo {
            st.mark_sent();
        }
    }

    /// Goes back to the state after the last packet sent, as a decoder has it after a pause.
    fn rewind(&mut self) {
        self.enc.rewind();
        if let Some(st) = &mut self.stereo {
            st.rewind();
        }
    }

    /// The mean square of the frame of `fl` samples at the front, mid and side.
    fn energy(&self, fl: usize) -> f64 {
        let sq = |x: &[f32]| x.iter().map(|&v| (v * v) as f64).sum::<f64>();
        let at = silk_enc::PAST..silk_enc::PAST + fl;
        (sq(&self.buf[at.clone()]) + if self.stereo.is_some() { sq(&self.side[at]) } else { 0.0 }) / fl as f64
    }
}

/// The `OpusHead` that describes a stream from a CELT-mode encoder to a container
/// ([`OpusEncoder::head`] for any mode).
pub fn opus_head(channels: usize) -> Vec<u8> {
    head_with(channels, PRE_SKIP)
}

fn head_with(channels: usize, pre_skip: usize) -> Vec<u8> {
    if (3..=8).contains(&channels) {
        return super::multistream::family1_head(channels, pre_skip);
    }
    let mut v = b"OpusHead".to_vec();
    v.push(1);
    v.push(channels as u8);
    v.extend((pre_skip as u16).to_le_bytes());
    v.extend(RATE.to_le_bytes());
    v.extend(0i16.to_le_bytes());
    v.push(0); // channel mapping family 0
    v
}

pub struct OpusEncoder {
    celt: Celt,
    rc: RangeEncoder,
    pre: Prefilter,
    channels: usize,
    bitrate: u32,
    /// The frame's size code (0 to 3: 2.5 to 20 ms) and samples.
    size: i32,
    frame: usize,
    /// Bytes of a full packet, TOC included.
    bytes: usize,
    /// The previous input sample of each channel, for the pre-emphasis.
    prev: [f32; 2],
    /// Pre-emphasised samples not yet fully coded, from the start of the next frame.
    sig: Vec<Vec<f32>>,
    frames: u64,
    /// Frames coded with short blocks.
    pub transients: u64,
    /// Samples per channel received.
    input: u64,
    silk: Option<Box<SilkPath>>,
    mode: OpusMode,
    /// Frames in a packet: 1, or 2 or 3 for 40 and 60 ms.
    per_packet: usize,
    /// The frames of the packet being put together, each with its TOC byte, and their lengths.
    parts: Vec<u8>,
    part_lens: [usize; 3],
    nparts: usize,
    dtx: Dtx,
    /// Frames (of up to 20 ms) not sent, in pauses with discontinuous transmission.
    pub skipped: u64,
    /// The packet loss in-band FEC is for, in percent (0: off).
    fec: u32,
    /// The frame of the packet being coded (0 to 2).
    part: usize,
    /// The mode of the packet being coded (never `Auto`), and of the frame coded before it.
    cur: OpusMode,
    prev_mode: Option<OpusMode>,
    /// Whether the frame before had a redundant CELT frame: a decoder then carries CELT's state
    /// on from it at a change of mode.
    prev_red: bool,
    /// In automatic mode: the packet starts with a redundant CELT frame (it follows CELT), or
    /// ends with one (CELT follows it); and packets since the last change.
    red_in: bool,
    to_celt: bool,
    hold: u32,
    signal: OpusSignal,
    voice: VoiceEst,
    /// The coder of the redundant frames.
    red_rc: RangeEncoder,
}

impl OpusEncoder {
    /// An encoder for `channels` (1 or 2) at about `bitrate` bits per second.
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if sample_rate != RATE {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Opus encoder takes 48000 Hz audio, not {sample_rate} Hz"))
                .hint("resample the audio to 48000 Hz first"));
        }
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Opus encoder takes mono or stereo audio, not {channels} channels")));
        }
        let mut sig = vec![Vec::with_capacity(2 * FRAME + LOOKAHEAD); channels];
        for s in sig.iter_mut() {
            s.resize(LOOKAHEAD, 0.0);
        }
        Ok(Self {
            celt: Celt::new(channels, true),
            rc: RangeEncoder::new(1275),
            pre: Prefilter::default(),
            channels,
            bitrate,
            size: 3,
            frame: FRAME,
            bytes: Self::frame_bytes(bitrate, FRAME, channels),
            prev: [0.0; 2],
            sig,
            frames: 0,
            transients: 0,
            input: 0,
            silk: None,
            mode: OpusMode::Celt,
            per_packet: 1,
            parts: Vec::new(),
            part_lens: [0; 3],
            nparts: 0,
            dtx: Dtx::new(),
            skipped: 0,
            fec: 0,
            part: 0,
            cur: OpusMode::Celt,
            prev_mode: None,
            prev_red: false,
            red_in: false,
            to_celt: false,
            hold: 0,
            signal: OpusSignal::Auto,
            voice: VoiceEst::new(),
            red_rc: RangeEncoder::new(257),
        })
    }

    /// Codes in `mode`. Only before any audio; SILK, hybrid and automatic modes take frames of
    /// 10 ms or more.
    pub fn with_mode(mut self, mode: OpusMode) -> Result<Self> {
        if self.input > 0 {
            return Err(Error::invalid_arg("the mode is set before any audio"));
        }
        if mode != OpusMode::Celt && self.size < 2 {
            return Err(Error::invalid_arg("SILK, hybrid and automatic modes take frames of 10 ms or more").alternatives(["10", "20", "40", "60"]));
        }
        self.silk = match mode {
            OpusMode::Celt => None,
            OpusMode::Silk | OpusMode::Hybrid | OpusMode::Auto => {
                let mut s = SilkPath::new(self.channels == 2)?;
                s.set_fec(self.fec);
                Some(Box::new(s))
            }
        };
        // a hybrid frame's CELT input waits for the SILK side
        let lead = if self.feeds_celt_with(mode) { LOOKAHEAD + HYBRID_SHIFT } else { LOOKAHEAD };
        self.celt = Celt::new(self.channels, true);
        self.sig = vec![vec![0.0; lead]; self.channels];
        self.mode = mode;
        self.cur = if mode == OpusMode::Auto { self.choose() } else { mode };
        self.bytes = self.max_bytes();
        self.parts = vec![0; self.per_packet * self.bytes];
        Ok(self)
    }

    /// Discontinuous transmission, on or off (the default): in a pause (200 ms without activity:
    /// silence or a steady background), frames are not sent but for one every 400 ms; a packet
    /// is then its TOC byte alone, and a decoder conceals the frame as one lost. Saves most of
    /// the bit rate of a voice link while nobody talks.
    pub fn with_dtx(mut self, on: bool) -> Self {
        self.dtx.on = on;
        self
    }

    /// In-band forward error correction (LBRR) for `loss` percent of packets lost; 0, the default,
    /// is off. Each active SILK frame (in SILK and hybrid modes) is coded again, coarser, into the
    /// next packet, so a decoder that lost a packet rebuilds its speech from the next one
    /// ([`OpusDecoder::decode_fec_into`](super::OpusDecoder::decode_fec_into)); the more loss, the
    /// finer the copy. The copies come out of the bit rate. In 40 and 60 ms hybrid packets only
    /// the last frame has a copy (the others would travel with their own frames), and CELT
    /// frames have none.
    pub fn with_fec(mut self, loss: u32) -> Self {
        self.fec = loss.min(100);
        if let Some(s) = &mut self.silk {
            s.set_fec(self.fec);
        }
        self
    }

    /// What the input is, for automatic mode: by default it is guessed from how the level moves
    /// (speech dips between syllables and words; music mostly holds its level), which takes a
    /// second or two to settle and can mistake music with many pauses for speech.
    pub fn with_signal(mut self, signal: OpusSignal) -> Self {
        self.signal = signal;
        if self.input == 0 && self.mode == OpusMode::Auto {
            self.cur = self.choose();
        }
        self
    }

    /// Changes the bit rate, between packets: a voice link adapting to the network, say. In
    /// automatic mode the mode follows. [`packet_bytes`](Self::packet_bytes) changes with it.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if self.nparts != 0 {
            return Err(Error::invalid_arg("the bit rate changes between packets"));
        }
        self.bitrate = bitrate;
        self.bytes = self.max_bytes();
        self.parts.resize(self.per_packet * self.bytes, 0);
        Ok(())
    }

    /// The bit rate asked for.
    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// SILK frames in a packet: a SILK packet of 40 or 60 ms is one SILK frame of 40 or 60 ms,
    /// which holds two or three 20 ms frames.
    fn silk_frames(&self) -> usize {
        if self.cur == OpusMode::Silk { self.per_packet } else { 1 }
    }

    /// Whether the CELT side takes the input, delayed to the SILK side's delay.
    fn feeds_celt_with(&self, mode: OpusMode) -> bool {
        matches!(mode, OpusMode::Hybrid | OpusMode::Auto)
    }

    /// The mode the encoder was set to code with.
    pub fn mode(&self) -> OpusMode {
        self.mode
    }

    /// The mode of the last packet coded (or of the next, before any): in automatic mode, the
    /// one picked.
    pub fn current_mode(&self) -> OpusMode {
        self.cur
    }

    /// The most bytes a frame takes (a SILK packet of 40 or 60 ms counts as one): CELT's are all
    /// this size, SILK's and hybrid's twice the bit rate's share at most.
    fn max_bytes(&self) -> usize {
        let silk = |frames: usize| (2 * self.bitrate as usize * self.frame * frames / RATE as usize / 8 + 1).clamp(80, 1275);
        match self.mode {
            OpusMode::Celt => Self::frame_bytes(self.bitrate, self.frame, self.channels),
            OpusMode::Silk | OpusMode::Hybrid => silk(self.silk_frames()),
            OpusMode::Auto => silk(self.per_packet).max(Self::frame_bytes(self.bitrate, self.frame, self.channels)),
        }
    }

    /// The mode for the bit rate and the signal, with some hysteresis around the current one.
    fn choose(&self) -> OpusMode {
        let v = match self.signal {
            OpusSignal::Voice => 1.0,
            OpusSignal::Music => 0.0,
            OpusSignal::Auto => self.voice.voice,
        };
        let rate = self.bitrate as f64;
        let stereo = self.channels == 2;
        let started = self.prev_mode.is_some();
        let voice_celt = if stereo { 44_000.0 } else { 64_000.0 };
        let mut celt = 16_000.0 + v * v * (voice_celt - 16_000.0);
        if started {
            celt += if self.cur == OpusMode::Celt { -4000.0 } else { 4000.0 };
        }
        if rate >= celt {
            return OpusMode::Celt;
        }
        let mut hybrid = if stereo { 24_000.0 } else { 16_000.0 };
        if started {
            hybrid += match self.cur {
                OpusMode::Hybrid => -1000.0,
                OpusMode::Silk => 1000.0,
                _ => 0.0,
            };
        }
        if rate >= hybrid { OpusMode::Hybrid } else { OpusMode::Silk }
    }

    /// In automatic mode, picks the next packet's mode. A change to CELT waits a packet, which
    /// ends with a redundant CELT frame; a change from CELT starts with one. A mode is kept for
    /// three packets at least.
    fn decide(&mut self) {
        if self.mode != OpusMode::Auto {
            return;
        }
        self.hold = self.hold.saturating_add(1);
        if self.to_celt {
            self.to_celt = false;
            self.switch(OpusMode::Celt);
            return;
        }
        let want = self.choose();
        if want == self.cur {
            return;
        }
        if self.prev_mode.is_none() {
            self.cur = want;
            return;
        }
        if self.hold < 3 {
            return;
        }
        if want == OpusMode::Celt {
            self.to_celt = true;
            self.hold = 0;
            return;
        }
        self.switch(want);
    }

    fn switch(&mut self, mode: OpusMode) {
        if self.cur == OpusMode::Celt {
            // a decoder starts SILK again after CELT
            self.red_in = true;
            if let Some(s) = &mut self.silk {
                s.restart();
            }
        }
        self.cur = mode;
        self.hold = 0;
        self.bytes = self.max_bytes();
    }

    /// The size of a redundant frame in a frame of `frame` samples, as libopus sizes it: about
    /// 1.5 times the bit rate's share for 5 ms, within what the frame holds; 0 for none.
    fn red_bytes(&self, frame: usize) -> usize {
        let ch = self.channels as i64;
        let frame_rate = RATE as i64 / frame as i64;
        let base_bits = 40 * ch + 20;
        let rate = 3 * (self.bitrate as i64 + base_bits * (200 - frame_rate)) / 2;
        let available = self.bytes as i64 * 8 - 2 * base_bits;
        let cap = (available * 240 / (240 + RATE as i64 / frame_rate) + base_bits) / 8;
        let bytes = (rate / 1600).min(cap);
        if bytes > 4 + 8 * ch { bytes.min(257) as usize } else { 0 }
    }

    /// Codes a redundant 5 ms CELT frame of the input from `at` (frames of the packet's SILK side's
    /// timing) up to band `end` into `red_rc`. One `first` in a frame comes after CELT and carries
    /// its state on, after which CELT starts again; one at the end starts CELT again and the CELT
    /// frames after it carry on from it.
    fn code_redundant(&mut self, at: usize, first: bool, end: usize, bytes: usize) -> Result<()> {
        if !first {
            self.celt.flush();
        }
        self.red_rc.reset(bytes);
        let n = 240 + LOOKAHEAD;
        let (a, b) = (&self.sig[0][at..at + n], self.sig.get(1).map_or(&[][..], |s| &s[at..at + n]));
        let both = [a, b];
        self.celt.encode_bands(&mut self.red_rc, &both[..self.channels], 1, false, true, None, 0, end);
        self.red_rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a redundant frame did not fit"))?;
        if first {
            self.celt.flush();
        }
        Ok(())
    }

    /// In automatic mode, keeps the pre-filter's history and the CELT input in step over `n`
    /// samples coded without the pre-filter.
    fn skip_celt(&mut self, n: usize) {
        if self.mode != OpusMode::Auto {
            return;
        }
        let (a, b) = (&self.sig[0][..], self.sig.get(1).map_or(&[][..], |s| &s[..]));
        let both = [a, b];
        self.pre.skip(&both[..self.channels], n);
    }

    /// Samples (at 48 kHz) a decoder drops from the start of the stream.
    pub fn pre_skip(&self) -> usize {
        if self.silk.is_some() { SILK_PRE_SKIP } else { PRE_SKIP }
    }

    /// The `OpusHead` that describes this encoder's stream to a container.
    pub fn head(&self) -> Vec<u8> {
        head_with(self.channels, self.pre_skip())
    }

    /// A frame's worth of the bit rate, but never less than a few bytes or more than a frame can hold.
    fn frame_bytes(bitrate: u32, frame: usize, channels: usize) -> usize {
        (bitrate as usize * frame / RATE as usize / 8).clamp(8 * channels, 1275)
    }

    /// Packets of `ms` milliseconds: 2.5, 5, 10, 20 (the default), 40 or 60. Shorter frames lower
    /// the delay and cost quality at the same bit rate; 40 and 60 ms packets hold two or three
    /// 20 ms frames and save a little of the per-packet cost. Only before any audio.
    pub fn with_frame_ms(mut self, ms: f64) -> Result<Self> {
        let (size, per_packet) = match ms {
            2.5 => (0, 1),
            5.0 => (1, 1),
            10.0 => (2, 1),
            20.0 => (3, 1),
            40.0 => (3, 2),
            60.0 => (3, 3),
            _ => return Err(Error::invalid_arg(format!("Opus packets are 2.5, 5, 10, 20, 40 or 60 ms, not {ms}")).alternatives(["2.5", "5", "10", "20", "40", "60"])),
        };
        if self.input > 0 {
            return Err(Error::invalid_arg("the frame length is set before any audio"));
        }
        if self.silk.is_some() && size < 2 {
            return Err(Error::invalid_arg(format!("SILK, hybrid and automatic modes take frames of 10 ms or more, not {ms}")).alternatives(["10", "20", "40", "60"]));
        }
        self.size = size;
        self.frame = FRAME >> (3 - size);
        self.per_packet = per_packet;
        self.bytes = self.max_bytes();
        self.parts = vec![0; per_packet * self.bytes];
        Ok(self)
    }

    /// Samples per channel in a packet (at 48 kHz): what [`encode_frame_into`](Self::encode_frame_into) takes.
    pub fn frame_samples(&self) -> usize {
        self.frame * self.per_packet
    }

    /// The most bytes a packet takes, TOC included. In CELT mode every packet of 2.5 to 20 ms but
    /// those of digital silence (3) is this long.
    pub fn packet_bytes(&self) -> usize {
        if self.per_packet == 1 || self.mode == OpusMode::Silk {
            self.bytes
        } else {
            // the TOC, the frame count, the lengths of all frames but the last (up to two bytes),
            // and the frames without their own TOC bytes
            2 + 2 * (self.per_packet - 1) + self.per_packet * (self.bytes - 1)
        }
    }

    /// The delay the encoder adds, in samples at 48 kHz: a frame of buffering and the window's
    /// look-ahead (the decoder drops the look-ahead as the stream's pre-skip). A frame passed
    /// to [`encode_frame_into`](Self::encode_frame_into) comes out of a decoder this much later.
    pub fn delay_samples(&self) -> usize {
        self.frame_samples() + self.pre_skip()
    }

    /// [`delay_samples`](Self::delay_samples) in milliseconds.
    pub fn delay_ms(&self) -> f64 {
        self.delay_samples() as f64 * 1000.0 / RATE as f64
    }

    /// Codes one frame: `pcm` holds exactly [`frame_samples`](Self::frame_samples) interleaved
    /// samples per channel (-1 to 1), and the packet, TOC byte first, is written to the start of
    /// `out` (at least [`packet_bytes`](Self::packet_bytes) long). Returns the packet's length.
    /// Allocates nothing. It cannot follow audio passed to [`Encoder::encode`] that has not made a
    /// whole frame.
    pub fn encode_frame_into(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        self.encode_with(pcm.len(), |i| pcm[i], out)
    }

    /// [`encode_frame_into`](Self::encode_frame_into) for 16-bit samples.
    pub fn encode_frame_into_i16(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        self.encode_with(pcm.len(), |i| pcm[i] as f32 / 32768.0, out)
    }

    fn encode_with(&mut self, len: usize, get: impl Fn(usize) -> f32, out: &mut [u8]) -> Result<usize> {
        let n = self.frame_samples();
        if len != n * self.channels {
            return Err(Error::invalid_arg(format!("a frame is {} samples ({n} per channel), not {len}", n * self.channels)));
        }
        if out.len() < self.packet_bytes() {
            return Err(Error::invalid_arg(format!("the packet needs {} bytes, the buffer has {}", self.packet_bytes(), out.len())));
        }
        if self.nparts != 0 {
            return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its packet"));
        }
        self.decide();
        if self.per_packet == 1 || self.cur == OpusMode::Silk {
            self.part = 0;
            return self.encode_one(&get, out);
        }
        let step = self.frame * self.channels;
        let mut parts = std::mem::take(&mut self.parts);
        for k in 0..self.per_packet {
            self.part = k;
            let at = k * self.bytes;
            let r = self.encode_one(&|i| get(k * step + i), &mut parts[at..at + self.bytes]);
            match r {
                Ok(l) => self.part_lens[k] = l,
                Err(e) => {
                    self.parts = parts;
                    return Err(e);
                }
            }
        }
        self.parts = parts;
        self.nparts = self.per_packet;
        Ok(self.assemble(out))
    }

    /// Puts the frames in `parts` (each behind its own TOC byte) into one code 3 packet in `out`,
    /// their sizes stated only when they differ. Returns its length.
    fn assemble(&mut self, out: &mut [u8]) -> usize {
        let m = self.per_packet;
        let lens = self.part_lens;
        // a frame of TOC alone (nothing to send) is a frame of length 0
        let body = |k: usize| lens[k].saturating_sub(1);
        let vbr = (1..m).any(|k| body(k) != body(0));
        out[0] = self.parts[0] | 3;
        out[1] = ((vbr as u8) << 7) | m as u8;
        let mut at = 2;
        if vbr {
            for k in 0..m - 1 {
                let l = body(k);
                if l < 252 {
                    out[at] = l as u8;
                    at += 1;
                } else {
                    let b0 = 252 + (l - 252) % 4;
                    out[at] = b0 as u8;
                    out[at + 1] = ((l - b0) / 4) as u8;
                    at += 2;
                }
            }
        }
        for k in 0..m {
            let src = k * self.bytes + 1;
            out[at..at + body(k)].copy_from_slice(&self.parts[src..src + body(k)]);
            at += body(k);
        }
        self.nparts = 0;
        at
    }

    /// Codes one frame of `frame` samples per channel into `out`, TOC first.
    fn encode_one(&mut self, get: &dyn Fn(usize) -> f32, out: &mut [u8]) -> Result<usize> {
        let (total, ch) = (self.frame * self.silk_frames(), self.channels);
        if let Some(s) = &mut self.silk {
            if !self.input.is_multiple_of(self.frame as u64) {
                return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its frame"));
            }
            for start in (0..total).step_by(FRAME) {
                s.feed(total.min(start + FRAME) - start, ch, |i| get(start * ch + i))?;
            }
            if self.feeds_celt_with(self.mode) {
                for i in 0..total {
                    for c in 0..self.channels {
                        self.push(c, get(i * self.channels + c));
                    }
                }
            }
            self.input += total as u64;
            return self.code_one(out);
        }
        if self.sig[0].len() != LOOKAHEAD {
            return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its frame"));
        }
        for i in 0..self.frame {
            for c in 0..self.channels {
                self.push(c, get(i * self.channels + c));
            }
        }
        self.input += self.frame as u64;
        self.code_one(out)
    }

    /// The bit rate used when none is asked for: 64 kb/s per channel.
    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    /// Codes the SILK frame at the front of the buffer into `out`, and returns the packet's length.
    fn code_silk(&mut self, out: &mut [u8]) -> Result<usize> {
        let n = self.silk_frames();
        let fl = self.frame / 3;
        let red_in = std::mem::take(&mut self.red_in);
        let red = if red_in || self.to_celt { self.red_bytes(n * self.frame) } else { 0 };
        if red_in && red > 0 {
            // from the CELT frames before, which it follows
            self.code_redundant(0, true, 17, red)?;
        }
        let s = self.silk.as_mut().expect("SILK mode");
        let need = silk_enc::PAST + n * fl + silk_enc::LOOKAHEAD;
        if s.buf.len() < need {
            return Err(Error::new(ErrorCode::Internal, "Opus: the SILK frame is not all here"));
        }
        let size = self.bytes - red;
        self.rc.reset(size - 1);
        let target = self.bitrate as f64 * (n * self.frame) as f64 / RATE as f64 - 8.0 - (8 * red) as f64;
        s.code(&mut self.rc, fl, n, target, ((size - 1) * 8) as i32 - (red > 0) as i32, true);
        if red > 0 {
            // where the redundant frame goes: a SILK frame's leftover bytes hold it
            self.rc.log(1, red_in as u32);
        }
        let used = self.rc.tell();
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // the frame ends where the coder's bits do: any more would read as a redundancy frame
        let len = (used as usize).div_ceil(8);
        debug_assert!(self.rc.bytes()[len..].iter().all(|&b| b == 0));
        // configurations 8 to 11: SILK, wideband, 10, 20, 40 or 60 ms; one frame per packet
        let config = if n == 1 { 8 + self.size as u8 - 2 } else { 8 + n as u8 };
        out[0] = (config << 3) | (((self.channels == 2) as u8) << 2);
        out[1..1 + len].copy_from_slice(&self.rc.bytes()[..len]);
        if red > 0 && !red_in {
            // the end of the packet, from which the CELT frames after it carry on
            self.code_redundant(n * self.frame - 240, false, 17, red)?;
        }
        if red > 0 {
            out[1 + len..1 + len + red].copy_from_slice(self.red_rc.bytes());
        }
        self.prev_red = red > 0;
        self.silk.as_mut().expect("SILK mode").advance(n * fl);
        if self.mode == OpusMode::Auto {
            self.skip_celt(n * self.frame);
            for s in self.sig.iter_mut() {
                s.drain(..n * self.frame);
            }
        }
        self.frames += n as u64;
        Ok(1 + len + red)
    }

    /// Codes a hybrid frame (SILK, then CELT from 8 kHz up, in one range coder) into `out`, and
    /// returns the packet's length.
    fn code_hybrid(&mut self, out: &mut [u8]) -> Result<usize> {
        let red_in = std::mem::take(&mut self.red_in);
        let red_end = self.to_celt && self.part + 1 == self.per_packet;
        let red = if red_in || red_end { self.red_bytes(self.frame) } else { 0 };
        // CELT starts again at a change of mode, as a decoder's does, unless a redundant frame
        // came before
        let fresh = self.prev_mode.is_some_and(|m| m != OpusMode::Hybrid) && !self.prev_red;
        let s = self.silk.as_mut().expect("hybrid mode");
        let fl = self.frame / 3;
        let need = silk_enc::PAST + fl + silk_enc::LOOKAHEAD;
        let n = self.frame + LOOKAHEAD;
        if s.buf.len() < need || self.sig[0].len() < n {
            return Err(Error::new(ErrorCode::Internal, "Opus: the hybrid frame is not all here"));
        }
        // the high band takes a steady share, its energies and some folded detail; SILK takes the
        // rest, as much as it needs, and the frame ends after both
        let cap = self.bytes - red;
        // the redundancy's flag, position and size
        let red_bits = if red > 0 { 9 } else { 0 };
        self.rc.reset(cap - 1);
        let celt_bps = 3000.0 + 0.25 * (self.bitrate as f64 - 16_000.0).max(0.0);
        let per_frame = |bps: f64| bps * self.frame as f64 / RATE as f64;
        let celt_bits = per_frame(celt_bps) as i32;
        let target = per_frame(self.bitrate as f64 - celt_bps) - 8.0;
        // a copy of the packet's last frame only: the others would travel with their own frames
        let keep = self.part + 1 == self.per_packet;
        let used = s.code(&mut self.rc, fl, 1, target, ((cap - 1) * 8) as i32 - celt_bits - 12 - red_bits, keep);
        let size = (1 + (used + 12 + red_bits + celt_bits) as usize).div_ceil(8).clamp(3, cap);
        self.rc.shrink(size - 1);
        let total = ((size - 1) * 8) as i32;
        if red > 0 {
            self.rc.log(12, 1);
            self.rc.log(1, red_in as u32);
            self.rc.uint(256, (red - 2) as u32);
        } else if used + 37 <= total {
            // no redundant CELT frame
            self.rc.log(12, 0);
        }
        if red_in && red > 0 {
            // from the CELT frames before, which it follows
            self.code_redundant(0, true, 21, red)?;
        }
        if fresh {
            self.celt.flush();
        }
        let intra = self.frames.is_multiple_of(INTRA_PERIOD) || self.dtx.skipped || fresh || red_in;
        let (a, b) = (&self.sig[0][..n], self.sig.get(1).map_or(&[][..], |s| &s[..n]));
        let both = [a, b];
        self.celt.encode_frame(&mut self.rc, &both[..self.channels], self.size, false, intra, None, HYBRID_START);
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // configurations 14 and 15: hybrid, full band, 10 or 20 ms; one frame per packet
        out[0] = ((14 + self.size as u8 - 2) << 3) | (((self.channels == 2) as u8) << 2);
        out[1..size].copy_from_slice(self.rc.bytes());
        self.transients += self.celt.was_transient() as u64;
        if red_end && red > 0 {
            // the end of the frame, from which the CELT frames after it carry on
            self.code_redundant(self.frame - 240, false, 21, red)?;
        }
        if red > 0 {
            out[size..size + red].copy_from_slice(self.red_rc.bytes());
        }
        self.prev_red = red > 0;
        self.silk.as_mut().expect("hybrid mode").advance(fl);
        self.skip_celt(self.frame);
        for s in self.sig.iter_mut() {
            s.drain(..self.frame);
        }
        self.frames += 1;
        Ok(size + red)
    }

    /// Whether a whole frame is waiting to be coded.
    fn frame_ready(&self) -> bool {
        match &self.silk {
            Some(s) => {
                // in automatic mode a packet waits for all its frames, whatever its mode
                let frames = if self.mode == OpusMode::Auto && self.nparts == 0 { self.per_packet } else { self.silk_frames() };
                s.buf.len() >= silk_enc::PAST + frames * self.frame / 3 + silk_enc::LOOKAHEAD
            }
            None => self.sig[0].len() >= self.frame + LOOKAHEAD,
        }
    }

    /// Takes `n` frames of interleaved input from `get`.
    fn take(&mut self, n: usize, get: impl Fn(usize) -> f32) -> Result<()> {
        let ch = self.channels;
        if let Some(s) = &mut self.silk {
            for start in (0..n).step_by(FRAME) {
                s.feed(n.min(start + FRAME) - start, ch, |i| get(start * ch + i))?;
            }
            if !self.feeds_celt_with(self.mode) {
                return Ok(());
            }
        }
        for i in 0..n {
            for c in 0..ch {
                self.push(c, get(i * ch + c));
            }
        }
        Ok(())
    }

    fn push(&mut self, c: usize, x: f32) {
        self.sig[c].push(x - EMPH_COEFF * self.prev[c]);
        self.prev[c] = x;
    }

    /// Codes the frame at the front of the buffered signal into `out`, and returns the packet's length.
    fn code(&mut self, out: &mut [u8]) -> Result<usize> {
        let n = self.frame + LOOKAHEAD;
        let silent = self.sig.iter().all(|s| s[..n].iter().all(|&v| v == 0.0));
        let auto = self.mode == OpusMode::Auto;
        // digital silence needs only the flag
        let size = if silent {
            3
        } else if auto {
            Self::frame_bytes(self.bitrate, self.frame, self.channels)
        } else {
            self.bytes
        };
        // CELT starts again at a change of mode, as a decoder's does, unless a redundant frame
        // came before; the post-filter starts from nothing either way
        let changed = self.prev_mode.is_some_and(|m| m != OpusMode::Celt);
        let fresh = changed && !self.prev_red;
        if fresh {
            self.celt.flush();
        }
        if changed {
            self.pre.restart();
        }
        self.rc.reset(size - 1);
        // the pitch pre-filter needs room for its parameters; off, it fades out
        let enabled = !silent && size - 1 > 12 * self.channels;
        let pitch = match &mut self.sig[..] {
            [a] => self.pre.run(&mut [&mut a[..n]], self.frame, enabled, size - 1),
            [a, b] => self.pre.run(&mut [&mut a[..n], &mut b[..n]], self.frame, enabled, size - 1),
            _ => unreachable!("one or two channels"),
        };
        let (a, b) = (&self.sig[0][..n], self.sig.get(1).map_or(&[][..], |s| &s[..n]));
        let both = [a, b];
        let refs = &both[..self.channels];
        let intra = self.frames.is_multiple_of(INTRA_PERIOD) || self.dtx.skipped || changed;
        self.celt.encode_frame(&mut self.rc, refs, self.size, silent, intra, pitch, 0);
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // configurations 28 to 31: CELT, full band, 2.5 to 20 ms; one frame per packet
        out[0] = ((28 + self.size as u8) << 3) | (((self.channels == 2) as u8) << 2);
        out[1..size].copy_from_slice(self.rc.bytes());
        self.frames += 1;
        self.transients += self.celt.was_transient() as u64;
        for s in self.sig.iter_mut() {
            s.drain(..self.frame);
        }
        if let (true, Some(s)) = (auto, &mut self.silk) {
            s.advance(self.frame / 3);
        }
        self.prev_red = false;
        Ok(size)
    }

    /// Codes the frame at the front of the buffered signal into `out`, in the encoder's mode; in a
    /// pause with discontinuous transmission, the frame is coded (the encoder keeps its state)
    /// but only its TOC byte sent.
    fn code_one(&mut self, out: &mut [u8]) -> Result<usize> {
        if let (OpusMode::Auto, Some(s)) = (self.mode, &self.silk) {
            let at = silk_enc::PAST;
            self.voice.update(&s.buf[at..at + self.silk_frames() * self.frame / 3]);
        }
        let send = !self.dtx.on || {
            let energy = match &self.silk {
                Some(s) => s.energy(self.silk_frames() * self.frame / 3),
                None => {
                    let mut e = 0.0;
                    for i in 0..self.frame {
                        let m = self.sig.iter().map(|s| s[i]).sum::<f32>() / self.channels as f32;
                        self.dtx.deemph = m + EMPH_COEFF * self.dtx.deemph;
                        e += (self.dtx.deemph * self.dtx.deemph) as f64;
                    }
                    e / self.frame as f64
                }
            };
            self.dtx.send(energy, (self.silk_frames() * self.frame) as f64 * 1000.0 / RATE as f64)
        };
        // a decoder keeps SILK's state through the frames it did not get
        if send && self.dtx.skipped {
            if let Some(s) = &mut self.silk {
                s.rewind();
            }
        }
        let len = match self.cur {
            OpusMode::Celt => self.code(out),
            OpusMode::Silk => self.code_silk(out),
            OpusMode::Hybrid | OpusMode::Auto => self.code_hybrid(out),
        }?;
        self.prev_mode = Some(self.cur);
        self.dtx.skipped = !send;
        if send {
            if let (true, Some(s)) = (self.dtx.on, &mut self.silk) {
                s.mark_sent();
            }
            return Ok(len);
        }
        self.skipped += self.silk_frames() as u64;
        Ok(1)
    }

    /// Codes the frame at the front of the buffered signal, and when that completes a packet, adds
    /// the packet to `packets`, `duration` samples long.
    fn emit(&mut self, packets: &mut Vec<Packet>, duration: i64) -> Result<()> {
        let pts = (self.frames as usize - self.nparts) as i64 * self.frame as i64;
        if self.nparts == 0 {
            self.decide();
        }
        if self.per_packet == 1 || self.cur == OpusMode::Silk {
            self.part = 0;
            let mut data = vec![0u8; self.bytes];
            let len = self.code_one(&mut data)?;
            data.truncate(len);
            packets.push(Packet { stream: 0, pts, duration, data });
            return Ok(());
        }
        let k = self.nparts;
        self.part = k;
        let mut parts = std::mem::take(&mut self.parts);
        let r = self.code_one(&mut parts[k * self.bytes..(k + 1) * self.bytes]);
        self.parts = parts;
        self.part_lens[k] = r?;
        self.nparts += 1;
        if self.nparts == self.per_packet {
            let mut data = vec![0u8; self.packet_bytes()];
            let len = self.assemble(&mut data);
            data.truncate(len);
            packets.push(Packet { stream: 0, pts, duration, data });
        }
        Ok(())
    }
}

impl Encoder for OpusEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the Opus encoder received a video frame"));
        };
        if a.sample_rate != RATE || a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed sample rate or channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        let mut packets = Vec::new();
        // a frame at a time, so the SILK buffer never holds more than one and its look-ahead
        for start in (0..n).step_by(self.frame) {
            let m = (n - start).min(self.frame);
            let base = start * self.channels;
            self.take(m, |i| a.samples[base + i] as f32)?;
            while self.frame_ready() {
                self.emit(&mut packets, self.frame_samples() as i64)?;
            }
        }
        self.input += n as u64;
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        if self.input == 0 {
            return Ok(packets);
        }
        // packets until every input sample has come out of the decoder, which is the pre-skip late
        let (frame, packet) = (self.frame, self.frame_samples());
        let skip = self.pre_skip();
        let packets_wanted = (self.input as usize + skip).div_ceil(packet);
        let wanted = (packets_wanted * self.per_packet) as u64;
        while self.frames < wanted {
            while !self.frame_ready() {
                self.take(frame, |_| 0.0)?;
            }
            let last = self.frames + (self.per_packet - self.nparts) as u64 == wanted;
            // the final packet decodes to more than the stream holds; say how much to keep
            let duration = if last { (self.input as usize + skip - (packets_wanted - 1) * packet) as i64 } else { packet as i64 };
            self.emit(&mut packets, duration)?;
        }
        Ok(packets)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    pub fn music(n: usize, channels: usize) -> Vec<f64> {
        let mut s = 12345u32;
        let mut out = Vec::with_capacity(n * channels);
        for i in 0..n {
            let t = i as f64 / 48_000.0;
            let mut noise = |amp: f64| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f64 / (1 << 23) as f64 - 1.0) * amp
            };
            let base = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.15 * (2.0 * std::f64::consts::PI * 1330.0 * t).sin() + 0.08 * (2.0 * std::f64::consts::PI * 5200.0 * t).sin();
            let l = base + noise(0.01);
            out.push(l);
            if channels == 2 {
                out.push(0.7 * base + 0.2 * (2.0 * std::f64::consts::PI * 880.0 * t).sin() + noise(0.01));
            }
        }
        out
    }

    pub fn round_trip(channels: usize, bitrate: u32, seconds: f64) -> (f64, usize) {
        let n = (48_000.0 * seconds) as usize;
        let input = music(n, channels);
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap();
        let mut packets = Vec::new();
        // in awkward chunks, to exercise the buffering
        for chunk in input.chunks(1000 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(channels), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        assert!(out.len() >= input.len(), "decoded {} samples for {}", out.len(), input.len());
        let (mut sig, mut err) = (0f64, 0f64);
        for (a, b) in input.iter().zip(&out) {
            sig += a * a;
            err += (a - b) * (a - b);
        }
        (10.0 * (sig / err.max(1e-30)).log10(), packets.iter().map(|p| p.data.len()).sum())
    }

    #[test]
    fn mono_round_trips_through_our_decoder() {
        let (snr, bytes) = round_trip(1, 64_000, 1.0);
        eprintln!("mono: {snr:.1} dB, {bytes} bytes");
        assert!(snr > 15.0, "{snr}");
    }

    #[test]
    fn stereo_round_trips_through_our_decoder() {
        let (snr, bytes) = round_trip(2, 128_000, 1.0);
        eprintln!("stereo: {snr:.1} dB, {bytes} bytes");
        assert!(snr > 15.0, "{snr}");
    }
}

#[cfg(test)]
mod sweep {
    use super::tests::round_trip as round_trip_pub;
    #[test]
    #[ignore]
    fn bitrate_sweep() {
        for br in [16_000, 32_000, 64_000, 96_000, 128_000, 192_000, 256_000, 320_000, 510_000] {
            let (snr, bytes) = round_trip_pub(2, br, 1.0);
            eprintln!("stereo {br}: {snr:.1} dB {bytes} bytes");
        }
        for br in [16_000, 32_000, 64_000, 128_000, 256_000] {
            let (snr, bytes) = round_trip_pub(1, br, 1.0);
            eprintln!("mono {br}: {snr:.1} dB {bytes} bytes");
        }
    }
}

#[cfg(test)]
mod stress {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    fn encode_all(channels: usize, bitrate: u32, input: &[f64]) -> Vec<Packet> {
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(777 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        packets
    }

    fn decode_all(channels: usize, packets: &[Packet]) -> usize {
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(channels), &params).unwrap();
        let mut n = 0;
        for p in packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                assert!(a.samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0 + 1e-6), "samples out of range");
                n += a.samples.len() / channels;
            }
        }
        n
    }

    #[test]
    fn extreme_signals_and_rates_never_overflow_a_packet() {
        let mut s = 99u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f64 / (1 << 23) as f64 - 1.0
        };
        let n = 48_000 / 2;
        for channels in [1usize, 2] {
            let noise: Vec<f64> = (0..n * channels).map(|_| rnd()).collect();
            let square: Vec<f64> = (0..n * channels).map(|i| if (i / channels / 50) % 2 == 0 { 1.0 } else { -1.0 }).collect();
            let clicks: Vec<f64> = (0..n * channels).map(|i| if (i / channels) % 4801 == 0 { 1.0 } else { 0.0 }).collect();
            let quiet: Vec<f64> = (0..n * channels).map(|_| rnd() * 1e-6).collect();
            let silence = vec![0.0; n * channels];
            for (name, sig) in [("noise", &noise), ("square", &square), ("clicks", &clicks), ("quiet", &quiet), ("silence", &silence)] {
                for bitrate in [6_000u32, 8_000, 16_000, 64_000, 256_000, 510_000] {
                    let packets = encode_all(channels, bitrate, sig);
                    let got = decode_all(channels, &packets);
                    assert!(got >= n, "{name} {channels}ch {bitrate}: {got} samples");
                    if name == "silence" {
                        assert!(packets.iter().all(|p| p.data.len() == 3), "silence is three bytes a frame");
                    }
                }
            }
        }
    }

    #[test]
    fn tiny_inputs() {
        for len in [0usize, 1, 2, 119, 120, 121, 959, 960, 961] {
            let sig: Vec<f64> = (0..len).map(|i| (i as f64 * 0.3).sin() * 0.5).collect();
            let packets = encode_all(1, 64_000, &sig);
            if len == 0 {
                assert!(packets.is_empty());
                continue;
            }
            let got = decode_all(1, &packets);
            assert!(got >= len && got < len + 960, "{len}: {got}");
        }
    }
}

#[cfg(test)]
mod attacks {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    /// A quiet tone with a hard-hit decaying note every 0.3 s: the kind of signal that short blocks are for.
    fn hits(n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                let since = t % 0.3;
                let hit = if since < 0.15 && t >= 0.3 { 0.8 * (-since * 40.0).exp() * (2.0 * std::f64::consts::PI * 1800.0 * since).sin() } else { 0.0 };
                0.003 * (2.0 * std::f64::consts::PI * 200.0 * t).sin() + hit
            })
            .collect()
    }

    #[test]
    fn attacks_use_short_blocks_and_decode_well() {
        let input = hits(48_000 * 2);
        let mut enc = OpusEncoder::new(48_000, 1, 64_000).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(960) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: 48_000, channels: 1, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert!(enc.transients >= 5 && enc.transients <= 12, "{} transient frames for 6 hits", enc.transients);
        let params = StreamParams::Audio { sample_rate: 48_000, channels: 1 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(1), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        let (mut sig, mut err) = (0f64, 0f64);
        for (a, b) in input.iter().zip(&out) {
            sig += a * a;
            err += (a - b) * (a - b);
        }
        let snr = 10.0 * (sig / err).log10();
        eprintln!("hits: {snr:.1} dB, {} short-block frames", enc.transients);
        assert!(snr > 15.0, "{snr}");
    }

    /// Speech through SILK mode and our decoder: the SNR at the best alignment, that alignment
    /// (0 when the pre-skip is right), and the bit rate.
    fn silk_round_trip(channels: usize, ms: f64, bitrate: u32) -> (f64, i64, f64) {
        let (snr, lag, bps, _) = mode_round_trip(OpusMode::Silk, channels, ms, bitrate, false);
        (snr, lag, bps)
    }

    /// Speech (with hiss above 8 kHz if `hiss`) through `mode` and our decoder: the SNR at the best alignment,
    /// that alignment, the bit rate, and the share of the input's energy above 8 kHz that the
    /// output has.
    fn mode_round_trip(mode: OpusMode, channels: usize, ms: f64, bitrate: u32, hiss: bool) -> (f64, i64, f64, f64) {
        let mut mono = super::super::silk_enc::tests::speechlike(96_000, 48_000.0);
        if hiss {
            // sibilance: noise from 10 to 14 kHz, in bursts
            let mut s = 99u32;
            let mut prev = 0f32;
            for (i, v) in mono.iter_mut().enumerate() {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                let w = (s >> 9) as f32 / (1 << 23) as f32 - 0.5;
                let hp = w - prev;
                prev = w;
                let burst = if (i / 4800) % 3 == 1 { 0.08 } else { 0.0 };
                *v += burst * hp * (2.0 * std::f32::consts::PI * 12_000.0 * i as f32 / 48_000.0).cos();
            }
        }
        let input: Vec<f64> = mono.iter().flat_map(|&v| std::iter::repeat_n(v as f64, channels)).collect();
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap().with_mode(mode).unwrap().with_frame_ms(ms).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        // configurations by mode: SILK has frames of 10 to 60 ms, the others' longer packets hold
        // 20 ms frames
        let (config, code) = match mode {
            OpusMode::Silk => (8 + [10.0, 20.0, 40.0, 60.0].iter().position(|&m| m == ms).unwrap() as u8, 0),
            _ => (if mode == OpusMode::Hybrid { 14 } else { 30 } + (ms.min(20.0) == 20.0) as u8, if ms > 20.0 { 3 } else { 0 }),
        };
        for p in &packets {
            assert!(p.data.len() <= enc.packet_bytes(), "{} > {}", p.data.len(), enc.packet_bytes());
            assert_eq!((p.data[0] >> 3, p.data[0] & 3), (config, code), "{mode:?} {ms} ms");
            if code == 3 {
                assert_eq!(p.data[1] & 63, (ms / 20.0) as u8);
            }
        }
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples.iter().step_by(channels).copied());
            }
        }
        assert!(out.len() >= mono.len(), "decoded {} samples for {}", out.len(), mono.len());
        let snr_at = |lag: i64| {
            let (mut sig, mut err) = (0f64, 0f64);
            for n in 100..mono.len() - 100 {
                let (a, b) = (mono[n] as f64, out[(n as i64 + lag) as usize]);
                sig += a * a;
                err += (a - b) * (a - b);
            }
            10.0 * (sig / err.max(1e-30)).log10()
        };
        let lag = (-30..=30).max_by(|&a, &b| snr_at(a).partial_cmp(&snr_at(b)).unwrap()).unwrap();
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        // energy above 9 kHz, in and out: a windowed-sinc high-pass
        let taps: Vec<f64> = (-64i32..=64)
            .map(|k| {
                let t = k as f64;
                let lp = if k == 0 { 2.0 * 9_000.0 / 48_000.0 } else { (2.0 * std::f64::consts::PI * 9_000.0 / 48_000.0 * t).sin() / (std::f64::consts::PI * t) };
                let w = 0.5 + 0.5 * (std::f64::consts::PI * t / 65.0).cos();
                (if k == 0 { 1.0 } else { 0.0 }) - lp * w
            })
            .collect();
        let high = |x: &dyn Fn(usize) -> f64| (200..mono.len() - 200).map(|n| taps.iter().enumerate().map(|(k, c)| c * x(n + k - 64)).sum::<f64>().powi(2)).sum::<f64>();
        let hf = high(&|n| out[(n as i64 + lag) as usize]) / high(&|n| mono[n] as f64);
        (snr_at(lag), lag, bytes as f64 * 8.0 / 2.0, hf)
    }

    #[test]
    fn hybrid_mode_round_trips_with_the_high_band() {
        for (ms, rate) in [(20.0, 24_000), (10.0, 32_000)] {
            let (snr, lag, bps, hf) = mode_round_trip(OpusMode::Hybrid, 1, ms, rate, true);
            let (silk_snr, _, _, silk_hf) = mode_round_trip(OpusMode::Silk, 1, ms, rate, true);
            eprintln!("hybrid {ms} ms at {rate}: {snr:.1} dB at lag {lag}, {:.1} kb/s, high band {hf:.2} (SILK alone {silk_snr:.1} dB, {silk_hf:.2})", bps / 1000.0);
            assert_eq!(lag, 0, "the two layers are out of step, or the pre-skip is off");
            assert!(snr > 8.0, "{snr}");
            assert!((bps / rate as f64 - 1.0).abs() < 0.2, "{bps} for {rate}");
            assert!(hf > 0.3 && hf > 3.0 * silk_hf, "{hf} vs {silk_hf}");
        }
        // the same in both channels: stereo with the side not coded
        let (snr, lag, _, _) = mode_round_trip(OpusMode::Hybrid, 2, 20.0, 24_000, true);
        assert!(snr > 8.0 && lag == 0, "{snr} {lag}");
    }

    #[test]
    fn stereo_silk_and_hybrid_keep_both_channels() {
        let x = super::super::silk_enc::tests::speechlike(96_000, 48_000.0);
        // left the signal; right a quieter copy 2 ms later and a tone
        let input: Vec<f32> = (0..x.len())
            .flat_map(|i| [x[i], 0.6 * x[i.saturating_sub(96)] + 0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()])
            .collect();
        let panned: Vec<f32> = x.iter().flat_map(|&v| [v, 0.4 * v]).collect();
        for (mode, rate) in [(OpusMode::Silk, 32_000), (OpusMode::Hybrid, 40_000)] {
            let run = |input: &[f32]| {
                let mut enc = OpusEncoder::new(48_000, 2, rate).unwrap().with_mode(mode).unwrap();
                let params = StreamParams::Audio { sample_rate: 48_000, channels: 2 };
                let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
                let n = enc.frame_samples();
                let (mut buf, mut pcm) = (vec![0u8; enc.packet_bytes()], vec![0f32; 2 * n]);
                let (mut out, mut bits) = (Vec::new(), 0);
                for f in input.chunks_exact(2 * n) {
                    let len = enc.encode_frame_into(f, &mut buf).unwrap();
                    assert_eq!(buf[0] & 4, 4, "the TOC says stereo");
                    bits += 8 * len;
                    let got = dec.decode_into(&buf[..len], &mut pcm).unwrap();
                    out.extend_from_slice(&pcm[..2 * got]);
                }
                let snr = |c: usize| {
                    let (mut sig, mut err) = (0f64, 0f64);
                    for i in 500..input.len() / 2 - 500 {
                        let (a, b) = (input[2 * i + c] as f64, out[2 * i + c] as f64);
                        sig += a * a;
                        err += (a - b) * (a - b);
                    }
                    10.0 * (sig / err.max(1e-30)).log10()
                };
                ([snr(0), snr(1)], bits)
            };
            let (snr, bits) = run(&input);
            let (same_snr, same_bits) = run(&panned);
            let (mono_snr, _, mono_bps, _) = mode_round_trip(mode, 1, 20.0, rate, false);
            eprintln!("{mode:?} stereo at {rate}: {:.1} {:.1} dB, {bits} bits; panned: {:.1} {:.1} dB, {same_bits} bits; mono {mono_snr:.1} dB, {mono_bps:.0} b/s", snr[0], snr[1], same_snr[0], same_snr[1]);
            // the right is quieter, and has the same noise as the left: mid and side share the bits
            assert!(snr[0] > 11.0 && snr[1] > 8.5, "{snr:?}");
            assert!((bits as f64 / 2.0 / rate as f64 - 1.0).abs() < 0.3, "{bits}");
            // a panned mono signal: the side is all predicted, and costs only its flags
            assert!(same_snr[0] > mono_snr - 1.0 && same_snr[1] > mono_snr - 1.0, "{same_snr:?} vs {mono_snr}");
            assert!((same_bits as f64) < 1.1 * 2.0 * mono_bps, "{same_bits} vs {mono_bps}");
        }
    }

    #[test]
    fn packets_of_40_and_60_ms_hold_two_or_three_frames() {
        for mode in [OpusMode::Celt, OpusMode::Silk, OpusMode::Hybrid] {
            let rate = if mode == OpusMode::Celt { 48_000 } else { 20_000 };
            let (snr20, _, bps20, _) = mode_round_trip(mode, 1, 20.0, rate, false);
            for ms in [40.0, 60.0] {
                let (snr, lag, bps, _) = mode_round_trip(mode, 1, ms, rate, false);
                eprintln!("{mode:?} {ms} ms at {rate}: {snr:.1} dB at lag {lag}, {:.1} kb/s (20 ms: {snr20:.1} dB, {:.1} kb/s)", bps / 1000.0, bps20 / 1000.0);
                assert_eq!(lag, 0);
                assert!(snr > snr20 - 1.0, "{snr} vs {snr20}");
                assert!(bps <= bps20 * 1.05, "{bps} vs {bps20}");
            }
        }
        // a frame at a time: 60 ms in, one packet out, and a buffer of packet_bytes is enough
        let mut enc = OpusEncoder::new(48_000, 2, 64_000).unwrap().with_frame_ms(60.0).unwrap();
        assert_eq!(enc.frame_samples(), 2880);
        let x: Vec<f32> = super::tests::music(2880 * 4, 2).iter().map(|&v| v as f32).collect();
        let mut buf = vec![0u8; enc.packet_bytes()];
        for f in x.as_chunks::<{ 2880 * 2 }>().0.iter() {
            let n = enc.encode_frame_into(f, &mut buf).unwrap();
            assert_eq!((buf[0] >> 3, buf[0] & 3, buf[1] & 63), (31, 3, 3));
            assert!(n <= enc.packet_bytes());
        }
        assert!(OpusEncoder::new(48_000, 1, 64_000).unwrap().with_frame_ms(30.0).is_err());
    }

    /// Speech, a pause of faint noise, and speech again, through the encoder with or without
    /// DTX and our decoder: the SNR of each stretch of speech, the bits, the frames skipped, and
    /// the level of the pause's output in dB.
    fn dtx_round_trip(mode: OpusMode, ms: f64, dtx: bool) -> ([f64; 2], usize, u64, f64) {
        let speech = super::super::silk_enc::tests::speechlike(48_000, 48_000.0);
        let mut s = 7u32;
        let pause: Vec<f32> = (0..72_000)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 9) as f32 / (1 << 23) as f32 - 0.5) * 0.001
            })
            .collect();
        let input: Vec<f32> = speech.iter().chain(&pause).chain(&speech).copied().collect();
        let rate = if mode == OpusMode::Celt { 48_000 } else { 24_000 };
        let mut enc = OpusEncoder::new(48_000, 1, rate).unwrap().with_mode(mode).unwrap().with_frame_ms(ms).unwrap().with_dtx(dtx);
        let params = StreamParams::Audio { sample_rate: 48_000, channels: 1 };
        let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
        let n = enc.frame_samples();
        let mut buf = vec![0u8; enc.packet_bytes()];
        let mut pcm = vec![0f32; n];
        let (mut out, mut bytes) = (Vec::new(), 0);
        for f in input.chunks_exact(n) {
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            bytes += len;
            let got = dec.decode_into(&buf[..len], &mut pcm).unwrap();
            out.extend_from_slice(&pcm[..got]);
        }
        // the decoder drops the encoder's delay as the pre-skip
        let lag = 0;
        let snr = |r: std::ops::Range<usize>| {
            let (mut sig, mut err) = (0f64, 0f64);
            for i in r {
                let (a, b) = (input[i] as f64, out[i + lag] as f64);
                sig += a * a;
                err += (a - b) * (a - b);
            }
            10.0 * (sig / err.max(1e-30)).log10()
        };
        let gap = 48_000 + 24_000..48_000 + 72_000;
        let level = gap.clone().map(|i| (out[i + lag] as f64).powi(2)).sum::<f64>() / gap.len() as f64;
        ([snr(500..47_000), snr(120_500..input.len() - 2 * n)], bytes * 8, enc.skipped, 10.0 * (level + 1e-20).log10())
    }

    #[test]
    fn dtx_skips_the_frames_of_a_pause() {
        for (mode, ms) in [(OpusMode::Celt, 20.0), (OpusMode::Celt, 10.0), (OpusMode::Silk, 20.0), (OpusMode::Hybrid, 20.0), (OpusMode::Silk, 60.0), (OpusMode::Celt, 40.0)] {
            let (snr0, bits0, skipped0, _) = dtx_round_trip(mode, ms, false);
            let (snr, bits, skipped, level) = dtx_round_trip(mode, ms, true);
            eprintln!("{mode:?} {ms} ms: {skipped} skipped, {bits} bits for {bits0}, speech {:.1} {:.1} dB for {:.1} {:.1}, pause at {level:.1} dB", snr[0], snr[1], snr0[0], snr0[1]);
            assert_eq!(skipped0, 0);
            // a pause of 1.5 s: 200 ms before skipping, and a frame sent every 400 ms
            let frames = (1500.0 / ms.min(20.0)) as u64;
            assert!(skipped > frames * 6 / 10 && skipped < frames * 9 / 10, "{skipped} of {frames}");
            assert!(bits < bits0 * 8 / 10, "{bits} vs {bits0}");
            assert!(snr[0] > snr0[0] - 0.5 && snr[1] > snr0[1] - 1.5, "{snr:?} vs {snr0:?}");
            // the noise was at -71 dB; what comes out of the pause is no louder
            assert!(level < -60.0, "{level}");
        }
    }

    /// Speech through the encoder with FEC for `loss` percent (0: off), and our decoder with
    /// every tenth packet lost: the SNR of the left channel with nothing lost, with the lost
    /// packets concealed, and with them rebuilt from the next packet's copy; and the bits.
    fn fec_round_trip(mode: OpusMode, channels: usize, ms: f64, rate: u32, loss: u32) -> [f64; 4] {
        let x = super::super::silk_enc::tests::speechlike(96_000, 48_000.0);
        let input: Vec<f32> = (0..x.len()).flat_map(|i| [x[i], 0.6 * x[i.saturating_sub(96)]].into_iter().take(channels)).collect();
        let mut enc = OpusEncoder::new(48_000, channels, rate).unwrap().with_mode(mode).unwrap().with_frame_ms(ms).unwrap().with_fec(loss);
        let n = enc.frame_samples();
        let mut buf = vec![0u8; enc.packet_bytes()];
        let packets: Vec<Vec<u8>> = input
            .chunks_exact(n * channels)
            .map(|f| {
                let len = enc.encode_frame_into(f, &mut buf).unwrap();
                buf[..len].to_vec()
            })
            .collect();
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let lost = |k: usize| k % 10 == 5 && k + 1 < packets.len();
        let mut snrs = [0.0; 4];
        for (way, snr) in snrs.iter_mut().take(3).enumerate() {
            let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
            let mut pcm = vec![0f32; n * channels];
            let mut out: Vec<f32> = Vec::new();
            for (k, p) in packets.iter().enumerate() {
                let got = match way {
                    // a frame at a time
                    1 if lost(k) => {
                        let mut got = 0;
                        while got < n {
                            got += dec.conceal_into(&mut pcm[got * channels..]).unwrap();
                        }
                        got
                    }
                    2 if lost(k) => dec.decode_fec_into(&packets[k + 1], &mut pcm).unwrap(),
                    _ => dec.decode_into(p, &mut pcm).unwrap(),
                };
                out.extend(pcm[..got * channels].iter().step_by(channels).copied());
            }
            let (mut sig, mut err) = (0f64, 0f64);
            for i in 500..out.len().min(x.len()) - 500 {
                sig += (x[i] as f64).powi(2);
                err += (x[i] as f64 - out[i] as f64).powi(2);
            }
            *snr = 10.0 * (sig / err.max(1e-30)).log10();
        }
        snrs[3] = packets.iter().map(|p| 8.0 * p.len() as f64).sum();
        snrs
    }

    #[test]
    fn fec_rebuilds_a_lost_packet_from_the_next() {
        for (mode, channels, ms, rate) in [(OpusMode::Silk, 1, 20.0, 24_000), (OpusMode::Silk, 1, 10.0, 24_000), (OpusMode::Silk, 1, 60.0, 24_000), (OpusMode::Silk, 2, 20.0, 32_000), (OpusMode::Hybrid, 1, 20.0, 32_000), (OpusMode::Hybrid, 1, 40.0, 32_000)] {
            let off = fec_round_trip(mode, channels, ms, rate, 0);
            let [all, concealed, rebuilt, bits] = fec_round_trip(mode, channels, ms, rate, 10);
            eprintln!("{mode:?} {channels} ch {ms} ms at {rate}: nothing lost {all:.1} dB ({:.1} without FEC), a tenth lost: concealed {concealed:.1}, rebuilt {rebuilt:.1}; {bits} bits ({} without)", off[0], off[3]);
            // a frame rebuilt from its copy is closer than one concealed: far closer for 20 ms
            // frames; of a 40 ms hybrid packet, only the last frame has a copy
            assert!(rebuilt > concealed + if ms == 20.0 && channels == 1 { 5.0 } else { 0.5 }, "{rebuilt} vs {concealed}");
            // without FEC there is nothing to rebuild from
            assert!((off[2] - off[1]).abs() < 0.1, "{off:?}");
            // the copies come out of the same rate: up to 60 % of an active frame's bits, mostly
            // their side information
            assert!(all > off[0] - 7.0 && (bits / off[3] - 1.0).abs() < 0.1, "{all} vs {}, {bits} vs {}", off[0], off[3]);
        }
    }

    #[test]
    fn silk_mode_round_trips_through_our_decoder() {
        for (channels, ms, rate) in [(1, 20.0, 12_000), (1, 20.0, 20_000), (1, 10.0, 16_000), (2, 20.0, 16_000)] {
            let (snr, lag, bps) = silk_round_trip(channels, ms, rate);
            eprintln!("SILK {channels} ch {ms} ms at {rate}: {snr:.1} dB at lag {lag}, {:.1} kb/s", bps / 1000.0);
            assert_eq!(lag, 0, "the pre-skip is off");
            assert!(snr > 8.0, "{snr}");
            assert!((bps / rate as f64 - 1.0).abs() < 0.3, "{bps} for {rate}");
        }
    }

    #[test]
    fn silk_mode_keeps_its_rate_on_clipped_input() {
        let mut enc = OpusEncoder::new(48_000, 1, 16_000).unwrap().with_mode(OpusMode::Silk).unwrap();
        let x: Vec<f32> = super::super::silk_enc::tests::speechlike(96_000, 48_000.0).iter().map(|v| v * 4.0).collect();
        let mut buf = vec![0u8; enc.packet_bytes()];
        let mut bytes = 0;
        for f in x.chunks_exact(enc.frame_samples()) {
            bytes += enc.encode_frame_into(f, &mut buf).unwrap();
        }
        let bps = bytes as f64 * 8.0 / 2.0;
        eprintln!("clipped: {:.1} kb/s", bps / 1000.0);
        assert!(bps < 16_000.0 * 1.4, "{bps}");
    }

    #[test]
    fn silk_mode_frame_at_a_time() {
        let mut enc = OpusEncoder::new(48_000, 1, 16_000).unwrap().with_mode(OpusMode::Silk).unwrap();
        assert!(OpusEncoder::new(48_000, 1, 16_000).unwrap().with_frame_ms(5.0).unwrap().with_mode(OpusMode::Silk).is_err());
        assert_eq!(enc.mode(), OpusMode::Silk);
        let x = super::super::silk_enc::tests::speechlike(48_000, 48_000.0);
        let mut buf = vec![0u8; enc.packet_bytes()];
        let mut sizes = Vec::new();
        for f in x.chunks_exact(enc.frame_samples()) {
            sizes.push(enc.encode_frame_into(f, &mut buf).unwrap());
        }
        assert_eq!(sizes.len(), 50);
        assert!(sizes.iter().all(|&n| n > 2 && n <= enc.packet_bytes()), "{sizes:?}");
    }

    /// The mode of a packet, from its TOC.
    fn toc_mode(b: u8) -> OpusMode {
        match b >> 3 {
            0..=11 => OpusMode::Silk,
            12..=15 => OpusMode::Hybrid,
            _ => OpusMode::Celt,
        }
    }

    /// `x` (mono) through automatic mode at `rates[k]` for second `k` (the last for the rest):
    /// each packet's mode, the SNR of each 20 ms of the output, and the SNR over all of it.
    fn auto_round_trip(x: &[f32], ms: f64, rates: &[u32], signal: OpusSignal) -> (Vec<OpusMode>, Vec<f64>, f64) {
        let mut enc = OpusEncoder::new(48_000, 1, rates[0]).unwrap().with_mode(OpusMode::Auto).unwrap().with_frame_ms(ms).unwrap().with_signal(signal);
        let n = enc.frame_samples();
        let params = StreamParams::Audio { sample_rate: 48_000, channels: 1 };
        let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
        let (mut modes, mut out) = (Vec::new(), Vec::new());
        let mut pcm = vec![0f32; n];
        for (k, f) in x.chunks_exact(n).enumerate() {
            let rate = rates[(k * n / 48_000).min(rates.len() - 1)];
            if rate != enc.bitrate() {
                enc.set_bitrate(rate).unwrap();
            }
            let mut buf = vec![0u8; enc.packet_bytes()];
            let len = enc.encode_frame_into(f, &mut buf).unwrap();
            assert_eq!(toc_mode(buf[0]), enc.current_mode());
            modes.push(enc.current_mode());
            let got = dec.decode_into(&buf[..len], &mut pcm).unwrap();
            out.extend_from_slice(&pcm[..got]);
        }
        let snr = |a: usize, b: usize| {
            let (mut sig, mut err) = (0f64, 0f64);
            for i in a..b {
                sig += (x[i] as f64).powi(2);
                err += (x[i] as f64 - out[i] as f64).powi(2);
            }
            // a window of near silence counts as clean
            if sig < 1e-6 * (b - a) as f64 { 99.0 } else { 10.0 * (sig / err.max(1e-30)).log10() }
        };
        let end = out.len().min(x.len()) - 500;
        let windows = (0..end / 960).map(|w| snr(w * 960, (w + 1) * 960)).collect();
        (modes, windows, snr(500, end))
    }

    /// Speech-like input, music, and speech again, `seconds` of each.
    fn speech_music_speech(seconds: f64) -> Vec<f32> {
        let n = (seconds * 48_000.0) as usize;
        let speech = super::super::silk_enc::tests::speechlike(n, 48_000.0);
        let music: Vec<f32> = super::tests::music(n, 1).iter().map(|&v| v as f32).collect();
        [&speech[..], &music[..], &speech[..]].concat()
    }

    #[test]
    fn auto_mode_picks_by_rate_and_signal() {
        // long enough for the guess at the signal to settle
        let x = super::super::silk_enc::tests::speechlike(6 * 48_000, 48_000.0);
        let m: Vec<f32> = super::tests::music(6 * 48_000, 1).iter().map(|&v| v as f32).collect();
        for (input, signal, rate, want) in [
            (&x, OpusSignal::Voice, 12_000, OpusMode::Silk),
            (&x, OpusSignal::Voice, 32_000, OpusMode::Hybrid),
            (&x, OpusSignal::Voice, 80_000, OpusMode::Celt),
            (&m, OpusSignal::Music, 32_000, OpusMode::Celt),
            (&m, OpusSignal::Music, 12_000, OpusMode::Silk),
            (&x, OpusSignal::Auto, 32_000, OpusMode::Hybrid),
            (&m, OpusSignal::Auto, 32_000, OpusMode::Celt),
        ] {
            let (modes, _, snr) = auto_round_trip(input, 20.0, &[rate], signal);
            let (fixed, _, fixed_bps, _) = if input == &x && want != OpusMode::Celt { mode_round_trip(want, 1, 20.0, rate, false) } else { (0.0, 0, 0.0, 0.0) };
            eprintln!("{signal:?} at {rate}: {:?} at the end, {snr:.1} dB (fixed {fixed:.1} dB, {fixed_bps:.0} b/s)", modes.last());
            assert!(modes[modes.len() * 2 / 3..].iter().all(|&m| m == want), "{signal:?} {rate}: {modes:?}");
        }
    }

    #[test]
    fn auto_mode_switches_with_the_signal_and_the_rate() {
        // speech, music and speech at 40 kb/s: hybrid, CELT, hybrid
        let x = speech_music_speech(4.0);
        for ms in [20.0, 10.0, 60.0] {
            let (modes, windows, snr) = auto_round_trip(&x, ms, &[40_000], OpusSignal::Auto);
            let changes: Vec<(usize, OpusMode)> = modes.windows(2).enumerate().filter(|(_, w)| w[0] != w[1]).map(|(i, w)| (i + 1, w[1])).collect();
            eprintln!("{ms} ms: {changes:?}, {snr:.1} dB");
            assert_eq!(changes.iter().map(|c| c.1).collect::<Vec<_>>(), [OpusMode::Hybrid, OpusMode::Celt, OpusMode::Hybrid], "{ms} ms");
            // (the threshold was 17 dB before the resampler matched libopus's: 20 ms now gives 17.4 dB
            // where it gave 13.5, and 10 ms gives 16.7)
            assert!(snr > 16.0, "{ms} ms: {snr}");
            // no click at a change: each 20 ms around it decodes as well as speech does
            for &(k, _) in &changes {
                let w = (k as f64 * ms / 20.0) as usize;
                let near = &windows[w - 4..w + 5];
                eprintln!("  {:?}", near.iter().map(|w| *w as i32).collect::<Vec<_>>());
                assert!(near.iter().all(|&v| v > 5.0), "{ms} ms at {k}: {near:?}");
            }
        }
        // speech at 12, 24, 80 and 12 kb/s: SILK, hybrid, CELT, SILK
        let x = super::super::silk_enc::tests::speechlike(4 * 48_000, 48_000.0);
        let (modes, windows, snr) = auto_round_trip(&x, 20.0, &[12_000, 24_000, 80_000, 12_000], OpusSignal::Voice);
        let changes: Vec<(usize, OpusMode)> = modes.windows(2).enumerate().filter(|(_, w)| w[0] != w[1]).map(|(i, w)| (i + 1, w[1])).collect();
        eprintln!("rates: {changes:?}, {snr:.1} dB");
        assert_eq!(changes.iter().map(|c| (c.0 / 50, c.1)).collect::<Vec<_>>(), [(1, OpusMode::Hybrid), (2, OpusMode::Celt), (3, OpusMode::Silk)]);
        for &(k, _) in &changes {
            let near = &windows[k - 4..k + 5];
            eprintln!("  {:?}", near.iter().map(|w| *w as i32).collect::<Vec<_>>());
            assert!(near[4..].iter().all(|&v| v > 5.0), "at {k}: {near:?}");
        }
    }
}
