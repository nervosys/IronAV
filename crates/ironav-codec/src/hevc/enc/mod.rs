//! A software HEVC (H.265) encoder: Main profile, 8-bit 4:2:0 with any even dimensions. Pictures are
//! IDR pictures and P pictures that refer to the one before, so the stream is low-delay and needs no
//! reordering by default. With `with_bframes(n)` the frames between anchor pictures (P, every n + 1
//! frames) are coded after the later anchor as B pictures that refer to both anchors (one picture from
//! each list, averaged). B pictures are not referred to, so they are non-reference pictures, and the
//! stream declares one picture of reordering. Coding tree units are 64x64, split into coding units of 8x8 to 32x32 by a
//! rate-distortion decision on the actual reconstruction. Intra units choose one of the 35 modes
//! (a coarse search then refinement) and a chroma mode, and 8x8 units choose one transform or four.
//! Inter units are merged (spatial candidates; a merged unit without residual is skipped) or use one
//! motion vector per unit with quarter-sample refinement, coded with AMVP. Residuals are transformed,
//! quantised with a dead zone by QP and coded with CABAC. The loop filters are the deblocking filter
//! and SAO, whose band or edge offsets are chosen per CTB on the deblocked picture.
//! Slices (rows of CTBs) are coded on threads. Output is Annex B with VPS, SPS and PPS ahead of
//! every IDR picture; the sample-entry configuration is in-band, as for H.264.
//!
//! The structure follows the H.264 encoder beside it. The prediction, transform, quantiser tables
//! and loop filter are the decoder's (the reconstruction is bit-exact by construction); the syntax
//! follows ITU-T H.265 and the CABAC encoder inverts the decoder's engine.

mod cabac;
mod cu;
mod sao;
mod syntax;

use super::filter;
use super::frame::{CurPic, Frame, Geometry, SliceInfo};
use super::ps::{parse_pps, parse_sps, Pps, Sps};
use cu::{lambda_for, Node, Source, SliceCoder};
use ironav_core::{Encoder, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamParams};
use std::ops::Range;
use syntax::{put_nal, Rps, LOG2_CTB};

/// Pictures between IDR pictures.
const KEYINT: u64 = 250;
/// B pictures between anchor pictures by default. None: with the QP cascade, B pictures save about 12 %
/// on the mandelbrot clip at equal PSNR, but on FFmpeg's testsrc2 they save about 3 % on average, too
/// little to be the default (see `with_bframes`).
pub const DEFAULT_BFRAMES: usize = 0;
/// Bits per pixel at quantiser 26, roughly, for the rate control's first guess.
const BPP: f64 = 0.1;
/// The quantiser used when neither a quantiser nor a bit rate is set.
pub const DEFAULT_QP: u8 = 27;
/// Quantiser offsets of IDR, P and B pictures: IDR pictures are a little finer, since later pictures
/// inherit their quality.
const QP_OFFSET: [i32; 3] = [-3, 0, 0];
/// The same with B pictures: anchors a little finer, B pictures (which later pictures never refer to)
/// coarser. Without this cascade B pictures at the anchors' quantiser save little or nothing.
const QP_OFFSET_BFRAMES: [i32; 3] = [-3, -2, 2];
/// The pictures of reordering a stream with `bframes` B pictures between anchors declares.
fn reorder_of(bframes: usize) -> u32 {
    u32::from(bframes > 0)
}

/// The picture kinds, as indices of `QP_OFFSET`.
const IDR: usize = 0;
const P: usize = 1;
const B: usize = 2;

