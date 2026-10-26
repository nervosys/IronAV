//! An MPEG-1/2/2.5 Layer III (MP3) decoder.
//!
//! The Huffman and band tables, the scale-factor layout, the stereo processing and the
//! short-block reordering follow FFmpeg's `mpegaudiodec_template.c`; the filterbank is computed
//! straight from the standard's definitions (a 36-point IMDCT per subband and the 64x32 matrixing
//! synthesis filter) in `f64`, which is plenty fast for audio.
//!
//! A packet is one whole frame, header included. Layers I and II are refused. After a seek, or at
//! the start of a stream, a frame that points back into data the decoder never saw (the bit
//! reservoir) decodes to silence.

mod tables;

use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};
use std::f64::consts::PI;
use std::sync::OnceLock;
use tables::*;

const BITRATES_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const BITRATES_LSF: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
const RATES_V1: [u32; 3] = [44100, 48000, 32000];

/// A parsed Layer III frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Whether the frame is MPEG-2 or MPEG-2.5 (half or quarter sample rate, one granule).
    pub lsf: bool,
    /// Whether a 16-bit CRC follows the header.
    pub crc: bool,
    pub bitrate: u32,
    pub sample_rate: u32,
    /// Index into the band tables: 0..3 for MPEG-1, 3..6 for MPEG-2, 6..9 for MPEG-2.5.
    pub rate_index: usize,
    /// 0 stereo, 1 joint stereo, 2 dual channel, 3 mono.
    pub mode: u8,
    pub mode_ext: u8,
    /// Whole frame length in bytes, header included.
    pub frame_len: usize,
}

impl Header {
    pub fn channels(&self) -> u16 {
        if self.mode == 3 { 1 } else { 2 }
    }
    /// Samples per channel in the frame.
    pub fn samples(&self) -> usize {
        if self.lsf { 576 } else { 1152 }
    }
    /// Bytes of side information.
    pub fn side_len(&self) -> usize {
        match (self.lsf, self.mode == 3) {
            (false, true) => 17,
            (false, false) => 32,
            (true, true) => 9,
            (true, false) => 17,
        }
    }
}

/// Parse the header at the start of `b`; `None` if it is not a valid Layer III header.
pub fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
    let layer = (b[1] >> 1) & 3;
    if version == 1 || layer != 1 {
        return None;
    }
    let crc = b[1] & 1 == 0;
    let br = (b[2] >> 4) as usize;
    let sr = ((b[2] >> 2) & 3) as usize;
    let padding = ((b[2] >> 1) & 1) as usize;
    if br == 0 || br == 15 || sr == 3 {
        return None;
    }
    let lsf = version != 3;
    let bitrate = if lsf { BITRATES_LSF[br] } else { BITRATES_V1[br] };
    let sample_rate = RATES_V1[sr] >> (if version == 3 { 0 } else if version == 2 { 1 } else { 2 });
    let rate_index = sr + 3 * (if version == 3 { 0 } else if version == 2 { 1 } else { 2 });
    let mode = b[3] >> 6;
    let mode_ext = (b[3] >> 4) & 3;
    let frame_len = (if lsf { 72 } else { 144 }) * bitrate as usize * 1000 / sample_rate as usize + padding;
    Some(Header { lsf, crc, bitrate, sample_rate, rate_index, mode, mode_ext, frame_len })
}

// ---- bits ----------------------------------------------------------------------------------------

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> u32 {
        let b = self.d.get(self.pos >> 3).map_or(0, |&v| (v >> (7 - (self.pos & 7))) & 1);
        self.pos += 1;
        b as u32
    }
    fn get(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }
}

// ---- Huffman -------------------------------------------------------------------------------------

/// A prefix code as a binary tree: `node[i][bit]` is a child index (> 0) or `!symbol` (< 0 as i32).
struct Tree {
    nodes: Vec<[i32; 2]>,
}

