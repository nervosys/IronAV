//! A software AV1 decoder. Stage 1: key frames and intra-only frames at 8, 10 and 12 bits, every
//! subsampling, tiles, palettes, intra block filters and lossless coding, with the deblocking
//! filter. Inter frames, CDEF, loop restoration, superres, film grain, intra block copy and
//! quantiser matrices are refused with an error rather than decoded wrongly.

pub mod block;
pub mod cdf_tables;
pub mod ipred;
pub mod itx;
pub mod itx1d;
pub mod lf;
pub mod obu;
pub mod quant_tables;
pub mod scan_tables;
pub mod symbol;
pub mod tables;

use std::sync::Arc;

use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};

use block::FrameDec;
use ipred::Plane;
use obu::*;
use symbol::Cdfs;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("AV1: {msg}"))
}

/// A decoded picture as the reference slots keep it.
struct Picture {
    planes: [Plane; 3],
    width: usize,
    height: usize,
    bd: u32,
    ss: [usize; 2],
    mono: bool,
    rgb: bool,
}

#[derive(Clone)]
struct RefSlot {
    pic: Arc<Picture>,
    frame_type: u8,
    order_hint: u32,
    /// The CDFs the frame ended with, for frames that load them (inter frames, later stages).
    #[allow(dead_code)]
    cdfs: Arc<Cdfs>,
}

/// The frame whose tile groups are being decoded.
struct Current {
    f: FrameDec,
    cdf: Box<Cdfs>,
    saved: Option<Box<Cdfs>>,
}

pub struct Av1Decoder {
    seq: Option<SequenceHeader>,
    refs: [Option<RefSlot>; 8],
    cur: Option<Current>,
    out: Vec<Frame>,
}

impl Av1Decoder {
    /// `extradata` is the av1C box (or Matroska CodecPrivate), whose config OBUs may hold the
    /// sequence header.
    pub fn new(extradata: &[u8]) -> Self {
        let mut d = Av1Decoder { seq: None, refs: Default::default(), cur: None, out: Vec::new() };
        if extradata.len() > 4 && extradata[0] & 0x80 != 0 {
            if let Ok(obus) = split_obus(&extradata[4..]) {
                for o in obus.iter().filter(|o| o.kind == OBU_SEQUENCE_HEADER) {
                    d.seq = parse_sequence_header(o.payload).ok();
                }
            }
        }
        d
    }

    fn ref_info(&self) -> RefInfo {
        let mut r = RefInfo { valid: [false; 8], order_hint: [0; 8], frame_type: [0; 8] };
        for (i, s) in self.refs.iter().enumerate() {
            if let Some(s) = s {
                r.valid[i] = true;
                r.order_hint[i] = s.order_hint;
                r.frame_type[i] = s.frame_type;
            }
        }
        r
    }

