//! A software AV1 decoder: key, intra-only and inter frames at 8, 10 and 12 bits, every
//! subsampling, tiles, palettes, intra block filters, lossless coding, every inter tool (compound
//! modes, masks, inter-intra, OBMC, warped and global motion, reference scaling, temporal motion
//! vectors), the deblocking filter, CDEF, loop restoration, quantiser matrices and film grain.
//! Superres and intra block copy are refused with an error rather than decoded wrongly.

pub mod block;
pub mod cdef;
pub mod cdf_tables;
pub mod enc;
pub mod film_grain;
pub mod grain_tables;
pub mod inter;
pub mod inter_tables;
pub mod intermode;
pub mod ipred;
pub mod itx;
pub mod itx1d;
pub mod lf;
pub mod lr;
pub mod mvpred;
pub mod obu;
pub mod qm_tables;
pub mod quant_tables;
pub mod scan_tables;
pub mod symbol;
pub mod tables;

use std::sync::Arc;

use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};

use block::{FrameDec, Picture};
use ipred::Plane;
use obu::*;
use symbol::Cdfs;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("AV1: {msg}"))
}

#[derive(Clone)]
struct RefSlot {
    pic: Arc<Picture>,
    state: RefState,
    /// The CDFs the frame ended with, for frames that load them.
    cdfs: Arc<Cdfs>,
    /// The grain parameters of the frame, which a show_existing_frame of it applies.
    grain: Option<film_grain::FilmGrain>,
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
        RefInfo { slots: std::array::from_fn(|i| self.refs[i].as_ref().map(|s| s.state.clone())) }
    }

    /// The frame's decoding state: its references, the CDFs and segment ids of its primary
    /// reference frame, and its motion field.
    fn start_frame(&self, seq: SequenceHeader, h: FrameHeader) -> Result<Current> {
        let primary = (h.primary_ref_frame != 7).then(|| h.ref_frame_idx[h.primary_ref_frame as usize]);
        let cdf = match primary {
            Some(i) => {
                let s = self.refs[i].as_ref().ok_or_else(|| invalid("the primary reference frame is missing"))?;
                Box::new((*s.cdfs).clone())
            }
            None => Cdfs::new(h.base_q_idx),
        };
        let intra = h.frame_is_intra();
        let mut f = FrameDec::new(seq, h);
        if !intra {
            for i in 0..7 {
                let s = self.refs[f.h.ref_frame_idx[i]].as_ref().ok_or_else(|| invalid("a reference frame is missing"))?;
                f.refs[i + 1] = Some(s.pic.clone());
            }
        }
        if let Some(s) = primary.and_then(|i| self.refs[i].as_ref()) {
            if s.pic.mi_rows == f.h.mi_rows && s.pic.mi_cols == f.h.mi_cols && !s.pic.seg_ids.is_empty() {
                f.prev_seg = Some(s.pic.clone());
            }
        }
        if f.h.use_ref_frame_mvs {
            f.setup_motion_field();
        }
        Ok(Current { f, cdf, saved: None })
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
                    check_supported(&h)?;
                    let rest = h.header_bytes;
                    self.cur = Some(self.start_frame(seq, h)?);
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
        // Loop restoration reads the deblocked rows beside each stripe, so keep them before CDEF.
        let deblocked = f.h.uses_lr.then(|| f.planes.clone());
        cdef::cdef(&mut f);
        if let Some(pre) = deblocked {
            lr::loop_restore(&mut f, &pre);
        }
        let mut cdfs = if f.h.disable_frame_end_update_cdf { cur.cdf } else { cur.saved.unwrap_or(cur.cdf) };
        cdfs.clear_counts();
        let h = &f.h;
        // SavedSegmentIds: libaom keeps the previous map where the frame does not update it.
        let seg_ids = if !h.seg_enabled {
            Vec::new()
        } else if !h.seg_update_map {
            f.prev_seg.as_ref().map_or_else(Vec::new, |p| p.seg_ids.clone())
        } else {
            f.mi.iter().map(|m| m.seg).collect()
        };
        let pic = Arc::new(Picture {
            width: h.upscaled_width as usize,
            height: h.height as usize,
            bd: f.bd,
            ss: f.ss,
            mono: f.num_planes == 1,
            rgb: f.seq.rgb,
            planes: std::mem::replace(&mut f.planes, [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)]),
            frame_type: h.frame_type,
            order_hint: h.order_hint,
            saved_hints: h.order_hints,
            mi_rows: h.mi_rows,
            mi_cols: h.mi_cols,
            mvs: std::mem::take(&mut f.saved_mvs),
            seg_ids,
        });
        let state = RefState {
            frame_type: h.frame_type,
            order_hint: h.order_hint,
            upscaled_width: h.upscaled_width,
            height: h.height,
            render_width: h.render_width,
            render_height: h.render_height,
            lf_ref_deltas: h.lf_ref_deltas,
            lf_mode_deltas: h.lf_mode_deltas,
            feature_enabled: h.feature_enabled,
            feature_data: h.feature_data,
            gm_params: h.gm_params,
        };
        let slot = RefSlot { pic: pic.clone(), state, cdfs: Arc::from(cdfs), grain: h.grain.clone() };
        for i in 0..8 {
            if h.refresh_frame_flags >> i & 1 == 1 {
                self.refs[i] = Some(slot.clone());
            }
        }
        if h.show_frame {
            self.emit(&pic, h.grain.as_ref(), pts)?;
        }
        Ok(())
    }

    fn show_existing(&mut self, h: &FrameHeader, pts: i64) -> Result<()> {
        let slot = self.refs[h.frame_to_show].clone().ok_or_else(|| invalid("show_existing_frame of an empty reference slot"))?;
        if slot.state.frame_type == KEY_FRAME {
            for r in self.refs.iter_mut() {
                *r = Some(slot.clone());
            }
        }
        self.emit(&slot.pic, slot.grain.as_ref(), pts)
    }

    fn emit(&mut self, p: &Picture, grain: Option<&film_grain::FilmGrain>, pts: i64) -> Result<()> {
        let grained;
        let pix: &[Plane; 3] = match grain {
            Some(g) => {
                grained = film_grain::apply(g, &p.planes, p.width, p.height, p.bd, if p.mono { 1 } else { 3 }, p.rgb);
                &grained
            }
            None => &p.planes,
        };
        if p.rgb && !p.mono {
            if p.bd != 8 || p.ss != [0, 0] {
                return Err(unsupported("RGB above 8 bits or subsampled has no output format"));
            }
            let mut v = Vec::with_capacity(p.width * p.height * 3);
            let [g, b, r] = pix;
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
            let src = &pix[pl];
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
fn check_supported(h: &FrameHeader) -> Result<()> {
    if h.superres_denom != 8 {
        return Err(unsupported("superres is not decoded yet"));
    }
    Ok(())
}

impl Decoder for Av1Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        self.temporal_unit(&packet.data, packet.pts)?;
        Ok(std::mem::take(&mut self.out))
    }
}
