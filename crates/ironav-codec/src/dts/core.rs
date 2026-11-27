//! The core substream: the frame header, the side information, and the subband audio, in the
//! fixed-point arithmetic of FFmpeg's `libavcodec/dca_core.c` (23-bit subband samples), followed by
//! the float synthesis filter bank.

use super::bits::Bits;
use super::huff::{self, GROUP_SIZE};
use super::synth::{lfe_fir, qmf_synth, ChannelSynth};
use super::tables::*;
use ironav_core::{Error, ErrorCode, Result};

/// Primary channels of a core frame at most (audio modes 0 to 9).
const MAX_CH: usize = 5;
const BANDS: usize = 32;
const MAX_BLOCKS: usize = 128;
/// Samples of history kept before each band's samples for the ADPCM predictor.
const HIST: usize = 4;
const STRIDE: usize = HIST + MAX_BLOCKS;
const LFE_HIST: usize = 8;
const ABITS_MAX: u32 = 26;

/// Primary channels of each audio mode.
const CHANNELS: [usize; 10] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5];
const SAMPLE_RATES: [u32; 16] = [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000];
const BITS_PER_SAMPLE: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];
const QUANT_INDEX_SEL_NBITS: [u32; 10] = [1, 2, 2, 2, 2, 3, 3, 3, 3, 3];
const BLOCK_CODE_NBITS: [u32; 7] = [7, 10, 12, 13, 15, 17, 19];

/// Speakers, numbered as FFmpeg's `DCASpeaker`.
const SPK_C: usize = 0;
const SPK_L: usize = 1;
const SPK_R: usize = 2;
const SPK_LS: usize = 3;
const SPK_RS: usize = 4;
const SPK_LFE: usize = 5;
const SPK_CS: usize = 6;

/// The speaker of each primary channel in each audio mode (`prm_ch_to_spkr_map`).
const PRM_CH_TO_SPKR: [&[usize]; 10] = [
    &[SPK_C],
    &[SPK_L, SPK_R],
    &[SPK_L, SPK_R],
    &[SPK_L, SPK_R],
    &[SPK_L, SPK_R],
    &[SPK_C, SPK_L, SPK_R],
    &[SPK_L, SPK_R, SPK_CS],
    &[SPK_C, SPK_L, SPK_R, SPK_CS],
    &[SPK_L, SPK_R, SPK_LS, SPK_RS],
    &[SPK_C, SPK_L, SPK_R, SPK_LS, SPK_RS],
];

/// The order the channels are output in: FFmpeg's default channel order (WAVE order) of the
/// speakers a core can have: FL FR FC LFE BC SL SR.
const OUTPUT_ORDER: [usize; 7] = [SPK_L, SPK_R, SPK_C, SPK_LFE, SPK_CS, SPK_LS, SPK_RS];

/// What a core frame's header says (ETSI TS 102 114 section 5.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Header {
    pub crc_present: bool,
    /// PCM sample blocks of 32 samples in the frame.
    pub npcmblocks: usize,
    /// Bytes in the core frame (of the 16-bit big-endian form).
    pub frame_size: usize,
    pub audio_mode: usize,
    pub sample_rate: u32,
    pub bit_rate_code: usize,
    pub drc_present: bool,
    pub ext_audio_present: bool,
    pub sync_ssf: bool,
    /// 0 none, 1 with the 128 times interpolation, 2 with 64 times.
    pub lfe: usize,
    pub predictor_history: bool,
    pub filter_perfect: bool,
    pub sumdiff_front: bool,
    pub sumdiff_surround: bool,
    /// Bits the header takes.
    pub header_bits: usize,
}

impl Header {
    /// The primary channels, without the LFE channel.
    pub fn primary_channels(&self) -> usize {
        CHANNELS[self.audio_mode]
    }

    /// All channels the core decodes to.
    pub fn channels(&self) -> usize {
        CHANNELS[self.audio_mode] + (self.lfe != 0) as usize
    }

    /// The bit rate in bits per second, or 0 for an open, variable or lossless stream.
    pub fn bit_rate(&self) -> u32 {
        let r = BIT_RATES[self.bit_rate_code];
        if r > 3 {
            r
        } else {
            0
        }
    }