pub struct HevcEncoder {
    width: usize,
    height: usize,
    frame_rate: Rational,
    /// A fixed quantiser, or a bit rate in bits per second to average.
    qp: Option<u8>,
    bitrate: Option<u32>,
    /// Rate control state: the quantiser for the next picture, and bits spent against bits allowed.
    rc_qp: f64,
    spent: f64,
    allowed: f64,
    sps: Sps,
    pps: Pps,
    geo: Geometry,
    vps_rbsp: Vec<u8>,
    sps_rbsp: Vec<u8>,
    pps_rbsp: Vec<u8>,
    level: u8,
    /// B pictures between anchor pictures.
    bframes: usize,
    /// The quantiser offsets of the picture kinds (IDR, P, B) for the stream's B pictures.
    qp_offsets: [i32; 3],
    /// The motion search's coarse grid (see `cu::WIDE_GRID`).
    grid: (i16, i16),
    /// Frames waiting for the anchor picture they are coded with: source, time stamp and display index.
    pending: Vec<(Source, i64, u64)>,
    /// The last two anchor pictures as the decoder reconstructs them: `refs` the last, which the
    /// next anchor refers to, and `prev` the one before, which the B pictures between the two refer to.
    refs: Option<Frame>,
    prev: Option<Frame>,
    /// Frames received, and the display index of the last IDR picture.
    count: u64,
    idr_at: u64,
    /// Pictures between IDR pictures.
    keyint: u64,
    /// Slices per picture, each coded on its own thread.
    slices: usize,
    /// Every picture's reconstruction by its time stamp.
    #[cfg(test)]
    recon: Vec<(i64, [Vec<u16>; 3])>,
}

impl HevcEncoder {
    /// IDR pictures every `n` frames (1 for IDR pictures only), in place of the default.
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    /// `n` B pictures between anchor pictures (0 for none, up to 3), in place of the default. Without
    /// B pictures each picture is sent as soon as it is coded, and the stream declares no reordering.
    /// With B pictures the anchors take a finer quantiser than P pictures would, and the B pictures a
    /// coarser one (`QP_OFFSET_BFRAMES`).
    pub fn with_bframes(mut self, n: usize) -> Self {
        self.bframes = n.min(3);
        self.qp_offsets = if self.bframes > 0 { QP_OFFSET_BFRAMES } else { QP_OFFSET };
        self.grid = if self.bframes > 0 { cu::WIDE_GRID } else { cu::NARROW_GRID };
        let reorder = reorder_of(self.bframes);
        let (pw, ph) = (self.sps.width, self.sps.height);
        let crop = [0, pw - self.width, 0, ph - self.height];
        self.vps_rbsp = syntax::vps(self.level, reorder);
        self.sps_rbsp = syntax::sps(pw, ph, crop, self.level, reorder);
        self.sps = parse_sps(&self.sps_rbsp).expect("the encoder's SPS parses");
        self.geo = Geometry::new(&self.sps, &self.pps);
        self
    }


