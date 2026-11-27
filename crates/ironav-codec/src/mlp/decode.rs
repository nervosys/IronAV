//! The access unit decoder, after FFmpeg's `mlpdec.c`.

use super::*;
use ironav_core::{AudioFrame, Decoder, Frame, Packet};

const MAX_CHANNELS: usize = 8;
const MAX_BLOCKSIZE: usize = 160;
const MAX_BLOCKSIZE_POW2: usize = 256;
const MAX_SUBSTREAMS: usize = 4;
const MAX_SAMPLERATE: u32 = 192000;
const MAX_MATRIX_CHANNEL_MLP: usize = 5;
const MAX_MATRIX_CHANNEL_TRUEHD: usize = 7;
const MAX_MATRICES_MLP: usize = 6;
const MAX_MATRICES_TRUEHD: usize = 8;
const MAX_FIR_ORDER: usize = 8;
const MAX_IIR_ORDER: usize = 4;

const FIR: usize = 0;
const IIR: usize = 1;

const PARAM_BLOCKSIZE: u8 = 1 << 7;
const PARAM_MATRIX: u8 = 1 << 6;
const PARAM_OUTSHIFT: u8 = 1 << 5;
const PARAM_QUANTSTEP: u8 = 1 << 4;
const PARAM_FIR: u8 = 1 << 3;
const PARAM_IIR: u8 = 1 << 2;
const PARAM_HUFFOFFSET: u8 = 1 << 1;
const PARAM_PRESENCE: u8 = 1;

/// The Huffman codes of the residuals (`ff_mlp_huffman_tables`): code and length for the symbols
/// 0 to 17 of each of the three tables.
const HUFFMAN: [[(u8, u8); 18]; 3] = [
    [
        (0x01, 9),
        (0x01, 8),
        (0x01, 7),
        (0x01, 6),
        (0x01, 5),
        (0x01, 4),
        (0x01, 3),
        (0x04, 3),
        (0x05, 3),
        (0x06, 3),
        (0x07, 3),
        (0x03, 3),
        (0x05, 4),
        (0x09, 5),
        (0x11, 6),
        (0x21, 7),
        (0x41, 8),
        (0x81, 9),
    ],
    [
        (0x01, 9),
        (0x01, 8),
        (0x01, 7),
        (0x01, 6),
        (0x01, 5),
        (0x01, 4),
        (0x01, 3),
        (0x02, 2),
        (0x03, 2),
        (0x03, 3),
        (0x05, 4),
        (0x09, 5),
        (0x11, 6),
        (0x21, 7),
        (0x41, 8),
        (0x81, 9),
        (0, 0),
        (0, 0),
    ],
    [
        (0x01, 9),
        (0x01, 8),
        (0x01, 7),
        (0x01, 6),
        (0x01, 5),
        (0x01, 4),
        (0x01, 3),
        (0x01, 1),
        (0x03, 3),
        (0x05, 4),
        (0x09, 5),
        (0x11, 6),
        (0x21, 7),
        (0x41, 8),
        (0x81, 9),
        (0, 0),
        (0, 0),
        (0, 0),
    ],
];

/// The noise of TrueHD streams (`noise_table`).
const NOISE_TABLE: [i8; 256] = [
    30, 51, 22, 54, 3, 7, -4, 38, 14, 55, 46, 81, 22, 58, -3, 2, 52, 31, -7, 51, 15, 44, 74, 30, 85, -17, 10, 33, 18, 80, 28, 62, 10, 32, 23, 69, 72, 26, 35, 17, 73, 60, 8, 56, 2, 6, -2, -5, 51,
    4, 11, 50, 66, 76, 21, 44, 33, 47, 1, 26, 64, 48, 57, 40, 38, 16, -10, -28, 92, 22, -18, 29, -10, 5, -13, 49, 19, 24, 70, 34, 61, 48, 30, 14, -6, 25, 58, 33, 42, 60, 67, 17, 54, 17, 22, 30,
    67, 44, -9, 50, -11, 43, 40, 32, 59, 82, 13, 49, -14, 55, 60, 36, 48, 49, 31, 47, 15, 12, 4, 65, 1, 23, 29, 39, 45, -2, 84, 69, 0, 72, 37, 57, 27, 41, -15, -16, 35, 31, 14, 61, 24, 0, 27, 24,
    16, 41, 55, 34, 53, 9, 56, 12, 25, 29, 53, 5, 20, -20, -8, 20, 13, 28, -3, 78, 38, 16, 11, 62, 46, 29, 21, 24, 46, 65, 43, -23, 89, 18, 74, 21, 38, -12, 19, 12, -19, 8, 15, 33, 4, 57, 9, -8,
    36, 35, 26, 28, 7, 83, 63, 79, 75, 11, 3, 87, 37, 47, 34, 40, 39, 19, 20, 42, 27, 34, 39, 77, 13, 42, 59, 64, 45, -1, 32, 37, 45, -5, 53, -6, 7, 36, 50, 23, 6, 32, 9, -21, 18, 71, 27, 52, -25,
    31, 35, 42, -1, 68, 63, 52, 26, 43, 66, 37, 41, 25, 40, 70,
];

#[derive(Clone, Copy, Default)]
struct FilterParams {
    order: usize,
    shift: u32,
    /// The last output values, the newest first.
    state: [i32; MAX_FIR_ORDER],
}