impl Tree {
    fn new() -> Self {
        Tree { nodes: vec![[0, 0]] }
    }
    fn insert(&mut self, code: u32, len: u32, sym: u32) {
        let mut at = 0usize;
        for i in (0..len).rev() {
            let bit = ((code >> i) & 1) as usize;
            if i == 0 {
                self.nodes[at][bit] = !(sym as i32);
            } else {
                if self.nodes[at][bit] <= 0 {
                    self.nodes.push([0, 0]);
                    self.nodes[at][bit] = self.nodes.len() as i32 - 1;
                }
                at = self.nodes[at][bit] as usize;
            }
        }
    }
    /// Read one symbol; `None` on a code that is not in the table.
    fn decode(&self, br: &mut Bits) -> Option<u32> {
        let mut at = 0usize;
        loop {
            let n = self.nodes[at][br.bit() as usize];
            if n < 0 {
                return Some(!n as u32);
            }
            if n == 0 {
                return None;
            }
            at = n as usize;
        }
    }
}

struct Statics {
    /// Big-value tables 1..=15 (index 0 unused): symbol = x << 4 | y.
    big: Vec<Tree>,
    quad: [Tree; 2],
    pow43: Vec<f64>,
    /// 2^(e/4) for e in -512..64, offset by 512.
    exp2q: Vec<f64>,
    /// cos(π/72 (2i+19)(2k+1)).
    imdct36: Vec<[f64; 18]>,
    /// cos(π/24 (2i+7)(2k+1)).
    imdct12: Vec<[f64; 6]>,
    /// Block-type windows for the long transform.
    win: [[f64; 36]; 4],
    win_short: [f64; 12],
    /// cos(n(2k+1)π/64), the 32-point transform the synthesis matrixing reduces to.
    matrix: Vec<[f64; 32]>,
    window: [f64; 512],
    cs: [f64; 8],
    ca: [f64; 8],
}

fn statics() -> &'static Statics {
    static S: OnceLock<Statics> = OnceLock::new();
    S.get_or_init(|| {
        // Big-value tables: sequentially assigned prefix codes, as in FFmpeg's VLC init.
        let mut big = vec![Tree::new()];
        let (mut at_len, mut at_sym) = (0usize, 0usize);
        for t in 0..15 {
            let n = HUFF_SIZES_MINUS_ONE[t] as usize + 1;
            let mut tree = Tree::new();
            let mut code = 0u64;
            for i in 0..n {
                let len = HUFF_LENS[at_len + i] as u32;
                tree.insert((code >> (32 - len)) as u32, len, HUFF_SYMBOLS[at_sym + i] as u32);
                code += 1u64 << (32 - len);
            }
            at_len += n;
            at_sym += n;
            big.push(tree);
        }
        let quad = [0, 1].map(|t| {
            let mut tree = Tree::new();
            for s in 0..16 {
                tree.insert(QUAD_CODES[t][s] as u32, QUAD_BITS[t][s] as u32, s as u32);
            }
            tree
        });
        let pow43 = (0..8206 + 16).map(|i| (i as f64).powf(4.0 / 3.0)).collect();
        let exp2q = (-512..64).map(|e| (e as f64 / 4.0).exp2()).collect();
        let imdct36 = (0..36).map(|i| std::array::from_fn(|k| (PI / 72.0 * (2 * i + 19) as f64 * (2 * k + 1) as f64).cos())).collect();
        let imdct12 = (0..12).map(|i| std::array::from_fn(|k| (PI / 24.0 * (2 * i + 7) as f64 * (2 * k + 1) as f64).cos())).collect();
        let mut win = [[0.0; 36]; 4];
        for i in 0..36 {
            let s = (PI / 36.0 * (i as f64 + 0.5)).sin();
            win[0][i] = s;
            win[1][i] = match i {
                0..=17 => s,
                18..=23 => 1.0,
                24..=29 => (PI / 12.0 * (i as f64 - 18.0 + 0.5)).sin(),
                _ => 0.0,
            };
            win[3][i] = match i {
                0..=5 => 0.0,
                6..=11 => (PI / 12.0 * (i as f64 - 6.0 + 0.5)).sin(),
                12..=17 => 1.0,
                _ => s,
            };
        }
        let win_short = std::array::from_fn(|i| (PI / 12.0 * (i as f64 + 0.5)).sin());
        let matrix = (0..32).map(|n| std::array::from_fn(|k| ((n * (2 * k + 1)) as f64 * PI / 64.0).cos())).collect();
        let mut window = [0.0; 512];
        for i in 0..=256 {
            let v = ENWINDOW[i] as f64 / 65536.0;
            window[i] = v;
            if i != 0 {
                window[512 - i] = if i & 63 != 0 { -v } else { v };
            }
        }
        let ci = [-0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037];
        let cs = ci.map(|c: f64| 1.0 / (1.0 + c * c).sqrt());
        let ca = std::array::from_fn(|i| ci[i] * cs[i]);
        Statics { big, quad, pow43, exp2q, imdct36, imdct12, win, win_short, matrix, window, cs, ca }
    })
}