    pub fn samples(&self) -> usize {
        self.npcmblocks * 32
    }
}

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("DTS: {msg}"))
}

/// The bytes of a core frame header (the 16-bit big-endian form), at least this many to parse.
pub(super) const HEADER_BYTES: usize = 18;

/// Parses a core frame header from the start of `data`, which must begin with the 32-bit sync word.
pub(super) fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < HEADER_BYTES {
        return Err(bad("the frame header is cut short"));
    }
    let mut r = Bits::new(data);
    if r.bits(32) != 0x7FFE8001 {
        return Err(bad("no sync word"));
    }
    r.skip(1); // frame type
    if r.bits(5) + 1 != 32 {
        return Err(Error::new(ErrorCode::UnsupportedCodec, "DTS: deficit samples are not supported"));
    }
    let crc_present = r.bit();
    let npcmblocks = r.bits(7) as usize + 1;
    if npcmblocks & 7 != 0 {
        return Err(bad("unsupported number of PCM sample blocks"));
    }
    let frame_size = r.bits(14) as usize + 1;
    if frame_size < 96 {
        return Err(bad("invalid core frame size"));
    }
    let audio_mode = r.bits(6) as usize;
    if audio_mode >= CHANNELS.len() {
        return Err(Error::new(ErrorCode::UnsupportedCodec, format!("DTS: unsupported audio channel arrangement {audio_mode}")));
    }
    let sample_rate = SAMPLE_RATES[r.bits(4) as usize];
    if sample_rate == 0 {
        return Err(bad("invalid core audio sampling frequency"));
    }
    let bit_rate_code = r.bits(5) as usize;
    if r.bit() {
        return Err(bad("reserved bit set"));
    }
    let drc_present = r.bit();
    r.skip(3); // time stamp, auxiliary data, HDCD
    r.skip(3); // extension audio descriptor
    let ext_audio_present = r.bit();
    let sync_ssf = r.bit();
    let lfe = r.bits(2) as usize;
    if lfe == 3 {
        return Err(bad("invalid low frequency effects flag"));
    }
    let predictor_history = r.bit();
    if crc_present {
        r.skip(16);
    }
    let filter_perfect = r.bit();
    r.skip(4 + 2); // encoder revision, copy history
    if BITS_PER_SAMPLE[r.bits(3) as usize] == 0 {
        return Err(bad("invalid source PCM resolution"));
    }
    let sumdiff_front = r.bit();
    let sumdiff_surround = r.bit();
    r.skip(4); // dialog normalisation
    Ok(Header {
        crc_present,
        npcmblocks,
        frame_size,
        audio_mode,
        sample_rate,
        bit_rate_code,
        drc_present,
        ext_audio_present,
        sync_ssf,
        lfe,
        predictor_history,
        filter_perfect,
        sumdiff_front,
        sumdiff_surround,
        header_bits: r.pos,
    })
}

fn clip23(a: i64) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1) as i32
}

/// FFmpeg's `norm__`: round and shift, or truncate to 32 bits when there is nothing to shift.
fn norm(a: i64, bits: i32) -> i32 {
    if bits > 0 {
        ((a + (1i64 << (bits - 1))) >> bits) as i32
    } else {
        a as i32
    }
}

/// `ff_dca_core_dequantize`: scales quantised samples into `out` by the step size and scale
/// factor, keeping 22 bits of the scale's resolution.
fn dequantize(out: &mut [i32], input: &[i32], step_size: i32, scale: i32) {
    let mut step_scale = step_size as i64 * scale as i64;
    let mut shift = 0;
    if step_scale > 1 << 23 {
        shift = (step_scale >> 23).ilog2() as i32 + 1;
        step_scale >>= shift;
    }
    for (o, &i) in out.iter_mut().zip(input) {
        *o = clip23(norm(i as i64 * step_scale, 22 - shift) as i64);
    }
}