#[derive(Clone, Copy, Default)]
struct ChannelParams {
    filter: [FilterParams; 2],
    coeff: [[i32; MAX_FIR_ORDER]; 2],
    huff_offset: i32,
    sign_huff_offset: i32,
    codebook: usize,
    huff_lsbs: i32,
}

struct SubStream {
    restart_seen: bool,
    end_of_stream: bool,
    noise_type: bool,
    min_channel: usize,
    max_channel: usize,
    coded_channels: u64,
    max_matrix_channel: usize,
    ch_assign: [usize; MAX_CHANNELS],
    mask: u64,
    channel_params: [ChannelParams; MAX_CHANNELS],
    noise_shift: u32,
    noisegen_seed: u32,
    data_check_present: bool,
    param_presence_flags: u8,
    num_primitive_matrices: usize,
    matrix_out_ch: [usize; 8],
    lsb_bypass: [bool; 8],
    /// 2.14 fixed point.
    matrix_coeff: [[i32; MAX_CHANNELS]; 8],
    matrix_noise_shift: [u32; 8],
    quant_step_size: [u32; MAX_CHANNELS],
    blocksize: usize,
    blockpos: usize,
    output_shift: [i32; MAX_CHANNELS],
    lossless_check_data: u32,
}

impl SubStream {
    fn new() -> Self {
        SubStream {
            restart_seen: false,
            end_of_stream: false,
            noise_type: false,
            min_channel: 0,
            max_channel: 0,
            coded_channels: 0,
            max_matrix_channel: 0,
            ch_assign: [0; MAX_CHANNELS],
            mask: 0,
            channel_params: [ChannelParams::default(); MAX_CHANNELS],
            noise_shift: 0,
            noisegen_seed: 0,
            data_check_present: false,
            param_presence_flags: 0,
            num_primitive_matrices: 0,
            matrix_out_ch: [0; 8],
            lsb_bypass: [false; 8],
            matrix_coeff: [[0; MAX_CHANNELS]; 8],
            matrix_noise_shift: [0; 8],
            quant_step_size: [0; MAX_CHANNELS],
            blocksize: 0,
            blockpos: 0,
            output_shift: [0; MAX_CHANNELS],
            lossless_check_data: 0xFFFF_FFFF,
        }
    }
}

/// The TrueHD or MLP decoder. A packet is an access unit (or several, one after another).
pub struct MlpDecoder {
    truehd: bool,
    is_major_sync_unit: bool,
    major_sync_header_size: usize,
    params_valid: bool,
    num_substreams: usize,
    substream_info: u32,
    max_decoded_substream: usize,
    needs_reordering: bool,
    access_unit_size: usize,
    access_unit_size_pow2: usize,
    sample_rate: u32,
    bits: u32,
    /// Channels of the output layout (that of the last decoded substream).
    out_channels: usize,
    substream: [SubStream; MAX_SUBSTREAMS],
    matrix_changed: u32,
    filter_changed: [[u32; 2]; MAX_CHANNELS],
    noise_buffer: [i8; MAX_BLOCKSIZE_POW2],
    bypassed_lsbs: [[i8; MAX_CHANNELS]; MAX_BLOCKSIZE],
    sample_buffer: [[i32; MAX_CHANNELS]; MAX_BLOCKSIZE],
    /// For each table and each 9 bits of input: the symbol and the code's length (0: no code).
    huff_lut: Vec<[(u8, u8); 512]>,
    check_failures: u64,
}

fn decode_error(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("MLP/TrueHD: {msg}"))
}

/// The channel TrueHD's number `index` is, among the channels of `mask`, as its bit; 0 if there are
/// fewer than `index + 1` channels (`thd_channel_layout_extract_channel`).
fn thd_extract_channel(mask: u64, index: u32) -> u64 {
    if mask.count_ones() <= index {
        return 0;
    }
    let mut left = index;
    for &c in THD_CHANNEL_ORDER.iter() {
        if mask & c != 0 {
            if left == 0 {
                return c;
            }
            left -= 1;
        }
    }
    0
}

/// The place of TrueHD's channel number `index` in the output of a substream whose channels are
/// `mask`: its rank among those channels in FFmpeg's native order. `None` if the mask has no such
/// channel.
pub(super) fn thd_native_index(mask: u64, index: u32) -> Option<usize> {
    let channel = thd_extract_channel(mask, index);
    (channel != 0).then(|| (mask & (channel - 1)).count_ones() as usize)
}

impl MlpDecoder {
    /// A decoder for TrueHD (`truehd`) or MLP.
    pub fn new(truehd: bool) -> Self {
        let mut huff_lut = vec![[(0u8, 0u8); 512]; 3];
        for (t, lut) in huff_lut.iter_mut().enumerate() {
            for (sym, &(code, len)) in HUFFMAN[t].iter().enumerate() {
                if len == 0 {
                    continue;
                }
                let first = (code as usize) << (9 - len);
                for e in lut.iter_mut().skip(first).take(1 << (9 - len)) {
                    *e = (sym as u8, len);
                }
            }
        }
        MlpDecoder {
            truehd,
            is_major_sync_unit: false,
            major_sync_header_size: 0,
            params_valid: false,
            num_substreams: 0,
            substream_info: 0,
            max_decoded_substream: 0,
            needs_reordering: false,
            access_unit_size: 0,
            access_unit_size_pow2: 0,
            sample_rate: 0,
            bits: 24,
            out_channels: 0,
            substream: [SubStream::new(), SubStream::new(), SubStream::new(), SubStream::new()],
            matrix_changed: 0,
            filter_changed: [[0; 2]; MAX_CHANNELS],
            noise_buffer: [0; MAX_BLOCKSIZE_POW2],
            bypassed_lsbs: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
            sample_buffer: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
            huff_lut,
            check_failures: 0,
        }
    }