// ---- granules --------------------------------------------------------------------------------------

#[derive(Clone)]
struct Granule {
    part2_3_length: usize,
    big_values: usize,
    global_gain: i32,
    scalefac_compress: usize,
    block_type: u8,
    switch_point: bool,
    table_select: [usize; 3],
    subblock_gain: [i32; 3],
    /// Pairs of coefficients in each Huffman region.
    region_size: [usize; 3],
    preflag: bool,
    scalefac_scale: bool,
    count1table_select: usize,
    scfsi: u8,
    scale_factors: [i32; 40],
    short_start: usize,
    long_end: usize,
    xr: Vec<f64>,
}

impl Default for Granule {
    fn default() -> Self {
        Granule {
            part2_3_length: 0,
            big_values: 0,
            global_gain: 0,
            scalefac_compress: 0,
            block_type: 0,
            switch_point: false,
            table_select: [0; 3],
            subblock_gain: [0; 3],
            region_size: [0; 3],
            preflag: false,
            scalefac_scale: false,
            count1table_select: 0,
            scfsi: 0,
            scale_factors: [0; 40],
            short_start: 13,
            long_end: 22,
            xr: Vec::new(),
        }
    }
}

fn band_index_long(rate: usize, j: usize) -> usize {
    BAND_SIZE_LONG[rate][..j].iter().map(|&s| s as usize >> 1).sum()
}

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("MP3: {msg}"))
}

/// Split `sf` into the digits of a mixed-radix number for the MPEG-2 scale-factor layout.
fn lsf_sf_expand(sf: usize, n1: usize, n2: usize, n3: usize) -> [usize; 4] {
    fn split(sf: &mut usize, n: usize) -> usize {
        let (d, m) = match n {
            3 => (*sf % 3, *sf / 3),
            4 => (*sf & 3, *sf >> 2),
            5 => (*sf % 5, *sf / 5),
            6 => (*sf % 6, *sf / 6),
            _ => (0, *sf),
        };
        *sf = m;
        d
    }
    let mut sf = sf;
    let s3 = split(&mut sf, n3);
    let s2 = split(&mut sf, n2);
    let s1 = split(&mut sf, n1);
    [sf, s1, s2, s3]
}

pub struct Mp3Decoder {
    channels: u16,
    reservoir: Vec<u8>,
    /// Overlap from the previous granule's IMDCT: [channel][subband][18].
    overlap: Vec<[[f64; 18]; 32]>,
    /// Polyphase synthesis history.
    v: Vec<[f64; 1024]>,
    v_off: Vec<usize>,
    granules: Vec<[Granule; 2]>,
}

impl Mp3Decoder {
    pub fn new(params: &StreamParams) -> Result<Self> {
        let channels = match params {
            StreamParams::Audio { channels, .. } => (*channels).clamp(1, 2),
            _ => return Err(Error::new(ErrorCode::InvalidInput, "MP3 needs an audio stream")),
        };
        Ok(Self {
            channels,
            reservoir: Vec::new(),
            overlap: vec![[[0.0; 18]; 32]; 2],
            v: vec![[0.0; 1024]; 2],
            v_off: vec![0; 2],
            granules: vec![Default::default(), Default::default()],
        })
    }

    fn silence(&mut self, h: &Header, pts: i64) -> Vec<Frame> {
        let n = h.samples() * self.channels as usize;
        vec![Frame::Audio(AudioFrame { sample_rate: h.sample_rate, channels: self.channels, pts, samples: vec![0.0; n] })]
    }