/// The side information of the frame being decoded.
struct Side {
    nsubframes: usize,
    nchannels: usize,
    nsubbands: [usize; MAX_CH],
    vq_start: [usize; MAX_CH],
    joint_index: [usize; MAX_CH],
    transition_sel: [usize; MAX_CH],
    scale_sel: [usize; MAX_CH],
    bitalloc_sel: [usize; MAX_CH],
    quant_sel: [[usize; 10]; MAX_CH],
    scale_adj: [[i32; 10]; MAX_CH],
    nsubsubframes: [usize; 16],
    pred_mode: [[bool; BANDS]; MAX_CH],
    pred_vq: [[usize; BANDS]; MAX_CH],
    bitalloc: [[u32; BANDS]; MAX_CH],
    transition: [[usize; BANDS]; MAX_CH],
    scale: [[[i32; 2]; BANDS]; MAX_CH],
    joint_sel: [usize; MAX_CH],
    joint_scale: [[i32; BANDS]; MAX_CH],
}

impl Side {
    fn new() -> Side {
        Side {
            nsubframes: 0,
            nchannels: 0,
            nsubbands: [0; MAX_CH],
            vq_start: [0; MAX_CH],
            joint_index: [0; MAX_CH],
            transition_sel: [0; MAX_CH],
            scale_sel: [0; MAX_CH],
            bitalloc_sel: [0; MAX_CH],
            quant_sel: [[0; 10]; MAX_CH],
            scale_adj: [[0; 10]; MAX_CH],
            nsubsubframes: [0; 16],
            pred_mode: [[false; BANDS]; MAX_CH],
            pred_vq: [[0; BANDS]; MAX_CH],
            bitalloc: [[0; BANDS]; MAX_CH],
            transition: [[0; BANDS]; MAX_CH],
            scale: [[[0; 2]; BANDS]; MAX_CH],
            joint_sel: [0; MAX_CH],
            joint_scale: [[0; BANDS]; MAX_CH],
        }
    }
}

/// The decoder of core frames: holds what carries over from one frame to the next.
pub(super) struct Core {
    /// Subband samples, `[channel][band]`, each with `HIST` samples of history before them.
    sb: Vec<i32>,
    /// Decimated LFE samples, with `LFE_HIST` of history before them.
    lfe: Vec<i32>,
    synth: Vec<ChannelSynth>,
    side: Box<Side>,
}

fn at(ch: usize, band: usize) -> usize {
    (ch * BANDS + band) * STRIDE
}

impl Core {
    pub(super) fn new() -> Core {
        Core { sb: vec![0; MAX_CH * BANDS * STRIDE], lfe: vec![0; LFE_HIST + MAX_BLOCKS / 2], synth: (0..MAX_CH).map(|_| ChannelSynth::new()).collect(), side: Box::new(Side::new()) }
    }

    /// Forgets the history, as at a seek.
    pub(super) fn reset(&mut self) {
        self.sb.fill(0);
        self.lfe.fill(0);
        self.synth = (0..MAX_CH).map(|_| ChannelSynth::new()).collect();
    }

    /// Decodes one core frame given in the 16-bit big-endian form. Returns its header and the
    /// interleaved samples in the output channel order.
    pub(super) fn decode(&mut self, data: &[u8]) -> Result<(Header, Vec<f32>)> {
        let h = parse_header(data)?;
        // the frame's own bytes; a frame cut short by the end of the data is read as far as it goes
        let data = &data[..h.frame_size.min(data.len())];
        let mut r = Bits::new(data);
        r.skip(h.header_bits);

        if !h.predictor_history {
            // the ADPCM history of the previous frame is dropped
            for ch in 0..MAX_CH {
                for band in 0..BANDS {
                    let b = at(ch, band);
                    self.sb[b..b + HIST].fill(0);
                }
            }
        }
        self.coding_header(&mut r, &h)?;
        let npcm = h.npcmblocks;
        let mut sub_pos = 0;
        let mut lfe_pos = LFE_HIST;
        for sf in 0..self.side.nsubframes {
            self.subframe_header(&mut r, &h, sf)?;
            self.subframe_audio(&mut r, &h, sf, &mut sub_pos, &mut lfe_pos)?;
        }
        if r.left() < 0 {
            return Err(bad("the audio data runs past the end of the frame"));
        }
        for ch in 0..self.side.nchannels {
            let mut nsubbands = self.side.nsubbands[ch];
            if self.side.joint_index[ch] != 0 {
                nsubbands = nsubbands.max(self.side.nsubbands[self.side.joint_index[ch] - 1]);
            }
            for band in 0..BANDS {
                let b = at(ch, band);
                if band < nsubbands {
                    // a frame whose subframes fall short of its length ends in silence
                    self.sb[b + HIST + sub_pos..b + HIST + npcm].fill(0);
                    // the last samples are the history of the next frame
                    self.sb.copy_within(b + npcm..b + npcm + HIST, b);
                } else {
                    self.sb[b..b + HIST + npcm].fill(0);
                }
            }
        }
        Ok((h, self.filter(&h)))
    }