    /// How many checks that FFmpeg reports without stopping have failed so far: restart header
    /// checksums, substream parities and checksums, and lossless check values.
    pub fn check_failures(&self) -> u64 {
        self.check_failures
    }

    /// A major sync: the stream's rate, sizes and channel masks (`read_major_sync`).
    fn read_major_sync(&mut self, body: &[u8]) -> Result<()> {
        let mh = parse_major_sync(body)?;
        if mh.group1_bits == 0 {
            return Err(decode_error("invalid/unknown bits per sample"));
        }
        if mh.group2_bits > mh.group1_bits {
            return Err(decode_error("channel group 2 cannot have more bits per sample than group 1"));
        }
        if mh.group2_sample_rate != 0 && mh.group2_sample_rate != mh.sample_rate {
            return Err(decode_error("channel groups with differing sample rates are not supported"));
        }
        if mh.sample_rate == 0 {
            return Err(decode_error("invalid/unknown sampling rate"));
        }
        if mh.sample_rate > MAX_SAMPLERATE {
            return Err(decode_error("sampling rate over the supported maximum"));
        }
        if mh.access_unit_size > MAX_BLOCKSIZE || mh.access_unit_size_pow2 > MAX_BLOCKSIZE_POW2 {
            return Err(decode_error("block size over the supported maximum"));
        }
        if mh.num_substreams == 0 {
            return Err(decode_error("no substreams"));
        }
        if !self.truehd && mh.num_substreams > 2 {
            return Err(decode_error("MLP only supports up to 2 substreams"));
        }
        if mh.num_substreams > MAX_SUBSTREAMS {
            return Err(decode_error("more substreams than the decoder supports"));
        }
        self.major_sync_header_size = mh.header_size;
        self.access_unit_size = mh.access_unit_size;
        self.access_unit_size_pow2 = mh.access_unit_size_pow2;
        self.num_substreams = mh.num_substreams;
        self.substream_info = mh.substream_info;
        // the 4th substream is used by Dolby Atmos for non-audio data: not decoded
        self.max_decoded_substream = (mh.num_substreams - 1).min(2);
        self.sample_rate = mh.sample_rate;
        self.bits = mh.group1_bits;
        self.params_valid = true;
        for s in self.substream.iter_mut() {
            s.restart_seen = false;
        }
        if !self.truehd && mh.truehd {
            return Err(decode_error("unexpected stream type in MLP"));
        }
        if self.truehd && !mh.truehd {
            return Err(decode_error("unexpected stream type in TrueHD"));
        }
        let masks = mh.masks([self.substream[0].mask, self.substream[1].mask, self.substream[2].mask, self.substream[3].mask]);
        for (s, m) in self.substream.iter_mut().zip(masks) {
            s.mask = m;
        }
        self.needs_reordering = (18..=20).contains(&mh.channel_arrangement);
        Ok(())
    }