    fn decode_frame(&mut self, p: &Packet) -> Result<Vec<Frame>> {
        let h = parse_header(&p.data).ok_or_else(|| bad("not a Layer III frame header (layers I and II are not supported)"))?;
        let nch = h.channels() as usize;
        let hdr = 4 + if h.crc { 2 } else { 0 };
        let end = h.frame_len.min(p.data.len());
        if end < hdr + h.side_len() {
            return Err(bad("frame is shorter than its side information"));
        }
        let side = &p.data[hdr..hdr + h.side_len()];
        let main = &p.data[hdr + h.side_len()..end];
        let mut br = Bits { d: side, pos: 0 };
        let ngr = if h.lsf { 1 } else { 2 };
        let main_data_begin;
        if h.lsf {
            main_data_begin = br.get(8) as usize;
            br.get(nch as u32);
        } else {
            main_data_begin = br.get(9) as usize;
            br.get(if nch == 2 { 3 } else { 5 });
            for ch in 0..nch {
                self.granules[ch][0].scfsi = 0;
                self.granules[ch][1].scfsi = br.get(4) as u8;
            }
        }
        let s = statics();
        for gr in 0..ngr {
            for ch in 0..nch {
                let g = &mut self.granules[ch][gr];
                g.part2_3_length = br.get(12) as usize;
                g.big_values = br.get(9) as usize;
                if g.big_values > 288 {
                    return Err(bad("big_values is too big"));
                }
                g.global_gain = br.get(8) as i32;
                g.scalefac_compress = br.get(if h.lsf { 9 } else { 4 }) as usize;
                if br.get(1) == 1 {
                    g.block_type = br.get(2) as u8;
                    if g.block_type == 0 {
                        return Err(bad("invalid block type"));
                    }
                    g.switch_point = br.get(1) == 1;
                    for i in 0..2 {
                        g.table_select[i] = br.get(5) as usize;
                    }
                    g.table_select[2] = 0;
                    for i in 0..3 {
                        g.subblock_gain[i] = br.get(3) as i32;
                    }
                    // init_short_region
                    g.region_size[0] = if g.block_type == 2 {
                        if h.rate_index != 8 { 18 } else { 36 }
                    } else if h.rate_index <= 2 {
                        18
                    } else if h.rate_index != 8 {
                        27
                    } else {
                        54
                    };
                    g.region_size[1] = 288;
                } else {
                    g.block_type = 0;
                    g.switch_point = false;
                    g.subblock_gain = [0; 3];
                    for i in 0..3 {
                        g.table_select[i] = br.get(5) as usize;
                    }
                    let ra1 = br.get(4) as usize;
                    let ra2 = br.get(3) as usize;
                    g.region_size[0] = band_index_long(h.rate_index, ra1 + 1);
                    g.region_size[1] = band_index_long(h.rate_index, (ra1 + ra2 + 2).min(22));
                }
                // region_offset2size
                g.region_size[2] = 288;
                let mut j = 0;
                for i in 0..3 {
                    let k = g.region_size[i].min(g.big_values);
                    g.region_size[i] = k - j;
                    j = k;
                }
                // compute_band_indexes
                if g.block_type == 2 {
                    if g.switch_point {
                        g.long_end = if h.rate_index <= 2 { 8 } else { 6 };
                        g.short_start = 3;
                    } else {
                        g.long_end = 0;
                        g.short_start = 0;
                    }
                } else {
                    g.short_start = 13;
                    g.long_end = 22;
                }
                g.preflag = if h.lsf { false } else { br.get(1) == 1 };
                g.scalefac_scale = br.get(1) == 1;
                g.count1table_select = br.get(1) as usize;
            }
        }

        // The bit reservoir: this frame's main data follows whatever the earlier frames left.
        let have = self.reservoir.len();
        let usable = main_data_begin <= have;
        let mut data: Vec<u8> = Vec::new();
        if usable {
            data.extend_from_slice(&self.reservoir[have - main_data_begin..]);
        }
        data.extend_from_slice(main);
        self.reservoir.extend_from_slice(main);
        if self.reservoir.len() > 512 {
            let cut = self.reservoir.len() - 512;
            self.reservoir.drain(..cut);
        }

        let mut out = vec![0.0f64; h.samples() * nch];
        let mut bitpos = 0usize;
        for gr in 0..ngr {
            for ch in 0..nch {
                let start = bitpos;
                bitpos += self.granules[ch][gr].part2_3_length;
                let sc0 = self.granules[ch][0].scale_factors;
                let g = &mut self.granules[ch][gr];
                g.xr.clear();
                g.xr.resize(576, 0.0);
                if !usable {
                    continue;
                }
                let mut bits = Bits { d: &data, pos: start };
                read_scale_factors(&h, g, &mut bits, ch, &sc0);
                let exps = exponents(&h, g);
                huffman(s, g, &mut bits, &exps, start + g.part2_3_length);
            }
            if h.mode == 1 {
                let (a, b) = self.granules.split_at_mut(1);
                compute_stereo(&h, &mut a[0][gr], &mut b[0][gr]);
            }
            for ch in 0..nch {
                let g = &mut self.granules[ch][gr];
                reorder_block(&h, g);
                antialias(s, g);
                let mut sb = [[0.0f64; 32]; 18];
                imdct(s, g, &mut self.overlap[ch], &mut sb);
                for t in 0..18 {
                    let mut pcm = [0.0f64; 32];
                    synth(s, &mut self.v[ch], &mut self.v_off[ch], &sb[t], &mut pcm);
                    let base = (gr * 18 + t) * 32;
                    for (i, &x) in pcm.iter().enumerate() {
                        out[(base + i) * nch + ch] = x;
                    }
                }
            }
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: h.sample_rate, channels: nch as u16, pts: p.pts, samples: out })])
    }
}