    /// An encoder for `params` at a constant quantiser (`qp`, 0-51) or an average bit rate in bits per second.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the HEVC encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software HEVC encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::invalid_arg(format!("the software HEVC encoder needs even dimensions, not {width}x{height}")).hint("scale or crop to an even width and height"));
        }
        let (width, height) = (width as usize, height as usize);
        // pictures are padded to whole minimum coding blocks, and the window crops the padding
        let (pw, ph) = (width.div_ceil(8) * 8, height.div_ceil(8) * 8);
        if pw > 8192 || ph > 4320 {
            return Err(Error::invalid_arg(format!("{width}x{height} is larger than the HEVC encoder takes")));
        }
        let level = if pw * ph <= 921_600 {
            93
        } else if pw * ph <= 2_228_224 {
            120
        } else {
            153
        };
        let crop = [0, pw - width, 0, ph - height];
        let sps_rbsp = syntax::sps(pw, ph, crop, level, reorder_of(DEFAULT_BFRAMES));
        let pps_rbsp = syntax::pps();
        let sps = parse_sps(&sps_rbsp)?;
        let pps = parse_pps(&pps_rbsp)?;
        let geo = Geometry::new(&sps, &pps);
        let fps = if frame_rate.num > 0 && frame_rate.den > 0 { frame_rate.num as f64 / frame_rate.den as f64 } else { 25.0 };
        let rc_qp = match bitrate {
            Some(r) => (26.0 - 7.0 * (r as f64 / (fps * width as f64 * height as f64) / BPP).log2()).clamp(10.0, 45.0),
            None => qp.unwrap_or(DEFAULT_QP).min(51) as f64,
        };
        Ok(HevcEncoder {
            width,
            height,
            frame_rate,
            qp: if bitrate.is_none() { Some(qp.unwrap_or(DEFAULT_QP).min(51)) } else { None },
            bitrate,
            rc_qp,
            spent: 0.0,
            allowed: 0.0,
            sps,
            pps,
            geo,
            vps_rbsp: syntax::vps(level, reorder_of(DEFAULT_BFRAMES)),
            sps_rbsp,
            pps_rbsp,
            level,
            bframes: DEFAULT_BFRAMES,
            qp_offsets: QP_OFFSET,
            grid: cu::NARROW_GRID,
            pending: Vec::new(),
            refs: None,
            prev: None,
            count: 0,
            idr_at: 0,
            keyint: KEYINT,
            // a slice per 128 rows or so: more slices lose too much prediction across their edges
            slices: std::thread::available_parallelism().map_or(1, |n| n.get()).min(((ph / 32) / 4).max(1)),
            #[cfg(test)]
            recon: Vec::new(),
        })
    }

    fn fps(&self) -> f64 {
        let r = self.frame_rate;
        if r.num > 0 && r.den > 0 { r.num as f64 / r.den as f64 } else { 25.0 }
    }

    /// The quantiser for the next picture (0 IDR, 1 P).
    fn next_qp(&self, kind: usize) -> i32 {
        let off = self.qp_offsets[kind];
        match self.qp {
            Some(q) => (q as i32 + off).clamp(0, 51),
            None => (self.rc_qp.round() as i32 + off).clamp(1, 51),
        }
    }

    /// Steers the quantiser so the bits spent follow the bits the rate allows (as the H.264 encoder does).
    fn rate_update(&mut self, bits: usize, qp: i32, kind: usize) {
        let Some(rate) = self.bitrate else {
            return;
        };
        let per_frame = rate as f64 / self.fps();
        self.spent += bits as f64;
        self.allowed += per_frame;
        let window = rate as f64;
        let budget = 6.0 * ((self.spent + window) / (self.allowed + window).max(1.0)).log2();
        let expected = per_frame * if kind == 0 { 4.0 } else { 1.0 };
        let wanted = (qp - self.qp_offsets[kind]) as f64 + 6.0 * (bits as f64 / expected).max(1e-3).log2();
        let next = 0.5 * self.rc_qp + 0.5 * wanted + budget;
        self.rc_qp = next.clamp(self.rc_qp - 4.0, self.rc_qp + 4.0).clamp(1.0, 51.0);
    }

    /// The frame's planes padded to the coded size by repeating their edges.
    fn source(&self, v: &ironav_core::VideoFrame) -> Result<Source> {
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != self.width || v.height as usize != self.height || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the HEVC encoder was set up for {}x{} yuv420p frames", self.width, self.height)));
        }
        let (pw, ph) = (self.sps.width, self.sps.height);
        let pad = |p: &[u8], w: usize, h: usize, pw: usize, ph: usize| -> Result<Vec<u16>> {
            if p.len() < w * h {
                return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
            }
            let mut out = vec![0u16; pw * ph];
            for y in 0..ph {
                let row = &p[y.min(h - 1) * w..y.min(h - 1) * w + w];
                for x in 0..pw {
                    out[y * pw + x] = row[x.min(w - 1)] as u16;
                }
            }
            Ok(out)
        };
        let (cw, ch) = (self.width / 2, self.height / 2);
        Ok(Source { planes: [pad(&v.planes[0], self.width, self.height, pw, ph)?, pad(&v.planes[1], cw, ch, pw / 2, ph / 2)?, pad(&v.planes[2], cw, ch, pw / 2, ph / 2)?] })
    }

    /// Codes one picture of `kind` (IDR, P or B) shown at POC `poc`, with time stamp `pts`, its
    /// reference picture set and the pictures of its lists. Returns the packet and the picture as the
    /// decoder reconstructs it.
    fn code_picture(&mut self, src: &Source, pts: i64, poc: i32, kind: usize, rps: &Rps, refs: [Option<&Frame>; 2]) -> (Packet, Frame) {
        let idr = kind == IDR;
        let qp = self.next_qp(kind);
        let (ctb_w, ctb_h) = (self.sps.ctb_w, self.sps.ctb_h);
        let nctb = ctb_w * ctb_h;
        let n = self.slices.clamp(1, ctb_h);
        let bands: Vec<Range<usize>> = (0..n).map(|i| i * ctb_h / n..(i + 1) * ctb_h / n).collect();
        let (sps, pps, geo, grid) = (&self.sps, &self.pps, &self.geo, self.grid);
        let coded: Vec<(SliceCoder, Vec<Node>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = bands
                .iter()
                .enumerate()
                .map(|(i, rows)| {
                    let rows = rows.clone();
                    s.spawn(move || {
                        let coder = SliceCoder::new(sps, pps, geo, src, refs, poc, qp, (rows.start * ctb_w) as i32, i as u16, grid);
                        coder.run(rows)
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().expect("an HEVC slice thread panicked")).collect()
        });
        // the slices' CTB rows make up the picture, which is deblocked, then gets its SAO decided on
        // the deblocked samples and applied by the decoder's filter; the slice data follows, as the
        // SAO syntax needs the parameters
        let mut pic = CurPic::new(sps, pts, poc);
        pic.slices = vec![SliceInfo { deblocking_disabled: false, beta_offset_div2: 0, tc_offset_div2: 0, loop_filter_across_slices: true, cb_qp_offset: 0, cr_qp_offset: 0 }; n];
        for (i, (rows, (part, _))) in bands.iter().zip(&coded).enumerate() {
            merge(&mut pic, &part.pic, rows, ctb_w, nctb, i as u16);
        }
        filter::deblock(&mut pic, sps, pps, geo);
        pic.sao = sao::decide(&pic, src, sps, lambda_for(qp));
        if pic.sao.iter().any(|s| s.kind != [0; 3]) {
            filter::sao(&mut pic, sps, pps, geo);
        }
        let mut data = Vec::new();
        if idr {
            put_nal(&mut data, 32, &self.vps_rbsp);
            put_nal(&mut data, 33, &self.sps_rbsp);
            put_nal(&mut data, 34, &self.pps_rbsp);
        }
        // B pictures are sub-layer non-reference pictures (TRAIL_N); IDR pictures are type 19, others 1
        let nal_kind = match kind {
            IDR => 19,
            B => 0,
            _ => 1,
        };
        for (i, (rows, (part, nodes))) in bands.iter().zip(&coded).enumerate() {
            let mut rbsp = syntax::slice_header(i == 0, rows.start * ctb_w, nctb, idr, kind == B, poc as u32, rps, qp);
            rbsp.extend(part.write(rows.clone(), nodes, &pic.sao));
            put_nal(&mut data, nal_kind, &rbsp);
        }
        self.rate_update(data.len() * 8, qp, kind);
        let frame = pic.frame;
        #[cfg(test)]
        self.recon.push((pts, frame.planes.clone()));
        (Packet { stream: 0, pts, duration: 1, data }, frame)
    }

    /// Codes the frames waiting for an anchor picture as P pictures, each from the one before, for
    /// the end of the stream and for an IDR picture, which no anchor precedes.
    fn code_pending(&mut self) -> Vec<Packet> {
        let mut out = Vec::new();
        for (src, pts, d) in std::mem::take(&mut self.pending) {
            let poc = (d - self.idr_at) as i32;
            let last = self.refs.take().expect("a picture precedes the pending frames");
            let rps = Rps { before: vec![last.poc - poc], after: Vec::new() };
            let (p, rec) = self.code_picture(&src, pts, poc, P, &rps, [Some(&last), None]);
            out.push(p);
            self.prev = Some(last);
            self.refs = Some(rec);
        }
        out
    }

    /// Codes the pending frames with the newest one as an anchor picture: a P picture from the last
    /// anchor, then the others as B pictures between the two anchors, in display order.
    fn code_gop(&mut self) -> Vec<Packet> {
        let mut waiting = std::mem::take(&mut self.pending);
        let Some((src, pts, d)) = waiting.pop() else {
            return Vec::new();
        };
        let poc = (d - self.idr_at) as i32;
        let last = self.refs.take().expect("an IDR picture precedes the anchors");
        let rps = Rps { before: vec![last.poc - poc], after: Vec::new() };
        let (p, rec) = self.code_picture(&src, pts, poc, P, &rps, [Some(&last), None]);
        let mut out = vec![p];
        let (l0, l1) = (Some(last), Some(rec));
        for (src, pts, d) in waiting {
            let poc = (d - self.idr_at) as i32;
            let rps = Rps { before: vec![l0.as_ref().map_or(0, |f| f.poc) - poc], after: vec![l1.as_ref().map_or(0, |f| f.poc) - poc] };
            let (p, _) = self.code_picture(&src, pts, poc, B, &rps, [l0.as_ref(), l1.as_ref()]);
            out.push(p);
        }
        self.prev = l0;
        self.refs = l1;
        out
    }

}

/// Copies the rows of CTBs `rows` of a slice's picture into the picture, with their per-block state.
fn merge(pic: &mut CurPic, part: &CurPic, rows: &Range<usize>, ctb_w: usize, nctb: usize, idx: u16) {
    let ctb = 1usize << LOG2_CTB;
    let (w, h) = (pic.frame.width, pic.frame.height);
    let (y0, y1) = (rows.start * ctb, (rows.end * ctb).min(h));
    for c in 0..3 {
        let (stride, a, b) = if c == 0 { (w, y0, y1) } else { (pic.frame.cwidth, y0 / 2, y1 / 2) };
        pic.frame.planes[c][a * stride..b * stride].copy_from_slice(&part.frame.planes[c][a * stride..b * stride]);
    }
    let w4 = pic.w4;
    let (c0, c1) = (y0 / 4 * w4, y1.div_ceil(4) * w4);
    pic.pred_mode[c0..c1].copy_from_slice(&part.pred_mode[c0..c1]);
    pic.skip[c0..c1].copy_from_slice(&part.skip[c0..c1]);
    pic.ct_depth[c0..c1].copy_from_slice(&part.ct_depth[c0..c1]);
    pic.ipm[c0..c1].copy_from_slice(&part.ipm[c0..c1]);
    pic.qp[c0..c1].copy_from_slice(&part.qp[c0..c1]);
    pic.nz[c0..c1].copy_from_slice(&part.nz[c0..c1]);
    pic.edges[c0..c1].copy_from_slice(&part.edges[c0..c1]);
    pic.frame.mv[c0..c1].copy_from_slice(&part.frame.mv[c0..c1]);
    for rs in rows.start * ctb_w..(rows.end * ctb_w).min(nctb) {
        pic.ctb_slice_addr[rs] = part.ctb_slice_addr[rs];
        pic.ctb_slice_idx[rs] = idx;
    }
}

impl Encoder for HevcEncoder {
    fn encode(&mut self, frame: &ironav_core::Frame) -> Result<Vec<Packet>> {
        let ironav_core::Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the HEVC encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let d = self.count;
        self.count += 1;
        if d == 0 || d - self.idr_at >= self.keyint {
            // an IDR picture: the frames waiting before it are coded first, as P pictures
            let mut out = self.code_pending();
            self.idr_at = d;
            self.refs = None;
            self.prev = None;
            let (p, rec) = self.code_picture(&src, v.pts, 0, IDR, &Rps::default(), [None, None]);
            self.refs = Some(rec);
            out.push(p);
            return Ok(out);
        }
        self.pending.push((src, v.pts, d));
        if self.pending.len() > self.bframes {
            return Ok(self.code_gop());
        }
        Ok(Vec::new())
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(self.code_pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hevc::HevcDecoder;
    use ironav_core::{Decoder, Frame as OutFrame, VideoFrame};

    fn pattern(w: u32, h: u32, t: u32) -> OutFrame {
        let (cw, ch) = ((w / 2) as usize, (h / 2) as usize);
        let y: Vec<u8> = (0..(w * h) as usize)
            .map(|i| {
                let (x, yy) = ((i % w as usize) as u32, (i / w as usize) as u32);
                (((x + 2 * t) * 3 + yy * 2 + ((x ^ yy) & 7) * 5) % 256) as u8
            })
            .collect();
        let c = |k: u32| -> Vec<u8> { (0..cw * ch).map(|i| (((i % cw) as u32 * k + (i / cw) as u32 * 3 + t * (k + 1)) % 256) as u8).collect() };
        OutFrame::Video(VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y, c(5), c(7)] })
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        if se == 0.0 {
            return 99.0;
        }
        10.0 * (255.0 * 255.0 * a.len() as f64 / se).log10()
    }

    /// Encodes `frames` pattern frames, flushing at the end, and returns the packets in decoding order.
    fn encode_all(enc: &mut HevcEncoder, w: u32, h: u32, frames: u32) -> Vec<Packet> {
        let mut pkts = Vec::new();
        for t in 0..frames {
            pkts.extend(enc.encode(&pattern(w, h, t)).unwrap());
        }
        pkts.extend(enc.flush().unwrap());
        pkts
    }

    /// The encoder's reconstruction is what the decoder produces, in display order: for an odd size
    /// whose window crops the padding, for a picture tall enough for two slices, and with and without
    /// B pictures. The packets come in decoding order and the decoder's pictures carry their time stamps.
    #[test]
    fn decoder_reproduces_the_encoder_reconstruction() {
        for bframes in [0usize, 1, 3] {
            for (w, h, frames) in [(178u32, 98u32, 10u32), (320, 544, 9)] {
                let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
                let mut enc = HevcEncoder::new(&params, Some(27), None).unwrap().with_bframes(bframes);
                let pw = enc.sps.width;
                let pkts = encode_all(&mut enc, w, h, frames);
                assert_eq!(pkts.len(), frames as usize);
                let mut dec = HevcDecoder::new();
                let mut out = Vec::new();
                for p in &pkts {
                    out.extend(dec.decode(p).unwrap());
                }
                out.extend(dec.flush().unwrap());
                assert_eq!(out.len(), frames as usize, "{w}x{h} with {bframes} B pictures");
                // the planes as the encoder reconstructed them, cropped to the window
                let crop = |p: &[u16], stride: usize, cw: usize, ch: usize| -> Vec<u8> { (0..ch).flat_map(|r| p[r * stride..r * stride + cw].iter().map(|&s| s as u8)).collect() };
                for (t, f) in out.iter().enumerate() {
                    let OutFrame::Video(v) = f else { panic!("video") };
                    assert_eq!((v.width, v.height), (w, h));
                    assert_eq!(v.pts, t as i64, "output {t} is in display order");
                    let rec = &enc.recon.iter().find(|r| r.0 == t as i64).expect("the picture's reconstruction").1;
                    assert_eq!(v.planes[0], crop(&rec[0], pw, w as usize, h as usize), "{w}x{h} picture {t} luma");
                    assert_eq!(v.planes[1], crop(&rec[1], pw / 2, w as usize / 2, h as usize / 2), "{w}x{h} picture {t} cb");
                    assert_eq!(v.planes[2], crop(&rec[2], pw / 2, w as usize / 2, h as usize / 2), "{w}x{h} picture {t} cr");
                    let src = pattern(w, h, t as u32);
                    let OutFrame::Video(s) = &src else { panic!() };
                    assert!(psnr(&v.planes[0], &s.planes[0]) > 30.0, "{w}x{h} picture {t} PSNR");
                }
            }
        }
    }

    /// With B pictures the packets come in decoding order: each anchor picture, then the frames
    /// before it in display order.
    #[test]
    fn b_pictures_are_sent_after_their_anchor() {
        let params = StreamParams::Video { width: 64, height: 32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = HevcEncoder::new(&params, Some(30), None).unwrap().with_bframes(3);
        let pts: Vec<i64> = encode_all(&mut enc, 64, 32, 10).iter().map(|p| p.pts).collect();
        assert_eq!(pts, [0, 4, 1, 2, 3, 8, 5, 6, 7, 9]);
    }

    /// A bit rate steers the quantiser: the stream lands within a factor of two of its target.
    #[test]
    fn bit_rate_control_lands_near_the_target() {
        let (w, h, frames) = (320u32, 240u32, 30u32);
        let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let rate = 1_500_000u32;
        let mut enc = HevcEncoder::new(&params, None, Some(rate)).unwrap();
        let bytes: usize = encode_all(&mut enc, w, h, frames).iter().map(|p| p.data.len()).sum();
        let target = rate as f64 / 25.0 * frames as f64 / 8.0;
        let ratio = bytes as f64 / target;
        assert!((0.5..2.0).contains(&ratio), "{bytes} bytes for a target of {target:.0}");
    }
}