    /// A restart header: the parameters that change rarely (`read_restart_header`).
    fn read_restart_header(&mut self, gb: &mut Bits, buf: &[u8], substr: usize) -> Result<()> {
        let truehd = self.truehd;
        let std_max_matrix_channel = if truehd { MAX_MATRIX_CHANNEL_TRUEHD } else { MAX_MATRIX_CHANNEL_MLP };
        let start_count = gb.pos;
        let sync_word = gb.bits(13);
        if sync_word != 0x31EA >> 1 {
            return Err(decode_error("restart header sync incorrect"));
        }
        let noise_type = gb.bit();
        if !truehd && noise_type {
            return Err(decode_error("MLP must have 0x31ea sync word"));
        }
        gb.skip(16); // output timestamp
        let min_channel = gb.bits(4) as usize;
        let max_channel = gb.bits(4) as usize;
        let max_matrix_channel = gb.bits(4) as usize;
        if max_matrix_channel > std_max_matrix_channel {
            return Err(decode_error("max matrix channel too large"));
        }
        // TrueHD streams with more than 6 channels use TrueHD's noise type
        if max_matrix_channel > MAX_MATRIX_CHANNEL_MLP && !noise_type {
            return Err(decode_error("more channels than the decoder supports"));
        }
        if max_channel + 1 > MAX_CHANNELS || max_channel + 1 < min_channel {
            return Err(decode_error("bad channel range"));
        }
        let is_last = substr == self.max_decoded_substream;
        let s = &mut self.substream[substr];
        s.min_channel = min_channel;
        s.max_channel = max_channel;
        s.coded_channels = ((1u64 << (max_channel + 1 - min_channel)) - 1) << min_channel;
        s.max_matrix_channel = max_matrix_channel;
        s.noise_type = noise_type;
        s.noise_shift = gb.bits(4);
        s.noisegen_seed = gb.bits(23);
        gb.skip(19);
        s.data_check_present = gb.bit();
        let lossless_check = gb.bits(8) as u8;
        if is_last && s.lossless_check_data != 0xFFFF_FFFF {
            let d = s.lossless_check_data;
            let tmp = (d ^ (d >> 16) ^ ((d ^ (d >> 16)) >> 8)) as u8;
            if tmp != lossless_check {
                self.check_failures += 1;
            }
        }
        let s = &mut self.substream[substr];
        gb.skip(16);
        s.ch_assign = [0; MAX_CHANNELS];
        for ch in 0..=s.max_matrix_channel {
            let mut ch_assign = gb.bits(6) as usize;
            if truehd {
                ch_assign = thd_native_index(s.mask, ch_assign as u32).unwrap_or(usize::MAX);
            }
            if ch_assign > s.max_matrix_channel {
                return Err(decode_error("assignment of a matrix channel to an invalid output channel"));
            }
            s.ch_assign[ch_assign] = ch;
        }
        let checksum = restart_checksum(buf, gb.pos - start_count);
        if checksum as u32 != gb.bits(8) {
            self.check_failures += 1;
        }
        let s = &mut self.substream[substr];
        // default decoding parameters
        s.param_presence_flags = 0xFF;
        s.num_primitive_matrices = 0;
        s.blocksize = 8;
        s.lossless_check_data = 0;
        s.output_shift = [0; MAX_CHANNELS];
        s.quant_step_size = [0; MAX_CHANNELS];
        for ch in s.min_channel..=s.max_channel {
            let cp = &mut s.channel_params[ch];
            cp.filter[FIR].order = 0;
            cp.filter[IIR].order = 0;
            cp.filter[FIR].shift = 0;
            cp.filter[IIR].shift = 0;
            // default audio coding is 24-bit raw PCM
            cp.huff_offset = 0;
            cp.sign_huff_offset = -(1 << 23);
            cp.codebook = 0;
            cp.huff_lsbs = 24;
        }
        if is_last {
            self.out_channels = s.mask.count_ones() as usize;
            if !truehd && self.needs_reordering {
                if s.mask == (QUAD | LFE) || s.mask == FIVE_ZERO_BACK {
                    let i = s.ch_assign[4];
                    s.ch_assign[4] = s.ch_assign[3];
                    s.ch_assign[3] = s.ch_assign[2];
                    s.ch_assign[2] = i;
                } else if s.mask == FIVE_ONE_BACK {
                    s.ch_assign.swap(2, 4);
                    s.ch_assign.swap(3, 5);
                }
            }
        }
        Ok(())
    }

    /// Parameters of one of a channel's prediction filters (`read_filter_params`).
    fn read_filter_params(&mut self, gb: &mut Bits, substr: usize, channel: usize, filter: usize) -> Result<()> {
        let max_order = if filter == IIR { MAX_IIR_ORDER } else { MAX_FIR_ORDER };
        let prev = self.filter_changed[channel][filter];
        self.filter_changed[channel][filter] += 1;
        if prev > 1 {
            return Err(decode_error("filters may change only once per access unit"));
        }
        let fp = &mut self.substream[substr].channel_params[channel];
        let order = gb.bits(4) as usize;
        if order > max_order {
            return Err(decode_error("filter order greater than the maximum"));
        }
        fp.filter[filter].order = order;
        if order > 0 {
            fp.filter[filter].shift = gb.bits(4);
            let coeff_bits = gb.bits(5);
            let coeff_shift = gb.bits(3);
            if !(1..=16).contains(&coeff_bits) {
                return Err(decode_error("filter coeff_bits must be between 1 and 16"));
            }
            if coeff_bits + coeff_shift > 16 {
                return Err(decode_error("sum of coeff_bits and coeff_shift for a filter must be 16 or less"));
            }
            for i in 0..order {
                fp.coeff[filter][i] = gb.sbits(coeff_bits) << coeff_shift;
            }
            if gb.bit() {
                if filter == FIR {
                    return Err(decode_error("FIR filter has state data specified"));
                }
                let state_bits = gb.bits(4);
                let state_shift = gb.bits(4);
                for i in 0..order {
                    fp.filter[filter].state[i] = if state_bits != 0 { gb.sbits(state_bits) << state_shift } else { 0 };
                }
            }
        }
        Ok(())
    }

    /// The primitive matrices (`read_matrix_params`).
    fn read_matrix_params(&mut self, gb: &mut Bits, substr: usize) -> Result<()> {
        let max_primitive_matrices = if self.truehd { MAX_MATRICES_TRUEHD } else { MAX_MATRICES_MLP };
        let prev = self.matrix_changed;
        self.matrix_changed += 1;
        if prev > 1 {
            return Err(decode_error("matrices may change only once per access unit"));
        }
        let s = &mut self.substream[substr];
        let r = (|| -> Result<()> {
            s.num_primitive_matrices = gb.bits(4) as usize;
            if s.num_primitive_matrices > max_primitive_matrices {
                return Err(decode_error("too many primitive matrices"));
            }
            for mat in 0..s.num_primitive_matrices {
                s.matrix_out_ch[mat] = gb.bits(4) as usize;
                let frac_bits = gb.bits(4);
                s.lsb_bypass[mat] = gb.bit();
                if s.matrix_out_ch[mat] > s.max_matrix_channel {
                    return Err(decode_error("invalid channel specified as output from matrix"));
                }
                if frac_bits > 14 {
                    return Err(decode_error("too many fractional bits specified"));
                }
                let max_chan = s.max_matrix_channel + if s.noise_type { 0 } else { 2 };
                for ch in 0..=max_chan {
                    let coeff_val = if gb.bit() { gb.sbits(frac_bits + 2) } else { 0 };
                    s.matrix_coeff[mat][ch] = coeff_val << (14 - frac_bits);
                }
                s.matrix_noise_shift[mat] = if s.noise_type { gb.bits(4) } else { 0 };
            }
            Ok(())
        })();
        if r.is_err() {
            s.num_primitive_matrices = 0;
            s.matrix_out_ch = [0; 8];
        }
        r
    }