fn read_scale_factors(h: &Header, g: &mut Granule, br: &mut Bits, ch: usize, sc0: &[i32; 40]) {
    if !h.lsf {
        let slen1 = SLEN[0][g.scalefac_compress] as u32;
        let slen2 = SLEN[1][g.scalefac_compress] as u32;
        if g.block_type == 2 {
            let n = if g.switch_point { 17 } else { 18 };
            let mut j = 0;
            for _ in 0..n {
                g.scale_factors[j] = if slen1 > 0 { br.get(slen1) as i32 } else { 0 };
                j += 1;
            }
            for _ in 0..18 {
                g.scale_factors[j] = if slen2 > 0 { br.get(slen2) as i32 } else { 0 };
                j += 1;
            }
            for _ in 0..3 {
                g.scale_factors[j] = 0;
                j += 1;
            }
        } else {
            let mut j = 0;
            for k in 0..4 {
                let n = if k == 0 { 6 } else { 5 };
                if g.scfsi & (0x8 >> k) == 0 {
                    let slen = if k < 2 { slen1 } else { slen2 };
                    for _ in 0..n {
                        g.scale_factors[j] = if slen > 0 { br.get(slen) as i32 } else { 0 };
                        j += 1;
                    }
                } else {
                    for _ in 0..n {
                        g.scale_factors[j] = sc0[j];
                        j += 1;
                    }
                }
            }
            g.scale_factors[j] = 0;
        }
    } else {
        let tindex = if g.block_type == 2 { if g.switch_point { 2 } else { 1 } } else { 0 };
        let mut sf = g.scalefac_compress;
        let slen;
        let tindex2;
        if h.mode_ext & 1 != 0 && ch == 1 {
            sf >>= 1;
            if sf < 180 {
                slen = lsf_sf_expand(sf, 6, 6, 0);
                tindex2 = 3;
            } else if sf < 244 {
                slen = lsf_sf_expand(sf - 180, 4, 4, 0);
                tindex2 = 4;
            } else {
                slen = lsf_sf_expand(sf - 244, 3, 0, 0);
                tindex2 = 5;
            }
        } else if sf < 400 {
            slen = lsf_sf_expand(sf, 5, 4, 4);
            tindex2 = 0;
        } else if sf < 500 {
            slen = lsf_sf_expand(sf - 400, 5, 4, 0);
            tindex2 = 1;
        } else {
            slen = lsf_sf_expand(sf - 500, 3, 0, 0);
            tindex2 = 2;
            g.preflag = true;
        }
        let mut j = 0;
        for k in 0..4 {
            let n = LSF_NSF[tindex2][tindex][k] as usize;
            for _ in 0..n {
                g.scale_factors[j] = if slen[k] > 0 { br.get(slen[k] as u32) as i32 } else { 0 };
                j += 1;
            }
        }
        while j < 40 {
            g.scale_factors[j] = 0;
            j += 1;
        }
    }
}