    // 5.3.2 - Primary audio coding header
    fn coding_header(&mut self, r: &mut Bits, h: &Header) -> Result<()> {
        if r.left() < 0 {
            return Err(bad("the frame is cut short"));
        }
        let s = &mut self.side;
        s.nsubframes = r.bits(4) as usize + 1;
        s.nchannels = r.bits(3) as usize + 1;
        if s.nchannels != CHANNELS[h.audio_mode] {
            return Err(bad("the number of primary audio channels does not fit the audio channel arrangement"));
        }
        let n = s.nchannels;
        for ch in 0..n {
            s.nsubbands[ch] = r.bits(5) as usize + 2;
            if s.nsubbands[ch] > BANDS {
                return Err(bad("invalid subband activity count"));
            }
        }
        for ch in 0..n {
            s.vq_start[ch] = r.bits(5) as usize + 1;
        }
        for ch in 0..n {
            let j = r.bits(3) as usize;
            if j > n {
                return Err(bad("invalid joint intensity coding index"));
            }
            s.joint_index[ch] = j;
        }
        for ch in 0..n {
            s.transition_sel[ch] = r.bits(2) as usize;
        }
        for ch in 0..n {
            s.scale_sel[ch] = r.bits(3) as usize;
            if s.scale_sel[ch] == 7 {
                return Err(bad("invalid scale factor code book"));
            }
        }
        for ch in 0..n {
            s.bitalloc_sel[ch] = r.bits(3) as usize;
            if s.bitalloc_sel[ch] == 7 {
                return Err(bad("invalid bit allocation quantizer select"));
            }
        }
        for book in 0..10 {
            for ch in 0..n {
                s.quant_sel[ch][book] = r.bits(QUANT_INDEX_SEL_NBITS[book]) as usize;
            }
        }
        for book in 0..10 {
            for ch in 0..n {
                if s.quant_sel[ch][book] < GROUP_SIZE[book] as usize {
                    s.scale_adj[ch][book] = SCALE_FACTOR_ADJ[r.bits(2) as usize] as i32;
                }
            }
        }
        if h.crc_present {
            r.skip(16);
        }
        Ok(())
    }

    fn scale_value(r: &mut Bits, index: &mut i32, sel: usize) -> Result<i32> {
        let table: &[u32] = if sel > 5 { &SCALE_FACTOR_QUANT7 } else { &SCALE_FACTOR_QUANT6 };
        // a Huffman code gives the difference to the last scale
        if sel < 5 {
            *index += huff::tables().scale_factor[sel].get(r).ok_or_else(|| bad("invalid scale factor code"))?;
        } else {
            *index = r.bits(sel as u32 + 1) as i32;
        }
        if *index < 0 || *index as usize >= table.len() {
            return Err(bad("invalid scale factor index"));
        }
        Ok(table[*index as usize] as i32)
    }

    fn joint_scale_value(r: &mut Bits, sel: usize) -> Result<i32> {
        // the absolute value is coded even with a Huffman code
        let index = if sel < 5 { huff::tables().scale_factor[sel].get(r).ok_or_else(|| bad("invalid joint scale factor code"))? } else { r.bits(sel as u32 + 1) as i32 };
        let index = index + 64;
        if index < 0 || index as usize >= JOINT_SCALE_FACTORS.len() {
            return Err(bad("invalid joint scale factor index"));
        }
        Ok(JOINT_SCALE_FACTORS[index as usize] as i32)
    }