    /// A channel's parameters (`read_channel_params`).
    fn read_channel_params(&mut self, gb: &mut Bits, substr: usize, ch: usize) -> Result<()> {
        let flags = self.substream[substr].param_presence_flags;
        if flags & PARAM_FIR != 0 && gb.bit() {
            self.read_filter_params(gb, substr, ch, FIR)?;
        }
        if flags & PARAM_IIR != 0 && gb.bit() {
            self.read_filter_params(gb, substr, ch, IIR)?;
        }
        let cp = &mut self.substream[substr].channel_params[ch];
        let (fir, iir) = (cp.filter[FIR], cp.filter[IIR]);
        if fir.order + iir.order > 8 {
            return Err(decode_error("total filter orders too high"));
        }
        if fir.order > 0 && iir.order > 0 && fir.shift != iir.shift {
            return Err(decode_error("FIR and IIR filters must use the same precision"));
        }
        // only the FIR filter's precision is used: an IIR filter alone lends its own
        if fir.order == 0 && iir.order > 0 {
            cp.filter[FIR].shift = iir.shift;
        }
        if flags & PARAM_HUFFOFFSET != 0 && gb.bit() {
            cp.huff_offset = gb.sbits(15);
        }
        cp.codebook = gb.bits(2) as usize;
        cp.huff_lsbs = gb.bits(5) as i32;
        if cp.codebook > 0 && cp.huff_lsbs > 24 {
            cp.huff_lsbs = 0;
            return Err(decode_error("invalid huff_lsbs"));
        }
        Ok(())
    }

    /// The sign/rounding-corrected Huffman offset of a channel (`calculate_sign_huff`).
    fn calculate_sign_huff(&self, substr: usize, ch: usize) -> i32 {
        let s = &self.substream[substr];
        let cp = &s.channel_params[ch];
        let lsb_bits = cp.huff_lsbs - s.quant_step_size[ch] as i32;
        let sign_shift = lsb_bits + if cp.codebook > 0 { 2 - cp.codebook as i32 } else { -1 };
        let mut sign_huff_offset = cp.huff_offset;
        if cp.codebook > 0 {
            sign_huff_offset = sign_huff_offset.wrapping_sub(7i32.wrapping_shl(lsb_bits as u32));
        }
        if sign_shift >= 0 {
            sign_huff_offset = sign_huff_offset.wrapping_sub(1i32.wrapping_shl(sign_shift as u32));
        }
        sign_huff_offset
    }