/// The exponent of 2^(e/4) applied to each of the 576 coefficients.
fn exponents(h: &Header, g: &Granule) -> [i32; 576] {
    let mut e = [0i32; 576];
    let mut at = 0;
    let gain = g.global_gain - 210;
    let shift = g.scalefac_scale as i32 + 1;
    let pretab = &PRETAB[g.preflag as usize];
    for i in 0..g.long_end {
        let v0 = gain - ((g.scale_factors[i] + pretab[i] as i32) << shift);
        for _ in 0..BAND_SIZE_LONG[h.rate_index][i] {
            e[at] = v0;
            at += 1;
        }
    }
    if g.short_start < 13 {
        let gains = g.subblock_gain.map(|s| gain - (s << 3));
        let mut k = g.long_end;
        for i in g.short_start..13 {
            let len = BAND_SIZE_SHORT[h.rate_index][i];
            for l in 0..3 {
                let v0 = gains[l] - (g.scale_factors[k] << shift);
                k += 1;
                for _ in 0..len {
                    e[at] = v0;
                    at += 1;
                }
            }
        }
    }
    e
}

fn dequant(s: &Statics, x: usize, e: i32) -> f64 {
    s.pow43[x] * s.exp2q[(e + 512).clamp(0, 575) as usize]
}

fn huffman(s: &Statics, g: &mut Granule, br: &mut Bits, exps: &[i32; 576], end: usize) {
    let mut idx = 0;
    for r in 0..3 {
        let mut pairs = g.region_size[r];
        if pairs == 0 {
            continue;
        }
        let k = g.table_select[r];
        let (table, linbits) = (HUFF_DATA[k][0] as usize, HUFF_DATA[k][1] as u32);
        if table == 0 {
            idx += 2 * pairs;
            continue;
        }
        let tree = &s.big[table];
        while pairs > 0 {
            if br.pos >= end {
                break;
            }
            let Some(sym) = tree.decode(br) else { break };
            let (mut x, mut y) = ((sym >> 4) as usize, (sym & 15) as usize);
            let e = exps[idx];
            let mut vx = 0.0;
            let mut vy = 0.0;
            if x != 0 {
                if x == 15 && linbits > 0 {
                    x += br.get(linbits) as usize;
                }
                vx = dequant(s, x, e);
                if br.bit() == 1 {
                    vx = -vx;
                }
            }
            if y != 0 {
                if y == 15 && linbits > 0 {
                    y += br.get(linbits) as usize;
                }
                vy = dequant(s, y, exps[idx + 1]);
                if br.bit() == 1 {
                    vy = -vy;
                }
            }
            g.xr[idx] = vx;
            g.xr[idx + 1] = vy;
            idx += 2;
            pairs -= 1;
        }
    }
    // the "count1" region: quadruples of -1, 0 and 1
    let tree = &s.quad[g.count1table_select];
    let mut last = 0usize;
    while idx <= 572 {
        let pos = br.pos;
        if pos >= end {
            if pos > end && last != 0 {
                // some encoders get this size wrong: undo the last quadruple
                idx -= 4;
                br.pos = last;
                for v in &mut g.xr[idx..idx + 4] {
                    *v = 0.0;
                }
            }
            break;
        }
        last = pos;
        let Some(code) = tree.decode(br) else { break };
        for q in 0..4 {
            if code & (8 >> q) != 0 {
                let v = dequant(s, 1, exps[idx + q]);
                g.xr[idx + q] = if br.bit() == 1 { -v } else { v };
            }
        }
        idx += 4;
    }
}