    // 5.4.1 - Primary audio coding side information
    fn subframe_header(&mut self, r: &mut Bits, h: &Header, sf: usize) -> Result<()> {
        if r.left() < 0 {
            return Err(bad("the frame is cut short"));
        }
        let t = huff::tables();
        let s = &mut self.side;
        let n = s.nchannels;
        s.nsubsubframes[sf] = r.bits(2) as usize + 1;
        r.skip(3); // partial subsubframe sample count
        for ch in 0..n {
            for band in 0..s.nsubbands[ch] {
                s.pred_mode[ch][band] = r.bit();
            }
        }
        for ch in 0..n {
            for band in 0..s.nsubbands[ch] {
                if s.pred_mode[ch][band] {
                    s.pred_vq[ch][band] = r.bits(12) as usize;
                }
            }
        }
        for ch in 0..n {
            let sel = s.bitalloc_sel[ch];
            for band in 0..s.vq_start[ch] {
                let abits = if sel < 5 { t.bit_allocation[sel].get(r).ok_or_else(|| bad("invalid bit allocation code"))? as u32 } else { r.bits(sel as u32 - 1) };
                if abits > ABITS_MAX {
                    return Err(bad("invalid bit allocation index"));
                }
                s.bitalloc[ch][band] = abits;
            }
        }
        for ch in 0..n {
            s.transition[ch] = [0; BANDS];
            // a transient is possible only with more than one subsubframe
            if s.nsubsubframes[sf] > 1 {
                let sel = s.transition_sel[ch];
                for band in 0..s.vq_start[ch] {
                    if s.bitalloc[ch][band] != 0 {
                        s.transition[ch][band] = t.transition_mode[sel].get(r).ok_or_else(|| bad("invalid transition mode code"))? as usize;
                    }
                }
            }
        }
        for ch in 0..n {
            let sel = s.scale_sel[ch];
            let mut index = 0;
            for band in 0..s.vq_start[ch] {
                if s.bitalloc[ch][band] != 0 {
                    s.scale[ch][band][0] = Self::scale_value(r, &mut index, sel)?;
                    if s.transition[ch][band] != 0 {
                        s.scale[ch][band][1] = Self::scale_value(r, &mut index, sel)?;
                    }
                } else {
                    s.scale[ch][band][0] = 0;
                }
            }
            for band in s.vq_start[ch]..s.nsubbands[ch] {
                s.scale[ch][band][0] = Self::scale_value(r, &mut index, sel)?;
            }
        }
        for ch in 0..n {
            if s.joint_index[ch] != 0 {
                s.joint_sel[ch] = r.bits(3) as usize;
                if s.joint_sel[ch] == 7 {
                    return Err(bad("invalid joint scale factor code book"));
                }
            }
        }
        for ch in 0..n {
            if s.joint_index[ch] != 0 {
                let src = s.joint_index[ch] - 1;
                for band in s.nsubbands[ch]..s.nsubbands[src] {
                    s.joint_scale[ch][band] = Self::joint_scale_value(r, s.joint_sel[ch])?;
                }
            }
        }
        if h.drc_present {
            r.skip(8); // dynamic range coefficient
        }
        if h.crc_present {
            r.skip(16);
        }
        Ok(())
    }

    /// Reads one band's 8 quantised samples for a subsubframe. Returns whether they were
    /// Huffman-coded, which changes the scale factor.
    fn extract_audio(&self, r: &mut Bits, audio: &mut [i32; 8], abits: u32, ch: usize) -> Result<bool> {
        if abits == 0 {
            *audio = [0; 8];
            return Ok(false);
        }
        if abits <= 10 {
            let book = abits as usize - 1;
            let sel = self.side.quant_sel[ch][book];
            if sel < GROUP_SIZE[book] as usize {
                let v = &huff::tables().quant_index[book][sel];
                for a in audio.iter_mut() {
                    *a = v.get(r).ok_or_else(|| bad("invalid quantisation index code"))?;
                }
                return Ok(true);
            }
            if abits <= 7 {
                let n = BLOCK_CODE_NBITS[book];
                let mut code1 = r.bits(n);
                let mut code2 = r.bits(n);
                let levels = QUANT_LEVELS[abits as usize];
                let offset = (levels as i32 - 1) / 2;
                for a in audio[..4].iter_mut() {
                    *a = (code1 % levels) as i32 - offset;
                    code1 /= levels;
                }
                for a in audio[4..].iter_mut() {
                    *a = (code2 % levels) as i32 - offset;
                    code2 /= levels;
                }
                if code1 | code2 != 0 {
                    return Err(bad("failed to decode block code"));
                }
                return Ok(false);
            }
        }
        for a in audio.iter_mut() {
            *a = r.sbits(abits - 3);
        }
        Ok(false)
    }