    /// The decoding parameters that change more often than the restart header
    /// (`read_decoding_params`).
    fn read_decoding_params(&mut self, gb: &mut Bits, substr: usize) -> Result<()> {
        let access_unit_size = self.access_unit_size;
        let mut recompute_sho = 0u32;
        let mut ret = Ok(());
        'params: {
            let s = &mut self.substream[substr];
            if s.param_presence_flags & PARAM_PRESENCE != 0 && gb.bit() {
                s.param_presence_flags = gb.bits(8) as u8;
            }
            if s.param_presence_flags & PARAM_BLOCKSIZE != 0 && gb.bit() {
                s.blocksize = gb.bits(9) as usize;
                if s.blocksize < 8 || s.blocksize > access_unit_size {
                    s.blocksize = 0;
                    return Err(decode_error("invalid blocksize"));
                }
            }
            if s.param_presence_flags & PARAM_MATRIX != 0 && gb.bit() {
                self.read_matrix_params(gb, substr)?;
            }
            let s = &mut self.substream[substr];
            if s.param_presence_flags & PARAM_OUTSHIFT != 0 && gb.bit() {
                for ch in 0..=s.max_matrix_channel {
                    s.output_shift[ch] = gb.sbits(4);
                    if s.output_shift[ch] < 0 {
                        // negative output shifts are not supported
                        s.output_shift[ch] = 0;
                    }
                }
            }
            if s.param_presence_flags & PARAM_QUANTSTEP != 0 && gb.bit() {
                for ch in 0..=s.max_channel {
                    s.quant_step_size[ch] = gb.bits(4);
                    recompute_sho |= 1 << ch;
                }
            }
            let (min, max) = (s.min_channel, s.max_channel);
            for ch in min..=max {
                if gb.bit() {
                    recompute_sho |= 1 << ch;
                    if let Err(e) = self.read_channel_params(gb, substr, ch) {
                        ret = Err(e);
                        break 'params;
                    }
                }
            }
        }
        let max_channel = self.substream[substr].max_channel;
        for ch in 0..=max_channel {
            if recompute_sho & (1 << ch) != 0 {
                let s = &mut self.substream[substr];
                let cp = &s.channel_params[ch];
                if cp.codebook > 0 && cp.huff_lsbs < s.quant_step_size[ch] as i32 {
                    if ret.is_ok() {
                        ret = Err(decode_error("quant_step_size larger than huff_lsbs"));
                    }
                    s.quant_step_size[ch] = 0;
                }
                let v = self.calculate_sign_huff(substr, ch);
                self.substream[substr].channel_params[ch].sign_huff_offset = v;
            }
        }
        ret
    }

    /// One sample time of residuals: the bypassed LSB bits, then each channel's Huffman and raw
    /// bits (`read_huff_channels`).
    fn read_huff_channels(&mut self, gb: &mut Bits, substr: usize, pos: usize) -> Result<()> {
        let s = &self.substream[substr];
        let row = pos + s.blockpos;
        for mat in 0..s.num_primitive_matrices {
            if s.lsb_bypass[mat] {
                self.bypassed_lsbs[row][mat] = gb.bit() as i8;
            }
        }
        for channel in s.min_channel..=s.max_channel {
            let cp = &s.channel_params[channel];
            let quant_step_size = s.quant_step_size[channel] as i32;
            let lsb_bits = cp.huff_lsbs - quant_step_size;
            let mut result = 0i32;
            if cp.codebook > 0 {
                let (sym, len) = self.huff_lut[cp.codebook - 1][(gb.peek32() >> 23) as usize];
                if len == 0 {
                    return Err(decode_error("invalid Huffman code"));
                }
                gb.skip(len as usize);
                result = sym as i32;
            }
            if lsb_bits > 0 {
                result = result.wrapping_shl(lsb_bits as u32).wrapping_add(gb.bits(lsb_bits as u32) as i32);
            }
            result = result.wrapping_add(cp.sign_huff_offset);
            result = result.wrapping_shl(quant_step_size as u32);
            self.sample_buffer[row][channel] = result;
        }
        Ok(())
    }

    /// The prediction filters of a channel over the block just read (`filter_channel` and
    /// `mlp_filter_channel`).
    fn filter_channel(&mut self, substr: usize, channel: usize) {
        let s = &mut self.substream[substr];
        let mask = -(1i32 << s.quant_step_size[channel]);
        let blocksize = s.blocksize;
        let blockpos = s.blockpos;
        let cp = &mut s.channel_params[channel];
        let (firorder, iirorder) = (cp.filter[FIR].order, cp.filter[IIR].order);
        let shift = cp.filter[FIR].shift;
        let mut fir = cp.filter[FIR].state;
        // only the first four IIR values are in use (the filter is at most of order 4)
        let mut iir = cp.filter[IIR].state;
        iir[MAX_IIR_ORDER..].fill(0);
        for i in 0..blocksize {
            let residual = self.sample_buffer[blockpos + i][channel];
            let mut accum = 0i64;
            for order in 0..firorder {
                accum += fir[order] as i64 * cp.coeff[FIR][order] as i64;
            }
            for order in 0..iirorder {
                accum += iir[order] as i64 * cp.coeff[IIR][order] as i64;
            }
            accum >>= shift;
            let result = ((accum + residual as i64) & mask as i64) as i32;
            fir.copy_within(0..MAX_FIR_ORDER - 1, 1);
            fir[0] = result;
            iir.copy_within(0..MAX_FIR_ORDER - 1, 1);
            iir[0] = (result as i64 - accum) as i32;
            self.sample_buffer[blockpos + i][channel] = result;
        }
        cp.filter[FIR].state = fir;
        cp.filter[IIR].state = iir;
    }

    /// A block of residuals, filtered into samples (`read_block_data`).
    fn read_block_data(&mut self, gb: &mut Bits, substr: usize) -> Result<()> {
        let s = &self.substream[substr];
        let (blockpos, blocksize) = (s.blockpos, s.blocksize);
        let mut expected_stream_pos = 0usize;
        let data_check_present = s.data_check_present;
        if data_check_present {
            expected_stream_pos = gb.pos;
            expected_stream_pos += gb.bits(16) as usize;
        }
        if blockpos + blocksize > self.access_unit_size {
            return Err(decode_error("too many audio samples in frame"));
        }
        for row in &mut self.bypassed_lsbs[blockpos..blockpos + blocksize] {
            *row = [0; MAX_CHANNELS];
        }
        for i in 0..blocksize {
            self.read_huff_channels(gb, substr, i)?;
        }
        let (min, max) = (self.substream[substr].min_channel, self.substream[substr].max_channel);
        for ch in min..=max {
            self.filter_channel(substr, ch);
        }
        self.substream[substr].blockpos += blocksize;
        if data_check_present {
            if gb.pos != expected_stream_pos {
                self.check_failures += 1;
            }
            gb.skip(8);
        }
        Ok(())
    }

    /// Two channels of noise for the matrix, when the restart sync word is 0x31EA
    /// (`generate_2_noise_channels`).
    fn generate_2_noise_channels(&mut self, substr: usize) {
        let s = &mut self.substream[substr];
        let mut seed = s.noisegen_seed;
        let maxchan = s.max_matrix_channel;
        for i in 0..s.blockpos {
            let seed_shr7 = (seed >> 7) as u16;
            self.sample_buffer[i][maxchan + 1] = ((seed >> 15) as i8 as i32) << s.noise_shift;
            self.sample_buffer[i][maxchan + 2] = (seed_shr7 as i8 as i32) << s.noise_shift;
            seed = (seed << 16) ^ seed_shr7 as u32 ^ ((seed_shr7 as u32) << 5);
        }
        s.noisegen_seed = seed;
    }

    /// A block of noise, when the restart sync word is 0x31EB (`fill_noise_buffer`).
    fn fill_noise_buffer(&mut self, substr: usize) {
        let s = &mut self.substream[substr];
        let mut seed = s.noisegen_seed;
        for i in 0..self.access_unit_size_pow2 {
            let seed_shr15 = (seed >> 15) as u8;
            self.noise_buffer[i] = NOISE_TABLE[seed_shr15 as usize];
            seed = (seed << 8) ^ seed_shr15 as u32 ^ ((seed_shr15 as u32) << 5);
        }
        s.noisegen_seed = seed;
    }

    /// Applies the matrices, and packs the samples of the last substream (`output_data`).
    fn output_data(&mut self, substr: usize, pts: i64) -> Result<Frame> {
        if self.out_channels != self.substream[substr].max_matrix_channel + 1 {
            return Err(decode_error("channel count mismatch"));
        }
        if self.substream[substr].blockpos == 0 {
            return Err(decode_error("no samples to output"));
        }
        let mut maxchan = self.substream[substr].max_matrix_channel;
        if !self.substream[substr].noise_type {
            self.generate_2_noise_channels(substr);
            maxchan += 2;
        } else {
            self.fill_noise_buffer(substr);
        }
        let s = &self.substream[substr];
        let blockpos = s.blockpos;
        let pow2 = self.access_unit_size_pow2;
        // the matrices in turn reconstruct the original samples (`mlp_rematrix_channel`)
        for mat in 0..s.num_primitive_matrices {
            let dest_ch = s.matrix_out_ch[mat];
            let coeffs = &s.matrix_coeff[mat];
            let noise_shift = s.matrix_noise_shift[mat];
            let mask = -(1i32 << s.quant_step_size[dest_ch]) as i64;
            let mut index = (s.num_primitive_matrices - mat) as i32;
            let index2 = 2 * index + 1;
            for i in 0..blockpos {
                let row = &mut self.sample_buffer[i];
                let mut accum = 0i64;
                for src in 0..=maxchan {
                    accum += row[src] as i64 * coeffs[src] as i64;
                }
                if noise_shift != 0 {
                    index &= pow2 as i32 - 1;
                    accum += self.noise_buffer[index as usize] as i64 * (1i64 << (noise_shift + 7));
                    index += index2;
                }
                row[dest_ch] = (((accum >> 14) & mask) as i32).wrapping_add(self.bypassed_lsbs[i][mat] as i32);
            }
        }
        // pack the output (`ff_mlp_pack_output`): lossless check data, then the samples
        let s = &mut self.substream[substr];
        let channels = s.max_matrix_channel + 1;
        let mut check = s.lossless_check_data;
        let is32 = self.bits > 16;
        let mut samples = Vec::with_capacity(blockpos * channels);
        for i in 0..blockpos {
            for out_ch in 0..channels {
                let mat_ch = s.ch_assign[out_ch];
                let sample = self.sample_buffer[i][mat_ch].wrapping_mul((1u32 << s.output_shift[mat_ch]) as i32);
                check ^= ((sample & 0xFF_FFFF) as u32) << mat_ch;
                samples.push(if is32 { sample.wrapping_mul(256) as f64 / 2147483648.0 } else { (sample >> 8) as f64 / 32768.0 });
            }
        }
        s.lossless_check_data = check;
        Ok(Frame::Audio(AudioFrame { sample_rate: self.sample_rate, channels: channels as u16, pts, samples }))
    }

    /// One access unit at the start of `buf`: the number of bytes it takes, and the frame it gives
    /// (`read_access_unit`).
    fn read_access_unit(&mut self, buf: &[u8], pts: i64) -> Result<(usize, Option<Frame>)> {
        if buf.len() < 4 {
            return Err(decode_error("access unit under 4 bytes"));
        }
        let length = (u16::from_be_bytes([buf[0], buf[1]]) & 0xFFF) as usize * 2;
        if length < 4 || length > buf.len() {
            return Err(decode_error("access unit length is out of range"));
        }
        let body = &buf[4..length];
        let mut gb = Bits::new(body);
        let mut header_size = 4usize;
        self.is_major_sync_unit = false;
        if gb.peek32() >> 1 == 0xF8726FBA >> 1 {
            if let Err(e) = self.read_major_sync(body) {
                self.params_valid = false;
                return Err(e);
            }
            self.is_major_sync_unit = true;
            header_size += self.major_sync_header_size;
            gb.pos = self.major_sync_header_size * 8;
        }
        if !self.params_valid {
            // stream parameters not seen: the frame is skipped
            return Ok((length, None));
        }
        let mut substream_parity_present = [false; MAX_SUBSTREAMS];
        let mut substream_data_len = [0usize; MAX_SUBSTREAMS];
        let mut substream_start = 0usize;
        let mut substr_header_size = 0usize;
        macro_rules! bad {
            ($msg:expr) => {{
                self.params_valid = false;
                return Err(decode_error($msg));
            }};
        }
        for substr in 0..self.num_substreams {
            let extraword_present = gb.bit();
            let nonrestart_substr = gb.bit();
            let checkdata_present = gb.bit();
            gb.skip(1);
            let mut end = gb.bits(12) as usize * 2;
            substr_header_size += 2;
            if extraword_present {
                if !self.truehd {
                    bad!("there must be no extraword for MLP");
                }
                gb.skip(16);
                substr_header_size += 2;
            }
            if length < header_size + substr_header_size {
                bad!("insufficient data for headers");
            }
            if nonrestart_substr == self.is_major_sync_unit {
                bad!("invalid nonrestart_substr");
            }
            if end + header_size + substr_header_size > length {
                end = length - header_size - substr_header_size;
            }
            if end < substream_start {
                bad!("the end offset of a substream's data is before its start");
            }
            if substr > self.max_decoded_substream {
                continue;
            }
            substream_parity_present[substr] = checkdata_present;
            substream_data_len[substr] = end - substream_start;
            substream_start = end;
        }
        let parity_bits = parity(&buf[..4]) ^ parity(&buf[header_size..header_size + substr_header_size]);
        if ((parity_bits >> 4) ^ parity_bits) & 0xF != 0xF {
            bad!("parity check failed");
        }
        let mut pos = header_size + substr_header_size;
        for substr in 0..=self.max_decoded_substream {
            let data = &buf[pos..pos + substream_data_len[substr]];
            let data_len = data.len();
            let mut gb = Bits::new(data);
            self.matrix_changed = 0;
            self.filter_changed = [[0; 2]; MAX_CHANNELS];
            self.substream[substr].blockpos = 0;
            'next_substr: {
                loop {
                    if gb.bit() {
                        if gb.bit() {
                            // a restart header should be present
                            if self.read_restart_header(&mut gb, data, substr).is_err() {
                                break 'next_substr;
                            }
                            self.substream[substr].restart_seen = true;
                        }
                        if !self.substream[substr].restart_seen {
                            break 'next_substr;
                        }
                        if self.read_decoding_params(&mut gb, substr).is_err() {
                            break 'next_substr;
                        }
                    }
                    if !self.substream[substr].restart_seen {
                        break 'next_substr;
                    }
                    let s = &self.substream[substr];
                    if ((self.out_channels == 6 && (self.substream_info >> 2) & 3 != 3)
                        || (self.out_channels == 8 && (self.substream_info >> 4) & 7 != 7 && (self.substream_info >> 4) & 7 != 6 && (self.substream_info >> 4) & 7 != 3))
                        && substr > 0
                        && substr < self.max_decoded_substream
                        && s.min_channel <= self.substream[substr - 1].max_channel
                    {
                        // the previous substream's channels overlap this one's
                        break 'next_substr;
                    }
                    if substr != self.max_decoded_substream && s.coded_channels & self.substream[self.max_decoded_substream].coded_channels != 0 {
                        // this substream's channels overlap the final substream's
                        break 'next_substr;
                    }
                    self.read_block_data(&mut gb, substr)?;
                    if gb.pos >= data_len * 8 {
                        return Err(decode_error("substream length mismatch"));
                    }
                    if gb.bit() {
                        break;
                    }
                }
                let pad = (16 - (gb.pos & 15)) & 15;
                gb.skip(pad);
                if data_len * 8 >= gb.pos + 32 {
                    if gb.bits(16) != 0xD234 {
                        return Err(decode_error("bad end-of-stream marker"));
                    }
                    let shorten_by = gb.bits(16);
                    let s = &mut self.substream[substr];
                    if self.truehd && shorten_by & 0x2000 != 0 {
                        s.blockpos -= ((shorten_by & 0x1FFF) as usize).min(s.blockpos);
                    } else if !self.truehd && shorten_by != 0xD234 {
                        return Err(decode_error("bad MLP end-of-stream marker"));
                    }
                    s.end_of_stream = true;
                }
                if substream_parity_present[substr] {
                    if data_len * 8 != gb.pos + 16 {
                        return Err(decode_error("substream length mismatch"));
                    }
                    let par = parity(&data[..data_len - 2]);
                    // a substream of 2 bytes has nothing for the checksum to cover
                    let checksum = if data_len > 2 { checksum8(data, data_len - 2) } else { 0 };
                    if (gb.bits(8) as u8 ^ par) != 0xA9 {
                        self.check_failures += 1;
                    }
                    if gb.bits(8) as u8 != checksum {
                        self.check_failures += 1;
                    }
                }
                if data_len * 8 != gb.pos {
                    return Err(decode_error("substream length mismatch"));
                }
            }
            pos += data_len;
        }
        let frame = self.output_data(self.max_decoded_substream, pts)?;
        for s in self.substream.iter_mut().take(self.max_decoded_substream + 1) {
            if s.end_of_stream {
                s.lossless_check_data = 0xFFFF_FFFF;
                s.end_of_stream = false;
                self.params_valid = false;
            }
        }
        Ok((length, Some(frame)))
    }
}

impl Decoder for MlpDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        if packet.data.len() < 4 {
            return Err(decode_error("access unit under 4 bytes"));
        }
        let mut frames = Vec::new();
        let mut pos = 0;
        let mut pts = packet.pts;
        while pos < packet.data.len() {
            match self.read_access_unit(&packet.data[pos..], pts) {
                Ok((n, frame)) => {
                    pos += n;
                    if let Some(f) = frame {
                        if let Frame::Audio(a) = &f {
                            pts += a.frames() as i64;
                        }
                        frames.push(f);
                    }
                }
                // an access unit after the first that is bad ends the packet; frames so far are kept
                Err(_) if !frames.is_empty() => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }

    fn reset(&mut self) -> bool {
        self.params_valid = false;
        for s in self.substream.iter_mut() {
            s.lossless_check_data = 0xFFFF_FFFF;
        }
        true
    }
}