fn reorder_block(h: &Header, g: &mut Granule) {
    if g.block_type != 2 {
        return;
    }
    let mut at = if g.switch_point { if h.rate_index != 8 { 36 } else { 72 } } else { 0 };
    let mut tmp = [0.0f64; 576];
    for i in g.short_start..13 {
        let len = BAND_SIZE_SHORT[h.rate_index][i] as usize;
        for j in 0..len {
            tmp[3 * j] = g.xr[at + j];
            tmp[3 * j + 1] = g.xr[at + len + j];
            tmp[3 * j + 2] = g.xr[at + 2 * len + j];
        }
        g.xr[at..at + 3 * len].copy_from_slice(&tmp[..3 * len]);
        at += 3 * len;
    }
}

fn compute_stereo(h: &Header, g0: &mut Granule, g1: &mut Granule) {
    const ISQRT2: f64 = std::f64::consts::FRAC_1_SQRT_2;
    let ms = h.mode_ext & 2 != 0;
    if h.mode_ext & 1 != 0 {
        // intensity stereo
        let (is_tab, sf_max): ([[f64; 16]; 2], i32) = if !h.lsf {
            let mut t = [[0.0; 16]; 2];
            for sf in 0..7 {
                let r = (sf as f64 * PI / 12.0).tan();
                t[0][sf] = r / (1.0 + r);
                t[1][sf] = 1.0 / (1.0 + r);
            }
            t[0][6] = 1.0;
            t[1][6] = 0.0;
            (t, 7)
        } else {
            let mut t = [[0.0; 16]; 2];
            let j = g1.scalefac_compress & 1;
            for i in 0..16 {
                let e = -((j + 1) as i32) * ((i as i32 + 1) >> 1);
                let f = (e as f64 / 4.0).exp2();
                let k = i & 1;
                t[k ^ 1][i] = f;
                t[k][i] = 1.0;
            }
            (t, 16)
        };
        let mut at = 576usize;
        let mut nz_short = [false; 3];
        let mut k = (13 - g1.short_start) * 3 + g1.long_end;
        k = k.wrapping_sub(3);
        let apply = |g0: &mut Granule, g1: &mut Granule, at: usize, len: usize, sf: Option<usize>| {
            match sf {
                Some(sf) => {
                    let (v1, v2) = (is_tab[0][sf], is_tab[1][sf]);
                    for j in 0..len {
                        let t = g0.xr[at + j];
                        g0.xr[at + j] = t * v1;
                        g1.xr[at + j] = t * v2;
                    }
                }
                None => {
                    if ms {
                        for j in 0..len {
                            let (a, b) = (g0.xr[at + j], g1.xr[at + j]);
                            g0.xr[at + j] = (a + b) * ISQRT2;
                            g1.xr[at + j] = (a - b) * ISQRT2;
                        }
                    }
                }
            }
        };
        let mut i = 12isize;
        while i >= g1.short_start as isize {
            if i != 11 {
                k = k.wrapping_sub(3);
            }
            let len = BAND_SIZE_SHORT[h.rate_index][i as usize] as usize;
            for l in (0..3).rev() {
                at -= len;
                if !nz_short[l] {
                    if g1.xr[at..at + len].iter().any(|&v| v != 0.0) {
                        nz_short[l] = true;
                        apply(g0, g1, at, len, None);
                        continue;
                    }
                    let sf = g1.scale_factors[k + l];
                    if sf >= sf_max {
                        apply(g0, g1, at, len, None);
                        continue;
                    }
                    apply(g0, g1, at, len, Some(sf as usize));
                } else {
                    apply(g0, g1, at, len, None);
                }
            }
            i -= 1;
        }
        let mut nz = nz_short[0] | nz_short[1] | nz_short[2];
        for i in (0..g1.long_end).rev() {
            let len = BAND_SIZE_LONG[h.rate_index][i] as usize;
            at -= len;
            if !nz {
                if g1.xr[at..at + len].iter().any(|&v| v != 0.0) {
                    nz = true;
                    apply(g0, g1, at, len, None);
                    continue;
                }
                let sf = g1.scale_factors[if i == 21 { 20 } else { i }];
                if sf >= sf_max {
                    apply(g0, g1, at, len, None);
                    continue;
                }
                apply(g0, g1, at, len, Some(sf as usize));
            } else {
                apply(g0, g1, at, len, None);
            }
        }
    } else if ms {
        for i in 0..576 {
            let (a, b) = (g0.xr[i], g1.xr[i]);
            g0.xr[i] = (a + b) * ISQRT2;
            g1.xr[i] = (a - b) * ISQRT2;
        }
    }
}