    // 5.5 - Primary audio data arrays
    fn subframe_audio(&mut self, r: &mut Bits, h: &Header, sf: usize, sub_pos: &mut usize, lfe_pos: &mut usize) -> Result<()> {
        let n = self.side.nchannels;
        let nss = self.side.nsubsubframes[sf];
        let nsamples = nss * 8;
        if *sub_pos + nsamples > h.npcmblocks {
            return Err(bad("subband sample buffer overflow"));
        }
        if r.left() < 0 {
            return Err(bad("the frame is cut short"));
        }
        // high frequency VQ subbands
        for ch in 0..n {
            let (vq_start, nsubbands) = (self.side.vq_start[ch], self.side.nsubbands[ch]);
            let mut vq_index = [0usize; BANDS];
            for idx in vq_index.iter_mut().take(nsubbands).skip(vq_start) {
                *idx = r.bits(10) as usize;
            }
            for band in vq_start..nsubbands {
                let coeff = &HIGH_FREQ_VQ[vq_index[band]];
                let scale = self.side.scale[ch][band][0];
                let b = at(ch, band) + HIST + *sub_pos;
                for (j, &c) in coeff.iter().take(nsamples).enumerate() {
                    self.sb[b + j] = clip23((((c as i32 * scale) + (1 << 3)) >> 4) as i64);
                }
            }
        }
        // low frequency effects data
        if h.lfe != 0 {
            let nlfe = 2 * h.lfe * nss;
            let mut audio = [0i32; 16];
            for a in audio[..nlfe].iter_mut() {
                *a = r.sbits(8);
            }
            let index = r.bits(8) as usize;
            if index >= SCALE_FACTOR_QUANT7.len() {
                return Err(bad("invalid LFE scale factor index"));
            }
            // the quantiser step size is 0.035
            let scale = norm(4697620i64 * SCALE_FACTOR_QUANT7[index] as i64, 23);
            for (k, &a) in audio[..nlfe].iter().enumerate() {
                self.lfe[*lfe_pos + k] = clip23(((a * scale) >> 4) as i64);
            }
            *lfe_pos += nlfe;
        }
        // audio data
        let lossless = BIT_RATES[h.bit_rate_code] == 3;
        let mut ofs = *sub_pos;
        let mut audio = [0i32; 8];
        for ssf in 0..nss {
            for ch in 0..n {
                if r.left() < 0 {
                    return Err(bad("the frame is cut short"));
                }
                for band in 0..self.side.vq_start[ch] {
                    let abits = self.side.bitalloc[ch][band];
                    let huffman = self.extract_audio(r, &mut audio, abits, ch)?;
                    let step_size = if lossless { LOSSLESS_QUANT[abits as usize] } else { LOSSY_QUANT[abits as usize] } as i32;
                    let trans = self.side.transition[ch][band];
                    let mut scale = if trans == 0 || ssf < trans { self.side.scale[ch][band][0] } else { self.side.scale[ch][band][1] };
                    if huffman {
                        let adj = self.side.scale_adj[ch][abits as usize - 1] as i64;
                        scale = clip23((adj * scale as i64) >> 22);
                    }
                    let b = at(ch, band) + HIST + ofs;
                    dequantize(&mut self.sb[b..b + 8], &audio, step_size, scale);
                }
            }
            // DSYNC
            if (ssf == nss - 1 || h.sync_ssf) && r.bits(16) != 0xFFFF {
                return Err(bad("DSYNC check failed"));
            }
            ofs += 8;
        }
        // inverse ADPCM
        for ch in 0..n {
            for band in 0..self.side.nsubbands[ch] {
                if !self.side.pred_mode[ch][band] {
                    continue;
                }
                let coeff = &ADPCM_VB[self.side.pred_vq[ch][band]];
                let b = at(ch, band) + HIST + *sub_pos;
                for j in 0..nsamples {
                    let mut pred = 0i64;
                    for (i, &c) in coeff.iter().enumerate() {
                        pred += self.sb[b + j - 1 - i] as i64 * c as i64;
                    }
                    let x = clip23(norm(pred, 13) as i64) as i64;
                    self.sb[b + j] = clip23(self.sb[b + j] as i64 + x);
                }
            }
        }
        // joint subband coding
        for ch in 0..n {
            if self.side.joint_index[ch] == 0 {
                continue;
            }
            let src = self.side.joint_index[ch] - 1;
            for band in self.side.nsubbands[ch]..self.side.nsubbands[src] {
                let scale = self.side.joint_scale[ch][band] as i64;
                let (d, s) = (at(ch, band) + HIST + *sub_pos, at(src, band) + HIST + *sub_pos);
                for j in 0..nsamples {
                    self.sb[d + j] = clip23(norm(self.sb[s + j] as i64 * scale, 17) as i64);
                }
            }
        }
        *sub_pos = ofs;
        Ok(())
    }

