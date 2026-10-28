//! A software VP9 decoder.
//!
//! Handles profiles 0 to 3 (8 and 10 bit output), every subsampling, tiles, segmentation, compound
//! prediction, and superframes. Reference frames of a different size than the frame being decoded
//! (spatial resampling) are refused.

#![allow(dead_code)]

mod adapt;
mod block;
mod dec;
mod header;
mod intrapred;
mod itx;
mod lpf;
mod lpf_dsp;
mod mvs;
mod rac;
mod recon;
mod tables;

use std::sync::Arc;

use ironav_core::{Decoder, Error, ErrorCode, Frame as OutFrame, Packet, PixelFormat, Result, VideoFrame};

use dec::*;
use header::*;
use rac::Rac;
use tables::*;

fn err(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

fn bad(e: HeaderError) -> Error {
    match e {
        HeaderError::Invalid(m) => Error::new(ErrorCode::InvalidInput, m.to_string()),
        HeaderError::Unsupported(m) => err(m),
    }
}

pub struct Vp9Decoder {
    st: HeaderState,
    refs: [Option<Arc<Pic>>; 8],
    last_pic: Option<Arc<Pic>>,
    segmap_ref: Option<Vec<u8>>,
    prev_seg: (bool, bool),
    last_keyframe: bool,
    out: Vec<OutFrame>,
    /// Picture buffers for the threads that decode tile columns, reused from frame to frame.
    pool: Vec<Pic>,
}

impl Vp9Decoder {
    pub fn new() -> Self {
        Vp9Decoder {
            st: HeaderState::new(),
            refs: Default::default(),
            last_pic: None,
            segmap_ref: None,
            prev_seg: (false, false),
            last_keyframe: false,
            out: Vec::new(),
            pool: Vec::new(),
        }
    }

    fn emit(&mut self, p: &Pic) -> Result<()> {
        let pix_fmt = match (p.bpp, p.ss_h, p.ss_v) {
            (8, true, true) => PixelFormat::Yuv420p,
            (8, true, false) => PixelFormat::Yuv422p,
            (8, false, false) => PixelFormat::Yuv444p,
            (10, true, true) => PixelFormat::Yuv420p10,
            (10, true, false) => PixelFormat::Yuv422p10,
            (10, false, false) => PixelFormat::Yuv444p10,
            _ => return Err(err("VP9 pixel format has no output format")),
        };
        let wide = p.bpp > 8;
        let mut planes = Vec::with_capacity(3);
        for pl in 0..3 {
            let (w, h) = if pl == 0 { (p.w, p.h) } else { ((p.w + p.ss_h as usize) >> p.ss_h as usize, (p.h + p.ss_v as usize) >> p.ss_v as usize) };
            let mut v = Vec::with_capacity(w * h * if wide { 2 } else { 1 });
            for r in 0..h {
                let row = &p.planes[pl][r * p.stride[pl]..r * p.stride[pl] + w];
                if wide {
                    for s in row {
                        v.extend_from_slice(&s.to_le_bytes());
                    }
                } else {
                    v.extend(row.iter().map(|&s| s as u8));
                }
            }
            planes.push(v);
        }
        self.out.push(OutFrame::Video(VideoFrame { width: p.w as u32, height: p.h as u32, pix_fmt, pts: p.pts, planes }));
        Ok(())
    }

    fn decode_frame(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let mut infos: [Option<RefInfo>; 8] = [None; 8];
        for i in 0..8 {
            infos[i] = self.refs[i].as_ref().map(|p| p.info());
        }
        let retain_segmap = self.segmap_ref.is_some() && (!self.prev_seg.0 || !self.prev_seg.1);
        let mut h = parse_uncompressed(data, &mut self.st, &infos).map_err(bad)?;
        if h.show_existing {
            let p = self.refs[h.show_existing_idx].clone().ok_or_else(|| Error::new(ErrorCode::InvalidInput, "requested reference not available".to_string()))?;
            return self.emit(&p);
        }
        if h.bpp != 8 && h.bpp != 10 {
            return Err(err("12-bit VP9 is not supported"));
        }
        let comp = &data[h.uncompressed_size..h.uncompressed_size + h.compressed_size];
        let (probs, coef) = parse_compressed(comp, &mut h, &self.st).map_err(bad)?;

        let intra = h.keyframe || h.intraonly;
        let src: Option<Arc<Pic>> = if !intra && !h.errorres { self.last_pic.clone() } else { None };
        if !retain_segmap || intra {
            self.segmap_ref = src.as_ref().map(|p| p.segmap.clone());
        }
        let cur = Pic::new(h.width, h.height, h.ss_h, h.ss_v, h.bpp, pts);
        if src.as_ref().map_or(true, |p| p.w != cur.w || p.h != cur.h) {
            self.segmap_ref = None;
        }

        let mut d = Dec::new(h.clone(), probs, coef, cur);
        d.segmap_ref = self.segmap_ref.clone();
        d.pool = std::mem::take(&mut self.pool);
        if !intra {
            for i in 0..3 {
                d.refs[i] = self.refs[h.refidx[i]].clone();
            }
        }
        d.prev = if h.use_last_frame_mvs { src.clone() } else { None };

        if h.refreshctx && h.parallelmode {
            let ctx = &mut self.st.prob_ctx[h.framectxid];
            for i in 0..4 {
                for j in 0..2 {
                    for k in 0..2 {
                        for l in 0..6 {
                            for m in 0..6 {
                                ctx.coef[i][j][k][l][m].copy_from_slice(&d.coef[i][j][k][l][m][..3]);
                            }
                        }
                    }
                }
                if h.txfmmode as usize == i {
                    break;
                }
            }
            ctx.p = d.p;
        }

        let tiles = &data[h.uncompressed_size + h.compressed_size..];
        let res = d.decode_tiles(tiles);
        self.pool = std::mem::take(&mut d.pool);
        res.map_err(|m| Error::new(ErrorCode::InvalidInput, m.to_string()))?;

        if h.refreshctx && !h.parallelmode {
            adapt::adapt_probs(&d, &mut self.st, self.last_keyframe);
        }
        if std::env::var_os("IRONAV_VP9_NOLF").is_none() {
            d.loopfilter(h.lf_sharpness);
        }

        let pic = Arc::new(d.cur);
        for i in 0..8 {
            if h.refreshrefmask & (1 << i) != 0 {
                self.refs[i] = Some(pic.clone());
            }
        }
        self.last_pic = Some(pic.clone());
        self.st.last_invisible = h.invisible;
        self.st.last_size = Some((h.width, h.height));
        self.last_keyframe = h.keyframe;
        self.prev_seg = (h.seg.enabled, h.seg.update_map);
        if !h.invisible {
            self.emit(&pic)?;
        }
        Ok(())
    }
}

impl Default for Vp9Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Splits a packet into its frames using the superframe index, if it has one.
fn split_superframe(data: &[u8]) -> Vec<&[u8]> {
    if let Some(&last) = data.last() {
        if last & 0xe0 == 0xc0 {
            let frames = (last & 7) as usize + 1;
            let mag = ((last >> 3) & 3) as usize + 1;
            let idx = 2 + mag * frames;
            if data.len() >= idx && data[data.len() - idx] == last {
                let mut out = Vec::new();
                let mut pos = 0;
                let table = &data[data.len() - idx + 1..];
                for f in 0..frames {
                    let mut sz = 0usize;
                    for b in 0..mag {
                        sz |= (table[f * mag + b] as usize) << (8 * b);
                    }
                    if pos + sz > data.len() - idx {
                        break;
                    }
                    if sz > 0 {
                        out.push(&data[pos..pos + sz]);
                    }
                    pos += sz;
                }
                return out;
            }
        }
    }
    vec![data]
}

impl Decoder for Vp9Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<OutFrame>> {
        for f in split_superframe(&packet.data) {
            self.decode_frame(f, packet.pts)?;
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<OutFrame>> {
        Ok(std::mem::take(&mut self.out))
    }
}

fn set_tile_offset(idx: usize, log2_n: u32, n: usize) -> (usize, usize) {
    let start = ((idx * n) >> log2_n).min(n) << 3;
    let end = (((idx + 1) * n) >> log2_n).min(n) << 3;
    (start, end)
}

impl Dec {
    fn decode_tiles(&mut self, mut data: &[u8]) -> std::result::Result<(), &'static str> {
        let tile_rows = 1usize << self.h.log2_tile_rows;
        let tile_cols = 1usize << self.h.log2_tile_cols;
        let keyintra = self.keyintra();

        for v in self.above_partition.iter_mut() {
            *v = 0;
        }
        for v in self.above_skip.iter_mut() {
            *v = 0;
        }
        for v in self.above_mode.iter_mut() {
            *v = if keyintra { DC_PRED } else { NEARESTMV };
        }
        for v in self.above_y_nnz.iter_mut() {
            *v = 0;
        }
        for p in 0..2 {
            for v in self.above_uv_nnz[p].iter_mut() {
                *v = 0;
            }
        }
        for v in self.above_segpred.iter_mut() {
            *v = 0;
        }

        // Every tile's data, by tile column and then tile row.
        let mut racs: Vec<Vec<Rac>> = (0..tile_cols).map(|_| Vec::with_capacity(tile_rows)).collect();
        for tile_row in 0..tile_rows {
            for tile_col in 0..tile_cols {
                let size = if tile_col == tile_cols - 1 && tile_row == tile_rows - 1 {
                    data.len()
                } else {
                    if data.len() < 4 {
                        return Err("truncated tile size");
                    }
                    let s = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                    data = &data[4..];
                    s
                };
                if size > data.len() {
                    return Err("tile larger than the frame data");
                }
                let mut r = Rac::new(&data[..size]).ok_or("empty tile")?;
                if r.get(128) {
                    return Err("tile marker bit set");
                }
                racs[tile_col].push(r);
                data = &data[size..];
            }
        }

        // Tile columns depend on nothing outside themselves, so several are decoded at once, each by a
        // fork with its own copy of the picture, and their columns are merged back afterwards.
        let threads = std::env::var("IRONAV_VP9_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get())).min(tile_cols);
        if threads <= 1 {
            for (tile_col, r) in racs.into_iter().enumerate() {
                self.decode_tile_col(tile_col, r)?;
            }
            return Ok(());
        }
        let per = tile_cols.div_ceil(threads);
        let (log2_cols, sb_cols) = (self.h.log2_tile_cols, self.sb_cols);
        let range = |first: usize| {
            let last = (first + per).min(tile_cols) - 1;
            (set_tile_offset(first, log2_cols, sb_cols).0, set_tile_offset(last, log2_cols, sb_cols).1)
        };
        let mut jobs: Vec<(usize, (usize, usize), Vec<Vec<Rac>>, Dec)> = Vec::new();
        let mut it = racs.into_iter();
        for first in (0..tile_cols).step_by(per) {
            jobs.push((first, range(first), it.by_ref().take(per).collect(), self.fork()));
        }
        let results: Vec<std::result::Result<(), &'static str>> = std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .iter_mut()
                .map(|(first, (col_start, col_end), cols, d)| {
                    let (first, col_start, col_end, cols) = (*first, *col_start, *col_end, std::mem::take(cols));
                    s.spawn(move || {
                        d.cur.clear_cols(col_start, col_end);
                        for (i, r) in cols.into_iter().enumerate() {
                            d.decode_tile_col(first + i, r)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        });
        for r in results {
            r?;
        }
        for (_, (col_start, col_end), _, d) in &jobs {
            self.merge_cols(d, *col_start, *col_end);
        }
        self.pool.extend(jobs.into_iter().map(|(_, _, _, d)| d.cur));
        Ok(())
    }

    /// Decodes one tile column, top to bottom through every tile row.
    fn decode_tile_col(&mut self, tile_col: usize, racs: Vec<Rac>) -> std::result::Result<(), &'static str> {
        let keyintra = self.keyintra();
        let (col_start, col_end) = set_tile_offset(tile_col, self.h.log2_tile_cols, self.sb_cols);
        self.tile_col_start = col_start;
        for (tile_row, rac) in racs.into_iter().enumerate() {
            let (row_start, row_end) = set_tile_offset(tile_row, self.h.log2_tile_rows, self.sb_rows);
            self.c = rac;
            let mut row = row_start;
            while row < row_end {
                self.left_partition = [0; 8];
                self.left_skip = [0; 8];
                self.left_mode = [if keyintra { DC_PRED } else { NEARESTMV }; 16];
                self.left_y_nnz = [0; 16];
                self.left_uv_nnz = [[0; 16]; 2];
                self.left_segpred = [0; 8];
                let mut col = col_start;
                while col < col_end {
                    let sb = (row >> 3) * self.sb_cols + (col >> 3);
                    self.lflvl[sb] = Filt::default();
                    self.decode_sb(row, col, 0)?;
                    col += 8;
                }
                row += 8;
            }
        }
        Ok(())
    }

    fn decode_sb(&mut self, row: usize, col: usize, bl: usize) -> std::result::Result<(), &'static str> {
        let c = ((self.above_partition[col] >> (3 - bl)) & 1) as usize | ((((self.left_partition[row & 7] >> (3 - bl)) & 1) as usize) << 1);
        let p: [u8; 3] = if self.keyintra() { DEFAULT_KF_PARTITION_PROBS[bl][c] } else { self.p.partition[bl][c] };
        let hbs = 4usize >> bl;
        let bp: usize;
        if bl == 3 {
            bp = self.c.tree(&PARTITION_TREE, &p);
            self.decode_block(row, col, bl, bp)?;
        } else if col + hbs < self.cols {
            if row + hbs < self.rows {
                bp = self.c.tree(&PARTITION_TREE, &p);
                match bp {
                    0 => self.decode_block(row, col, bl, bp)?,
                    1 => {
                        self.decode_block(row, col, bl, bp)?;
                        self.decode_block(row + hbs, col, bl, bp)?;
                    }
                    2 => {
                        self.decode_block(row, col, bl, bp)?;
                        self.decode_block(row, col + hbs, bl, bp)?;
                    }
                    _ => {
                        self.decode_sb(row, col, bl + 1)?;
                        self.decode_sb(row, col + hbs, bl + 1)?;
                        self.decode_sb(row + hbs, col, bl + 1)?;
                        self.decode_sb(row + hbs, col + hbs, bl + 1)?;
                    }
                }
            } else if self.c.get(p[1]) {
                bp = 3;
                self.decode_sb(row, col, bl + 1)?;
                self.decode_sb(row, col + hbs, bl + 1)?;
            } else {
                bp = 1;
                self.decode_block(row, col, bl, bp)?;
            }
        } else if row + hbs < self.rows {
            if self.c.get(p[2]) {
                bp = 3;
                self.decode_sb(row, col, bl + 1)?;
                self.decode_sb(row + hbs, col, bl + 1)?;
            } else {
                bp = 2;
                self.decode_block(row, col, bl, bp)?;
            }
        } else {
            bp = 3;
            self.decode_sb(row, col, bl + 1)?;
        }
        self.counts.partition[bl][c][bp] += 1;
        Ok(())
    }
}
