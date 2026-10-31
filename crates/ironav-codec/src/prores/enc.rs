//! The ProRes encoder: a forward DCT, one fixed quantiser per profile, and the entropy coding of
//! FFmpeg's `prores_ks` (the same DC differences and run/level codewords). The frames carry no
//! quantisation matrices, so the decoder uses flat ones, and each coefficient is the orthonormal
//! transform's divided by the quantiser (by four more at 12 bits, whose samples are four times as
//! large as 10-bit ones).

use super::{block_offsets, mb_width, slices, Profile, DC_CB, FIRST_DC_CB, FRAME_ID, LEVEL_CB, RUN_CB, SCAN};
use ironav_core::{Encoder, Error, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};

pub struct ProResEncoder {
    profile: Profile,
    width: usize,
    height: usize,
    pix_fmt: PixelFormat,
}

impl ProResEncoder {
    /// An encoder for `params`, a video stream of 4:2:2 10-bit (`yuv422p10le`) for the 4:2:2
    /// profiles, or 4:4:4 at 10 or 12 bits for the 4444 ones.
    pub fn new(params: &StreamParams, profile: Profile) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, .. } = *params else {
            return Err(Error::invalid_arg("the ProRes encoder takes a video stream"));
        };
        let takes = if profile.is_444() { &[PixelFormat::Yuv444p10, PixelFormat::Yuv444p12][..] } else { &[PixelFormat::Yuv422p10][..] };
        if !takes.contains(&pix_fmt) {
            return Err(Error::invalid_arg(format!("ProRes {profile:?} takes {}, not {}", takes[0].name(), pix_fmt.name())));
        }
        if width == 0 || height == 0 || width > 65535 || height > 65535 {
            return Err(Error::invalid_arg("ProRes frames are 1 to 65535 pixels on each side"));
        }
        Ok(Self { profile, width: width as usize, height: height as usize, pix_fmt })
    }

    fn encode_frame(&self, v: &VideoFrame) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let (mbw, mbh) = (w.div_ceil(16), h.div_ceil(16));
        let c444 = self.profile.is_444();
        let twelve = c444;
        // 10-bit samples of a 4:4:4 profile go up to 12 bits
        let shift = if twelve && self.pix_fmt == PixelFormat::Yuv444p10 { 2 } else { 0 };
        let layout = self.pix_fmt.planes(w, h);
        let planes: Vec<(Vec<u16>, usize)> = (0..3)
            .map(|p| {
                let (pw, ph, _) = layout[p];
                let src: Vec<u16> = v.planes[p].chunks(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
                let stride = mbw * mb_width(p, c444);
                let mut out = vec![0u16; stride * mbh * 16];
                for y in 0..mbh * 16 {
                    let row = &src[y.min(ph - 1) * pw..];
                    for x in 0..stride {
                        out[y * stride + x] = row[x.min(pw - 1)] << shift;
                    }
                }
                (out, stride)
            })
            .collect();

        let mut out = vec![0u8; 4];
        out.extend_from_slice(FRAME_ID);
        // frame header: 20 bytes, with no quantisation matrices
        out.extend_from_slice(&20u16.to_be_bytes());
        out.extend_from_slice(&(c444 as u16).to_be_bytes());
        out.extend_from_slice(b"IRON");
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.push(if c444 { 0xC0 } else { 0x80 });
        // reserved, colour primaries, transfer and matrix (unspecified), alpha, reserved, matrix flags
        out.extend_from_slice(&[0; 7]);
        // picture header: 8 bytes, the slice table, then the slices
        let pic = out.len();
        let list = slices(mbw, mbh);
        out.push(0x40);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(list.len() as u16).to_be_bytes());
        out.push(3 << 4);
        let table = out.len();
        out.extend(vec![0u8; 2 * list.len()]);
        let q = self.profile.qscale();
        for (i, &(x, y, n)) in list.iter().enumerate() {
            let mut planes_bytes = Vec::with_capacity(3);
            for (p, (src, stride)) in planes.iter().enumerate() {
                planes_bytes.push(self.encode_plane(src, *stride, p, (x, y), n, twelve));
            }
            let (ys, us) = (planes_bytes[0].len() as u16, planes_bytes[1].len() as u16);
            let mut s = vec![6 << 3, q as u8];
            s.extend_from_slice(&ys.to_be_bytes());
            s.extend_from_slice(&us.to_be_bytes());
            for b in &planes_bytes {
                s.extend_from_slice(b);
            }
            out[table + 2 * i..table + 2 * i + 2].copy_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(&s);
        }
        let pic_len = (out.len() - pic) as u32;
        out[pic + 1..pic + 5].copy_from_slice(&pic_len.to_be_bytes());
        let len = out.len() as u32;
        out[..4].copy_from_slice(&len.to_be_bytes());
        out
    }

    /// The entropy-coded coefficients of one plane's slice of `n` macroblocks at `(mbx, mby)`.
    fn encode_plane(&self, src: &[u16], stride: usize, plane: usize, (mbx, mby): (usize, usize), n: usize, twelve: bool) -> Vec<u8> {
        let c444 = self.profile.is_444();
        let offs = block_offsets(plane, c444);
        let mbw_px = mb_width(plane, c444);
        let nb = n * offs.len();
        let (bias, ks) = if twelve { (2048.0, 4.0) } else { (512.0, 1.0) };
        let step = ks * self.profile.qscale() as f64;
        let mut lv = vec![0i32; nb * 64];
        for k in 0..n {
            for (t, &(dx, dy)) in offs.iter().enumerate() {
                let b = (k * offs.len() + t) * 64;
                let (x0, y0) = ((mbx + k) * mbw_px + dx, mby * 16 + dy);
                let mut f = [0f64; 64];
                for r in 0..8 {
                    for c in 0..8 {
                        f[r * 8 + c] = src[(y0 + r) * stride + x0 + c] as f64 - bias;
                    }
                }
                for (l, c) in lv[b..b + 64].iter_mut().zip(fdct(&f)) {
                    *l = (c / step).round() as i32;
                }
            }
        }
        let mut bw = BitWriter::default();
        encode_dcs(&mut bw, &lv, nb);
        encode_acs(&mut bw, &lv, nb);
        bw.finish()
    }
}