fn antialias(s: &Statics, g: &mut Granule) {
    let n = if g.block_type == 2 {
        if !g.switch_point {
            return;
        }
        1
    } else {
        31
    };
    for sb in 1..=n {
        let base = 18 * sb;
        for i in 0..8 {
            let (a, b) = (g.xr[base - 1 - i], g.xr[base + i]);
            g.xr[base - 1 - i] = a * s.cs[i] - b * s.ca[i];
            g.xr[base + i] = b * s.cs[i] + a * s.ca[i];
        }
    }
}

/// Inverse MDCT with windowing and overlap-add; `out[t][sb]` receives the 18 time samples of each
/// subband (with the odd samples of odd subbands inverted).
fn imdct(s: &Statics, g: &Granule, overlap: &mut [[f64; 18]; 32], out: &mut [[f64; 32]; 18]) {
    for sb in 0..32 {
        let x = &g.xr[18 * sb..18 * sb + 18];
        let mut z = [0.0f64; 36];
        // most encoders leave the top subbands empty
        let silent = x.iter().all(|&v| v == 0.0);
        let short = g.block_type == 2 && !(g.switch_point && sb < 2);
        if silent {
        } else if short {
            for w in 0..3 {
                for i in 0..12 {
                    let mut acc = 0.0;
                    for k in 0..6 {
                        acc += x[3 * k + w] * s.imdct12[i][k];
                    }
                    z[6 + 6 * w + i] += acc * s.win_short[i];
                }
            }
        } else {
            let win = &s.win[if g.switch_point && sb < 2 { 0 } else { g.block_type as usize }];
            for i in 0..36 {
                let mut acc = 0.0;
                for k in 0..18 {
                    acc += x[k] * s.imdct36[i][k];
                }
                z[i] = acc * win[i];
            }
        }
        for t in 0..18 {
            let v = z[t] + overlap[sb][t];
            overlap[sb][t] = z[t + 18];
            out[t][sb] = if sb & 1 == 1 && t & 1 == 1 { -v } else { v };
        }
    }
}

/// One step of the polyphase synthesis filterbank: 32 subband samples in, 32 PCM samples out.
fn synth(s: &Statics, v: &mut [f64; 1024], off: &mut usize, sb: &[f64; 32], out: &mut [f64; 32]) {
    *off = (*off + 1024 - 64) & 1023;
    // V[i] = sum_k cos((16+i)(2k+1)pi/64) S[k] needs only d[n] = sum_k cos(n(2k+1)pi/64) S[k], n < 32:
    // the cosine is symmetric about n = 32 (where it is 0) and antisymmetric about n = 64.
    let mut d = [0.0f64; 32];
    for (n, d) in d.iter_mut().enumerate() {
        let m = &s.matrix[n];
        let mut acc = 0.0;
        for k in 0..32 {
            acc += m[k] * sb[k];
        }
        *d = acc;
    }
    for i in 0..64 {
        let x = match i {
            0..=15 => d[16 + i],
            16 => 0.0,
            17..=47 => -d[48 - i],
            _ => -d[i - 48],
        };
        v[(*off + i) & 1023] = x;
    }
    for j in 0..32 {
        let mut acc = 0.0;
        for i in 0..8 {
            acc += s.window[64 * i + j] * v[(*off + 128 * i + j) & 1023];
            acc += s.window[64 * i + 32 + j] * v[(*off + 128 * i + 96 + j) & 1023];
        }
        out[j] = acc;
    }
}

impl Decoder for Mp3Decoder {
    fn decode(&mut self, p: &Packet) -> Result<Vec<Frame>> {
        match self.decode_frame(p) {
            Ok(f) => Ok(f),
            Err(e) => {
                // a damaged frame is silence, like the reference decoder's concealment-free path
                let Some(h) = parse_header(&p.data) else { return Err(e) };
                if self.channels != h.channels() {
                    return Err(e);
                }
                Ok(self.silence(&h, p.pts))
            }
        }
    }
}