    /// Runs the synthesis filters over the decoded subband samples and arranges the channels.
    fn filter(&mut self, h: &Header) -> Vec<f32> {
        let npcm = h.npcmblocks;
        let nsamples = npcm * 32;
        let mut spk: [Vec<f32>; 7] = Default::default();
        let window = if h.filter_perfect { &FIR_32_PERFECT } else { &FIR_32_NONPERFECT };
        for ch in 0..h.primary_channels() {
            let mut out = vec![0f32; nsamples];
            let bands: Vec<&[i32]> = (0..BANDS).map(|b| &self.sb[at(ch, b) + HIST..at(ch, b) + HIST + npcm]).collect();
            qmf_synth(&mut self.synth[ch], window, &bands, npcm, &mut out);
            spk[PRM_CH_TO_SPKR[h.audio_mode][ch]] = out;
        }
        if h.lfe != 0 {
            let dec_select = (h.lfe == 1) as usize;
            let mut out = vec![0f32; nsamples];
            lfe_fir(&mut out, &self.lfe, LFE_HIST, if dec_select == 1 { &LFE_FIR_128 } else { &LFE_FIR_64 }, npcm, dec_select);
            let nlfe = npcm >> (dec_select + 1);
            for n in (0..LFE_HIST).rev() {
                self.lfe[n] = self.lfe[nlfe + n];
            }
            spk[SPK_LFE] = out;
        }
        // sum and difference channels
        if (h.sumdiff_front && h.audio_mode > 0) || h.audio_mode == 3 {
            butterflies(&mut spk, SPK_L, SPK_R);
        }
        if h.sumdiff_surround && h.audio_mode >= 8 {
            butterflies(&mut spk, SPK_LS, SPK_RS);
        }
        let present: Vec<&Vec<f32>> = OUTPUT_ORDER.iter().map(|&s| &spk[s]).filter(|v| !v.is_empty()).collect();
        let nch = present.len();
        let mut out = vec![0f32; nsamples * nch];
        for (c, v) in present.iter().enumerate() {
            for (i, &x) in v.iter().enumerate() {
                out[i * nch + c] = x;
            }
        }
        out
    }
}

/// `a, b = a + b, a - b`.
fn butterflies(spk: &mut [Vec<f32>; 7], a: usize, b: usize) {
    let (x, y) = (std::mem::take(&mut spk[a]), std::mem::take(&mut spk[b]));
    let (sum, diff): (Vec<f32>, Vec<f32>) = x.iter().zip(&y).map(|(&p, &q)| (p + q, p - q)).unzip();
    spk[a] = sum;
    spk[b] = diff;
}