/// The 8x8 forward DCT of `f` (raster order), orthonormal: the same scale the decoder inverts.
fn fdct(f: &[f64; 64]) -> [f64; 64] {
    let t: [[f64; 8]; 8] = std::array::from_fn(|u| {
        std::array::from_fn(|x| {
            let cu = if u == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
            cu / 2.0 * (((2 * x + 1) * u) as f64 * std::f64::consts::PI / 16.0).cos()
        })
    });
    let mut tmp = [0f64; 64];
    for r in 0..8 {
        for u in 0..8 {
            tmp[r * 8 + u] = (0..8).map(|x| f[r * 8 + x] * t[u][x]).sum();
        }
    }
    let mut out = [0f64; 64];
    for v in 0..8 {
        for u in 0..8 {
            out[v * 8 + u] = (0..8).map(|r| t[v][r] * tmp[r * 8 + u]).sum();
        }
    }
    out
}

/// Bits, most significant first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    n: u8,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b & 1) as u8;
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }

    fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit(v >> i);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        while self.n != 0 {
            self.bit(0);
        }
        self.out
    }
}

/// The zigzag code of a signed value, as FFmpeg's MAKE_CODE.
fn make_code(x: i32) -> u32 {
    ((x * 2) ^ (x >> 31)) as u32
}

/// A Rice or exp-Golomb codeword of `val` in codebook `cb` (FFmpeg's encode_vlc_codeword).
fn vlc(bw: &mut BitWriter, cb: u8, val: u32) {
    let switch_bits = (cb & 3) as u32 + 1;
    let rice = (cb >> 5) as u32;
    let exp = ((cb >> 2) & 7) as u32;
    let switch_val = switch_bits << rice;
    if val >= switch_val {
        let v = val + (1 << exp) - switch_val;
        let e = 31 - v.leading_zeros();
        bw.put(e - exp + switch_bits, 0);
        bw.put(e + 1, v);
    } else {
        let e = val >> rice;
        bw.put(e, 0);
        bw.put(1, 1);
        if rice > 0 {
            bw.put(rice, val & ((1 << rice) - 1));
        }
    }
}

fn encode_dcs(bw: &mut BitWriter, lv: &[i32], nb: usize) {
    let mut prev = lv[0];
    vlc(bw, FIRST_DC_CB, make_code(prev));
    let (mut sign, mut cb) = (0i32, 5usize);
    for b in 1..nb {
        let cur = lv[b * 64];
        let delta = cur - prev;
        let new_sign = if delta < 0 { -1 } else { 0 };
        let code = make_code((delta ^ sign) - sign);
        vlc(bw, DC_CB[cb], code);
        cb = code.min(6) as usize;
        sign = new_sign;
        prev = cur;
    }
}

fn encode_acs(bw: &mut BitWriter, lv: &[i32], nb: usize) {
    let (mut prev_run, mut prev_level, mut run) = (4usize, 2usize, 0usize);
    for &s in &SCAN[1..] {
        for b in 0..nb {
            let level = lv[b * 64 + s as usize];
            if level != 0 {
                let abs = level.unsigned_abs();
                vlc(bw, RUN_CB[prev_run], run as u32);
                vlc(bw, LEVEL_CB[prev_level], abs - 1);
                bw.put(1, (level < 0) as u32);
                prev_run = run.min(15);
                prev_level = abs.min(9) as usize;
                run = 0;
            } else {
                run += 1;
            }
        }
    }
}

impl Encoder for ProResEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the ProRes encoder received an audio frame"));
        };
        if (v.width as usize, v.height as usize) != (self.width, self.height) || v.pix_fmt != self.pix_fmt {
            return Err(Error::invalid_input(format!(
                "the ProRes encoder was set up for {}x{} {}, and got {}x{} {}",
                self.width,
                self.height,
                self.pix_fmt.name(),
                v.width,
                v.height,
                v.pix_fmt.name()
            )));
        }
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data: self.encode_frame(v) }])
    }
}