    fn temporal_unit(&mut self, data: &[u8], pts: i64) -> Result<()> {
        for o in split_obus(data)? {
            let (tid, sid) = o.ext.map_or((0, 0), |(t, s)| (t as u32, s as u32));
            if let (Some(_), Some(seq)) = (o.ext, &self.seq) {
                if seq.op_idc != 0 && ((seq.op_idc >> tid) & 1 == 0 || (seq.op_idc >> (sid + 8)) & 1 == 0) {
                    continue;
                }
            }
            match o.kind {
                OBU_SEQUENCE_HEADER => self.seq = Some(parse_sequence_header(o.payload)?),
                OBU_FRAME_HEADER | OBU_REDUNDANT_FRAME_HEADER | OBU_FRAME => {
                    if o.kind != OBU_FRAME && self.cur.is_some() {
                        continue;
                    }
                    let seq = self.seq.clone().ok_or_else(|| invalid("a frame before any sequence header"))?;
                    let h = parse_frame_header(o.payload, &seq, &self.ref_info(), tid, sid)?;
                    if h.show_existing_frame {
                        self.show_existing(&h, pts)?;
                        continue;
                    }
                    check_supported(&seq, &h)?;
                    let cdf = Cdfs::new(h.base_q_idx);
                    let rest = h.header_bytes;
                    self.cur = Some(Current { f: FrameDec::new(seq, h), cdf, saved: None });
                    if o.kind == OBU_FRAME {
                        self.tile_group(&o.payload[rest..], pts)?;
                    }
                }
                OBU_TILE_GROUP => self.tile_group(o.payload, pts)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn tile_group(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let Some(cur) = self.cur.as_mut() else {
            return Err(invalid("a tile group without a frame header"));
        };
        let res = Self::tiles(cur, data);
        let last = match res {
            Ok(l) => l,
            Err(e) => {
                self.cur = None;
                return Err(e);
            }
        };
        if last {
            self.finish(pts)?;
        }
        Ok(())
    }

    /// Decodes a tile group's tiles; returns whether it held the frame's last tile.
    fn tiles(cur: &mut Current, data: &[u8]) -> Result<bool> {
        let h = &cur.f.h;
        let num = h.num_tiles();
        let mut b = Bits::new(data);
        let (start, end) = if num > 1 && b.flag()? {
            let bits = h.tile_cols_log2 + h.tile_rows_log2;
            (b.f(bits)? as usize, b.f(bits)? as usize)
        } else {
            (0, num - 1)
        };
        if end >= num || start > end {
            return Err(invalid("tile group tile range out of bounds"));
        }
        b.byte_align();
        let mut p = b.byte_pos();
        let (cols, tsb, ctx_tile) = (h.tile_cols, h.tile_size_bytes, h.context_update_tile_id);
        for tn in start..=end {
            let size = if tn == end {
                data.len().checked_sub(p).ok_or_else(|| invalid("tile group too short"))?
            } else {
                let b = data.get(p..p + tsb).ok_or_else(|| invalid("tile size runs past the tile group"))?;
                p += tsb;
                b.iter().rev().fold(0usize, |a, &x| (a << 8) | x as usize) + 1
            };
            let tile = data.get(p..p + size).ok_or_else(|| invalid("tile runs past the tile group"))?;
            p += size;
            let out = cur.f.decode_tile(tile, tn / cols, tn % cols, cur.cdf.clone())?;
            if tn == ctx_tile {
                cur.saved = Some(out);
            }
        }
        Ok(end == num - 1)
    }

    fn finish(&mut self, pts: i64) -> Result<()> {
        let Some(cur) = self.cur.take() else { return Ok(()) };
        let mut f = cur.f;
        if f.h.lf_level[0] != 0 || f.h.lf_level[1] != 0 {
            lf::deblock(&mut f);
        }
        let mut cdfs = if f.h.disable_frame_end_update_cdf { cur.cdf } else { cur.saved.unwrap_or(cur.cdf) };
        cdfs.clear_counts();
        let h = &f.h;
        let pic = Arc::new(Picture {
            width: h.upscaled_width as usize,
            height: h.height as usize,
            bd: f.bd,
            ss: f.ss,
            mono: f.num_planes == 1,
            rgb: f.seq.rgb,
            planes: std::mem::replace(&mut f.planes, [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)]),
        });
        let slot = RefSlot { pic: pic.clone(), frame_type: h.frame_type, order_hint: h.order_hint, cdfs: Arc::from(cdfs) };
        for i in 0..8 {
            if h.refresh_frame_flags >> i & 1 == 1 {
                self.refs[i] = Some(slot.clone());
            }
        }
        if h.show_frame {
            self.emit(&pic, pts)?;
        }
        Ok(())
    }

    fn show_existing(&mut self, h: &FrameHeader, pts: i64) -> Result<()> {
        let slot = self.refs[h.frame_to_show].clone().ok_or_else(|| invalid("show_existing_frame of an empty reference slot"))?;
        if slot.frame_type == KEY_FRAME {
            for r in self.refs.iter_mut() {
                *r = Some(slot.clone());
            }
        }
        self.emit(&slot.pic, pts)
    }

    fn emit(&mut self, p: &Picture, pts: i64) -> Result<()> {
        if p.rgb && !p.mono {
            if p.bd != 8 || p.ss != [0, 0] {
                return Err(unsupported("RGB above 8 bits or subsampled has no output format"));
            }
            let mut v = Vec::with_capacity(p.width * p.height * 3);
            let [g, b, r] = &p.planes;
            for y in 0..p.height {
                for x in 0..p.width {
                    v.extend([r.at(x, y), g.at(x, y), b.at(x, y)].map(|s| s as u8));
                }
            }
            let f = VideoFrame { width: p.width as u32, height: p.height as u32, pix_fmt: PixelFormat::Rgb24, pts, planes: vec![v] };
            self.out.push(Frame::Video(f));
            return Ok(());
        }
        let pix_fmt = match (p.bd, p.mono, p.ss) {
            (8, true, _) => PixelFormat::Gray8,
            (_, true, _) => return Err(unsupported("monochrome above 8 bits has no output format")),
            (8, _, [1, 1]) => PixelFormat::Yuv420p,
            (8, _, [1, 0]) => PixelFormat::Yuv422p,
            (8, _, [0, 0]) => PixelFormat::Yuv444p,
            (10, _, [1, 1]) => PixelFormat::Yuv420p10,
            (10, _, [1, 0]) => PixelFormat::Yuv422p10,
            (10, _, [0, 0]) => PixelFormat::Yuv444p10,
            (12, _, [1, 1]) => PixelFormat::Yuv420p12,
            (12, _, [1, 0]) => PixelFormat::Yuv422p12,
            (12, _, [0, 0]) => PixelFormat::Yuv444p12,
            _ => return Err(unsupported("this subsampling has no output format")),
        };
        let mut planes = Vec::with_capacity(3);
        for pl in 0..if p.mono { 1 } else { 3 } {
            let (w, h) = if pl == 0 { (p.width, p.height) } else { ((p.width + p.ss[0]) >> p.ss[0], (p.height + p.ss[1]) >> p.ss[1]) };
            let src = &p.planes[pl];
            let mut v = Vec::with_capacity(w * h * if p.bd > 8 { 2 } else { 1 });
            for r in 0..h {
                let row = &src.data[r * src.stride..r * src.stride + w];
                if p.bd > 8 {
                    v.extend(row.iter().flat_map(|s| s.to_le_bytes()));
                } else {
                    v.extend(row.iter().map(|&s| s as u8));
                }
            }
            planes.push(v);
        }
        self.out.push(Frame::Video(VideoFrame { width: p.width as u32, height: p.height as u32, pix_fmt, pts, planes }));
        Ok(())
    }
}

/// Refuses frames that need a tool this decoder does not have yet.
fn check_supported(seq: &SequenceHeader, h: &FrameHeader) -> Result<()> {
    if h.superres_denom != 8 {
        return Err(unsupported("superres is not decoded yet"));
    }
    if h.uses_lr {
        return Err(unsupported("loop restoration is not decoded yet"));
    }
    let cdef_on = seq.enable_cdef && !h.coded_lossless && !h.allow_intrabc;
    let n = 1 << h.cdef_bits;
    if cdef_on && (0..n).any(|i| h.cdef_y_pri[i] | h.cdef_y_sec[i] | h.cdef_uv_pri[i] | h.cdef_uv_sec[i] != 0) {
        return Err(unsupported("CDEF is not decoded yet (encode with enable-cdef=0)"));
    }
    if h.using_qmatrix && (h.qm_y < 15 || h.qm_u < 15 || h.qm_v < 15) {
        return Err(unsupported("quantiser matrices are not decoded yet"));
    }
    Ok(())
}

impl Decoder for Av1Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        self.temporal_unit(&packet.data, packet.pts)?;
        Ok(std::mem::take(&mut self.out))
    }
}
