//! The slice and macroblock layers of MPEG-1 and MPEG-2 video: macroblock types, motion vectors
//! (frame, field, 16x8 and dual prime), DC prediction, the coefficients with their inverse
//! quantisation, and the reconstruction of a macroblock. The syntax and the arithmetic follow
//! FFmpeg's `mpeg12dec.c`, `mpeg12.c`, `mpegvideo_motion.c` and `mpegvideo_dec.c` (Fabrice
//! Bellard, Michael Niedermayer and the FFmpeg developers, LGPL-2.1-or-later), so that the output
//! matches FFmpeg's decoder with `-idct simple`.

use super::mc::{self, View};
use super::tables as T;
use super::{Env, Frame};
use crate::mpeg4::bits::{Bits, Vlc};
use crate::mpeg4::idct;
use ironav_core::{Error, ErrorCode, Result};

fn bad(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("MPEG-2: {msg}"))
}

/// How a macroblock's vectors predict it.
#[derive(Clone, Copy, PartialEq)]
enum Mt {
    /// One vector for the whole macroblock (frame pictures, and all of MPEG-1).
    Frame,
    /// A vector per field: the two fields of a frame macroblock, or the whole of a field macroblock.
    Field,
    /// Two vectors, one for each 16x8 half of a field macroblock.
    Half,
    /// Dual prime.
    Dmv,
}

const PICT_I: u8 = 1;
const PICT_P: u8 = 2;
const PICT_B: u8 = 3;
const FRAME: u8 = 3;

struct S<'a> {
    e: &'a Env<'a>,
    br: Bits<'a>,
    cur: &'a mut Frame,
    /// The view of the frame that the picture is: all rows, or one field's.
    view: View,
    mb_x: usize,
    mb_y: usize,
    /// Macroblock rows in the picture.
    rows: usize,
    qscale: i32,
    last_dc: [i32; 3],
    last_mv: [[[i32; 2]; 2]; 2],
    mv_dir: u8,
    mv_type: Mt,
    mv: [[[i32; 2]; 4]; 2],
    field_select: [[usize; 2]; 2],
    interlaced_dct: bool,
    prev_intra: bool,
    block: [[i16; 64]; 12],
    scan: &'static [u8; 64],
}

/// Decodes one slice: `d` is its data after the start code, `mb_y` its macroblock row in the picture
/// (a row of the field, for a field picture).
pub(crate) fn decode_slice(e: &Env, cur: &mut Frame, d: &[u8], mb_y: usize) -> Result<()> {
    let field = e.h.structure != FRAME;
    let rows = if field { e.g.mbh >> 1 } else { e.g.mbh };
    let view = if field { View::field((e.h.structure == 2) as usize) } else { View::FRAME };
    let mut s = S {
        e,
        br: Bits::new(d),
        cur,
        view,
        mb_x: 0,
        mb_y,
        rows,
        qscale: 0,
        last_dc: [0; 3],
        last_mv: [[[0; 2]; 2]; 2],
        mv_dir: 0,
        mv_type: Mt::Frame,
        mv: [[[0; 2]; 4]; 2],
        field_select: [[0; 2]; 2],
        interlaced_dct: false,
        prev_intra: false,
        block: [[0; 64]; 12],
        scan: if e.h.alternate_scan { &T::ALTERNATE_VERTICAL } else { &T::ZIGZAG },
    };
    s.run()
}

/// The symbols of a macroblock address increment: 33 is the escape (add 33), 34 stuffing, 35 the end.
fn mbincr(br: &mut Bits, v: &Vlc) -> Result<usize> {
    v.read(br).ok_or_else(|| bad("macroblock address increment damaged"))
}

impl<'a> S<'a> {
    fn get_qscale(&mut self) -> i32 {
        let q = self.br.read(5) as usize;
        if self.e.h.q_scale_type {
            T::NON_LINEAR_QSCALE[q] as i32
        } else {
            (q as i32) << 1
        }
    }

    fn run(&mut self) -> Result<()> {
        let e = self.e;
        if e.mpeg2 && e.g.mbh > 2800 / 16 {
            self.br.skip(3);
        }
        self.qscale = self.get_qscale();
        if self.qscale == 0 {
            return Err(bad("slice quantiser is 0"));
        }
        while self.br.bit() {
            self.br.skip(8);
            if self.br.overrun() {
                return Err(bad("slice header cut short"));
            }
        }
        let mut mb_x = 0;
        loop {
            let code = mbincr(&mut self.br, &e.t.mbincr)?;
            if code >= 33 {
                if code == 33 {
                    mb_x += 33;
                }
            } else {
                mb_x += code;
                break;
            }
            if self.br.overrun() {
                return Err(bad("slice cut short"));
            }
        }
        if mb_x >= e.g.mbw {
            return Err(bad("initial skip overflow"));
        }
        self.mb_x = mb_x;
        let dc0 = 128 << e.h.intra_dc_precision;
        self.last_dc = [dc0; 3];
        let mut skip_run: i32 = 0;
        loop {
            self.mb(&mut skip_run)?;
            self.mb_x += 1;
            if self.mb_x >= e.g.mbw {
                self.mb_x = 0;
                self.mb_y += 1;
                if self.mb_y >= self.rows {
                    return Ok(());
                }
            }
            if self.br.overrun() {
                return Err(bad("slice cut short"));
            }
            if skip_run == -1 {
                skip_run = 0;
                loop {
                    let code = mbincr(&mut self.br, &e.t.mbincr)?;
                    if code >= 33 {
                        if code == 33 {
                            skip_run += 33;
                        } else if code == 35 {
                            if skip_run != 0 || self.br.peek(15) != 0 {
                                return Err(bad("slice mismatch"));
                            }
                            return Ok(());
                        }
                    } else {
                        skip_run += code as i32;
                        break;
                    }
                    if self.br.overrun() {
                        return Err(bad("slice cut short"));
                    }
                }
                if skip_run != 0 {
                    if e.h.ptype == PICT_I {
                        return Err(bad("a skipped macroblock in an I picture"));
                    }
                    self.last_dc = [dc0; 3];
                    self.mv_type = if e.h.structure == FRAME { Mt::Frame } else { Mt::Field };
                    let fs = ((e.h.structure as usize).wrapping_sub(1)) & 1;
                    if e.h.ptype == PICT_P {
                        // a zero vector is implied
                        self.mv_dir = 1;
                        self.mv[0][0] = [0, 0];
                        self.last_mv[0] = [[0; 2]; 2];
                        self.field_select[0][0] = fs;
                    } else {
                        // the vectors and directions of the macroblock before
                        self.mv[0][0] = self.last_mv[0][0];
                        self.mv[1][0] = self.last_mv[1][0];
                        self.field_select[0][0] = fs;
                        self.field_select[1][0] = fs;
                    }
                }
            }
        }
    }

    /// One macroblock: parsed, then reconstructed.
    fn mb(&mut self, skip_run: &mut i32) -> Result<()> {
        let e = self.e;
        let h = e.h;
        let r = *skip_run;
        *skip_run -= 1;
        if r != 0 {
            if h.ptype == PICT_B && self.prev_intra {
                return Err(bad("a skipped macroblock after an intra one"));
            }
            self.prev_intra = false;
            self.recon(false, &[false; 12]);
            return Ok(());
        }
        let flags = match h.ptype {
            PICT_I => {
                if self.br.bit() {
                    T::F_INTRA
                } else if self.br.bit() {
                    T::F_QUANT | T::F_INTRA
                } else {
                    return Err(bad("bad macroblock type in an I picture"));
                }
            }
            PICT_P => T::PTYPE_FLAGS[e.t.ptype.read(&mut self.br).ok_or_else(|| bad("bad macroblock type in a P picture"))?],
            _ => T::BTYPE_FLAGS[e.t.btype.read(&mut self.br).ok_or_else(|| bad("bad macroblock type in a B picture"))?],
        };
        let frame_pic = h.structure == FRAME;
        let nblk = if e.g.fmt == 1 { 6 } else { 8 };
        let mut coded = [false; 12];
        if flags & T::F_INTRA != 0 {
            if frame_pic && !h.frame_pred_frame_dct {
                self.interlaced_dct = self.br.bit();
            }
            if flags & T::F_QUANT != 0 {
                self.qscale = self.get_qscale();
            }
            if h.concealment {
                // the vectors are only parsed
                if !frame_pic {
                    self.br.skip(1);
                }
                for k in 0..2 {
                    let v = self.motion(h.fcode[0][k], self.last_mv[0][0][k])?;
                    self.last_mv[0][0][k] = v;
                    self.last_mv[0][1][k] = v;
                }
                self.br.skip(1);
            } else {
                self.last_mv = [[[0; 2]; 2]; 2];
            }
            for n in 0..nblk {
                self.block_intra(n)?;
            }
            self.prev_intra = true;
            self.recon(true, &coded);
            return Ok(());
        }
        // an inter macroblock
        if flags & T::F_ZERO != 0 {
            self.mv_dir = 1;
            if frame_pic {
                if !h.frame_pred_frame_dct {
                    self.interlaced_dct = self.br.bit();
                }
                self.mv_type = Mt::Frame;
            } else {
                self.mv_type = Mt::Field;
                self.field_select[0][0] = h.structure as usize - 1;
            }
            if flags & T::F_QUANT != 0 {
                self.qscale = self.get_qscale();
            }
            self.last_mv[0] = [[0; 2]; 2];
            self.mv[0][0] = [0, 0];
        } else {
            let motion_type = if frame_pic && h.frame_pred_frame_dct {
                2
            } else {
                let m = self.br.read(2);
                if frame_pic && flags & T::F_CBP != 0 {
                    self.interlaced_dct = self.br.bit();
                }
                m
            };
            if flags & T::F_QUANT != 0 {
                self.qscale = self.get_qscale();
            }
            self.mv_dir = (flags >> 3) & 3;
            self.motion_vectors(flags, motion_type, frame_pic)?;
        }
        self.last_dc = [128 << h.intra_dc_precision; 3];
        if flags & T::F_CBP != 0 {
            let mut cbp = e.t.mbpat.read(&mut self.br).ok_or_else(|| bad("bad coded block pattern"))? as u32;
            if nblk > 6 {
                cbp = (cbp << (nblk - 6)) | self.br.read(nblk as u32 - 6);
            }
            if cbp == 0 {
                return Err(bad("invalid coded block pattern"));
            }
            for n in 0..nblk {
                if cbp >> (nblk - 1 - n) & 1 != 0 {
                    self.block_inter(n)?;
                    coded[n] = true;
                }
            }
        }
        self.prev_intra = false;
        self.recon(false, &coded);
        Ok(())
    }

    /// A motion vector component: the difference to `pred`, wrapped to the range `fcode` sets.
    fn motion(&mut self, fcode: i32, pred: i32) -> Result<i32> {
        let code = self.e.t.mv.read(&mut self.br).ok_or_else(|| bad("bad motion vector code"))?;
        if code == 0 {
            return Ok(pred);
        }
        let sign = self.br.bit();
        let shift = fcode - 1;
        let mut val = code as i32;
        if shift != 0 {
            val = (val - 1) << shift;
            val |= self.br.read(shift as u32) as i32;
            val += 1;
        }
        if sign {
            val = -val;
        }
        val += pred;
        let n = 5 + shift as u32;
        Ok((val << (32 - n)) >> (32 - n))
    }

    fn dmv(&mut self) -> i32 {
        if self.br.bit() {
            1 - ((self.br.bit() as i32) << 1)
        } else {
            0
        }
    }

    fn motion_vectors(&mut self, flags: u8, motion_type: u32, frame_pic: bool) -> Result<()> {
        let h = self.e.h;
        let has = |i: usize| flags & (T::F_FWD << i) != 0;
        match motion_type {
            2 => {
                if frame_pic {
                    self.mv_type = Mt::Frame;
                    for i in 0..2 {
                        if has(i) {
                            let x = self.motion(h.fcode[i][0], self.last_mv[i][0][0])?;
                            let y = self.motion(h.fcode[i][1], self.last_mv[i][0][1])?;
                            self.last_mv[i][0] = [x, y];
                            self.last_mv[i][1] = [x, y];
                            let k = if h.full_pel[i] { 2 } else { 1 };
                            self.mv[i][0] = [x * k, y * k];
                        }
                    }
                } else {
                    self.mv_type = Mt::Half;
                    for i in 0..2 {
                        if has(i) {
                            for j in 0..2 {
                                self.field_select[i][j] = self.br.read(1) as usize;
                                for k in 0..2 {
                                    let v = self.motion(h.fcode[i][k], self.last_mv[i][j][k])?;
                                    self.last_mv[i][j][k] = v;
                                    self.mv[i][j][k] = v;
                                }
                            }
                        }
                    }
                }
            }
            1 => {
                self.mv_type = Mt::Field;
                if frame_pic {
                    for i in 0..2 {
                        if has(i) {
                            for j in 0..2 {
                                self.field_select[i][j] = self.br.read(1) as usize;
                                let x = self.motion(h.fcode[i][0], self.last_mv[i][j][0])?;
                                self.last_mv[i][j][0] = x;
                                self.mv[i][j][0] = x;
                                let y = self.motion(h.fcode[i][1], self.last_mv[i][j][1] >> 1)?;
                                self.last_mv[i][j][1] = 2 * y;
                                self.mv[i][j][1] = y;
                            }
                        }
                    }
                } else {
                    if h.progressive_sequence {
                        return Err(bad("field motion in a progressive sequence"));
                    }
                    for i in 0..2 {
                        if has(i) {
                            self.field_select[i][0] = self.br.read(1) as usize;
                            for k in 0..2 {
                                let v = self.motion(h.fcode[i][k], self.last_mv[i][0][k])?;
                                self.last_mv[i][0][k] = v;
                                self.last_mv[i][1][k] = v;
                                self.mv[i][0][k] = v;
                            }
                        }
                    }
                }
            }
            3 => {
                if h.progressive_sequence {
                    return Err(bad("dual prime in a progressive sequence"));
                }
                self.mv_type = Mt::Dmv;
                for i in 0..2 {
                    if has(i) {
                        let my_shift = frame_pic as u32;
                        let mx = self.motion(h.fcode[i][0], self.last_mv[i][0][0])?;
                        self.last_mv[i][0][0] = mx;
                        self.last_mv[i][1][0] = mx;
                        let dmx = self.dmv();
                        let my = self.motion(h.fcode[i][1], self.last_mv[i][0][1] >> my_shift)?;
                        let dmy = self.dmv();
                        self.last_mv[i][0][1] = my * (1 << my_shift);
                        self.last_mv[i][1][1] = my * (1 << my_shift);
                        self.mv[i][0] = [mx, my];
                        self.mv[i][1] = [mx, my];
                        let (gx, gy) = ((mx > 0) as i32, (my > 0) as i32);
                        if frame_pic {
                            let mut m = if h.tff { 1 } else { 3 };
                            self.mv[i][2] = [((mx * m + gx) >> 1) + dmx, ((my * m + gy) >> 1) + dmy - 1];
                            m = 4 - m;
                            self.mv[i][3] = [((mx * m + gx) >> 1) + dmx, ((my * m + gy) >> 1) + dmy + 1];
                        } else {
                            let mut y = ((my + gy) >> 1) + dmy;
                            if h.structure == 1 {
                                y -= 1;
                            } else {
                                y += 1;
                            }
                            self.mv[i][2] = [((mx + gx) >> 1) + dmx, y];
                        }
                    }
                }
            }
            _ => return Err(bad("motion type 0")),
        }
        Ok(())
    }

    // ---- coefficients ----

    /// The intra block `n`: its DC from the predictor and the difference, and the AC run-levels.
    fn block_intra(&mut self, n: usize) -> Result<()> {
        let e = self.e;
        let h = e.h;
        let comp = if n < 4 { 0 } else { (n & 1) + 1 };
        let vlc = if comp == 0 { &e.t.dc_lum } else { &e.t.dc_chroma };
        let size = vlc.read(&mut self.br).ok_or_else(|| bad("bad DC code"))?;
        let diff = if size == 0 { 0 } else { self.br.xbits(size as u32) };
        let dc = self.last_dc[comp] + diff;
        self.last_dc[comp] = dc;
        let q = self.qscale;
        let scan = self.scan;
        let mut blk = [0i16; 64];
        if e.mpeg2 {
            let m = if n < 4 { &e.m.intra } else { &e.m.chroma_intra };
            blk[0] = (dc * (1 << (3 - h.intra_dc_precision))) as i16;
            let mut mismatch = blk[0] as i32 ^ 1;
            let rl = if h.intra_vlc_format { &e.t.rl2 } else { &e.t.rl1 };
            let mut i: i32 = 0;
            loop {
                let sym = rl.read(&mut self.br).ok_or_else(|| bad("bad coefficient code"))?;
                if sym == T::RL_SYMBOLS + 1 {
                    break;
                }
                let (j, level) = if sym == T::RL_SYMBOLS {
                    i += self.br.read(6) as i32 + 1;
                    if i > 63 {
                        return Err(bad("coefficients run off the block"));
                    }
                    let j = scan[i as usize] as usize;
                    let lv = ((self.br.read(12) as i32) << 20) >> 20;
                    let mag = (lv.abs() * q * m[j] as i32) >> 4;
                    (j, if lv < 0 { -mag } else { mag })
                } else {
                    i += T::RUN[sym] as i32 + 1;
                    if i > 63 {
                        return Err(bad("coefficients run off the block"));
                    }
                    let j = scan[i as usize] as usize;
                    let mag = (T::LEVEL[sym] as i32 * q * m[j] as i32) >> 4;
                    (j, if self.br.bit() { -mag } else { mag })
                };
                mismatch ^= level;
                blk[j] = level as i16;
            }
            blk[63] ^= (mismatch & 1) as i16;
        } else {
            let m = &e.m.intra;
            blk[0] = (dc * m[0] as i32) as i16;
            let mut i: i32 = 0;
            loop {
                let sym = e.t.rl1.read(&mut self.br).ok_or_else(|| bad("bad coefficient code"))?;
                if sym == T::RL_SYMBOLS + 1 {
                    break;
                }
                let (run, lv) = if sym == T::RL_SYMBOLS { (self.br.read(6) as i32 + 1, self.escape1()) } else { (T::RUN[sym] as i32 + 1, 0) };
                i += run;
                if i > 63 {
                    return Err(bad("coefficients run off the block"));
                }
                let j = scan[i as usize] as usize;
                let level = if sym == T::RL_SYMBOLS {
                    let mag = (((lv.abs() * q * m[j] as i32) >> 4) - 1) | 1;
                    if lv < 0 {
                        -mag
                    } else {
                        mag
                    }
                } else {
                    let mag = (((T::LEVEL[sym] as i32 * q * m[j] as i32) >> 4) - 1) | 1;
                    if self.br.bit() {
                        -mag
                    } else {
                        mag
                    }
                };
                blk[j] = level as i16;
            }
        }
        self.block[n] = blk;
        Ok(())
    }

    /// The level of an MPEG-1 escape: 8 bits, or 16 when the first byte is 0 or -128.
    fn escape1(&mut self) -> i32 {
        let lv = ((self.br.read(8) as i32) << 24) >> 24;
        if lv == -128 {
            self.br.read(8) as i32 - 256
        } else if lv == 0 {
            self.br.read(8) as i32
        } else {
            lv
        }
    }

    /// An inter block (the coded blocks of a predicted macroblock).
    fn block_inter(&mut self, n: usize) -> Result<()> {
        let e = self.e;
        let q = self.qscale;
        let scan = self.scan;
        let m = if !e.mpeg2 || n < 4 { &e.m.inter } else { &e.m.chroma_inter };
        let mut blk = [0i16; 64];
        let mut i: i32 = -1;
        let mut mismatch = 1i32;
        let mut done = false;
        // the first coefficient has a shorter code for the most common value
        if self.br.peek(1) == 1 {
            let mut level = (3 * q * m[0] as i32) >> 5;
            if e.mpeg2 {
                // no oddification
            } else {
                level = (level - 1) | 1;
            }
            if self.br.peek(2) & 1 != 0 {
                level = -level;
            }
            blk[0] = level as i16;
            mismatch ^= level;
            i = 0;
            self.br.skip(2);
            if self.br.peek(2) == 2 {
                self.br.skip(2);
                done = true;
            }
        }
        if !done {
            loop {
                let sym = e.t.rl1.read(&mut self.br).ok_or_else(|| bad("bad coefficient code"))?;
                if sym == T::RL_SYMBOLS + 1 {
                    break;
                }
                let (run, lv) = if sym == T::RL_SYMBOLS {
                    let run = self.br.read(6) as i32 + 1;
                    let lv = if e.mpeg2 { ((self.br.read(12) as i32) << 20) >> 20 } else { self.escape1() };
                    (run, Some(lv))
                } else {
                    (T::RUN[sym] as i32 + 1, None)
                };
                i += run;
                if i > 63 {
                    return Err(bad("coefficients run off the block"));
                }
                let j = scan[i as usize] as usize;
                let (mag, neg) = match lv {
                    Some(lv) => (lv.abs(), lv < 0),
                    None => (T::LEVEL[sym] as i32, self.br.bit()),
                };
                let mut v = ((mag * 2 + 1) * q * m[j] as i32) >> 5;
                if !e.mpeg2 {
                    v = (v - 1) | 1;
                }
                if neg {
                    v = -v;
                }
                mismatch ^= v;
                blk[j] = v as i16;
            }
        }
        if e.mpeg2 {
            blk[63] ^= (mismatch & 1) as i16;
        }
        self.block[n] = blk;
        Ok(())
    }

    // ---- reconstruction ----

    fn recon(&mut self, intra: bool, coded: &[bool; 12]) {
        if !intra {
            self.mc_mb();
        }
        let e = self.e;
        let v = self.view;
        let frame_pic = e.h.structure == FRAME;
        let ild = self.interlaced_dct && frame_pic;
        let (mx, my) = (self.mb_x, self.mb_y);
        for n in 0..4 {
            if intra || coded[n] {
                let p = &mut self.cur.p[0];
                let vs = p.stride * v.step;
                let base = v.at(p, 16 * mx + (n & 1) * 8, 16 * my);
                let (off, es) = if ild { ((n >> 1) * vs, vs * 2) } else { ((n >> 1) * 8 * vs, vs) };
                if intra {
                    idct::put(&mut self.block[n], &mut p.d[base + off..], es);
                } else {
                    idct::add(&mut self.block[n], &mut p.d[base + off..], es);
                }
            }
        }
        let nblk = if e.g.fmt == 1 { 6 } else { 8 };
        for n in 4..nblk {
            if intra || coded[n] {
                let p = &mut self.cur.p[1 + (n & 1)];
                let vs = p.stride * v.step;
                let (base, off, es) = if e.g.fmt == 1 {
                    (v.at(p, 8 * mx, 8 * my), 0, vs)
                } else {
                    let lower = n >= 6;
                    let (off, es) = if ild { (if lower { vs } else { 0 }, vs * 2) } else { (if lower { 8 * vs } else { 0 }, vs) };
                    (v.at(p, 8 * mx, 16 * my), off, es)
                };
                if intra {
                    idct::put(&mut self.block[n], &mut p.d[base + off..], es);
                } else {
                    idct::add(&mut self.block[n], &mut p.d[base + off..], es);
                }
            }
        }
    }

    // ---- motion compensation ----

    fn mc_mb(&mut self) {
        let first = self.mv_dir & 1 != 0;
        for dir in 0..2 {
            if self.mv_dir & (1 << dir) != 0 {
                self.mc_dir(dir, dir == 1 && first);
            }
        }
    }

    /// The frame and the rows of it that field `fs` of the reference in direction `dir` gives a
    /// field picture. The second field of a P frame can predict from the first field of its own frame.
    fn field_ref(&self, dir: usize, fs: usize) -> (&'a Frame, View) {
        let e: &'a Env<'a> = self.e;
        let base = if dir == 0 { e.fwd } else { e.bwd };
        let same_parity = e.h.structure as usize == fs + 1;
        let f = if same_parity || e.h.ptype == PICT_B || e.first_field { base } else { e.same.unwrap_or(base) };
        (f, View::field(fs))
    }

    fn mc_dir(&mut self, dir: usize, avg: bool) {
        let e: &'a Env<'a> = self.e;
        let frame_pic = e.h.structure == FRAME;
        let base = if dir == 0 { e.fwd } else { e.bwd };
        let (mx, my) = (self.mb_x, self.mb_y);
        match self.mv_type {
            Mt::Frame => {
                let [x, y] = self.mv[dir][0];
                self.pred(View::FRAME, base, View::FRAME, 16 * my, x, y, 16, avg);
            }
            Mt::Field if frame_pic => {
                for j in 0..2 {
                    let [x, y] = self.mv[dir][j];
                    self.pred(View::field(j), base, View::field(self.field_select[dir][j]), 8 * my, x, y, 8, avg);
                }
            }
            Mt::Field => {
                let (f, rv) = self.field_ref(dir, self.field_select[dir][0]);
                let [x, y] = self.mv[dir][0];
                self.pred(self.view, f, rv, 16 * my, x, y, 16, avg);
            }
            Mt::Half => {
                for i in 0..2 {
                    let (f, rv) = self.field_ref(dir, self.field_select[dir][i]);
                    let [x, y] = self.mv[dir][i];
                    self.pred(self.view, f, rv, 16 * my + 8 * i, x, y, 8, avg);
                }
            }
            Mt::Dmv if frame_pic => {
                for i in 0..2 {
                    for j in 0..2 {
                        let [x, y] = self.mv[dir][2 * i + j];
                        self.pred(View::field(j), base, View::field(j ^ i), 8 * my, x, y, 8, avg || i == 1);
                    }
                }
            }
            Mt::Dmv => {
                let _ = mx;
                for i in 0..2 {
                    let sel = (e.h.structure as usize != i + 1) as usize;
                    let f = if i == 1 && !e.first_field { e.same.unwrap_or(base) } else { base };
                    let [x, y] = self.mv[dir][2 * i];
                    self.pred(self.view, f, View::field(sel), 16 * my, x, y, 16, avg || i == 1);
                }
            }
        }
        let _ = mx;
    }

    /// Predicts the luma block of height `bh` at row `ly0` of the picture view `dv` of the macroblock's
    /// column, and the chroma blocks that go with it, from `rf` through the view `rv` of it, displaced by
    /// the half-sample vector (`mvx`, `mvy`).
    #[allow(clippy::too_many_arguments)]
    fn pred(&mut self, dv: View, rf: &Frame, rv: View, ly0: usize, mvx: i32, mvy: i32, bh: usize, avg: bool) {
        let x0 = 16 * self.mb_x;
        mc::block(&mut self.cur.p[0], dv, x0, ly0, &rf.p[0], rv, x0 as i32 + (mvx >> 1), ly0 as i32 + (mvy >> 1), (mvx & 1) as usize, (mvy & 1) as usize, 16, bh, avg);
        let cx0 = 8 * self.mb_x;
        let (cy0, ch, fx, fy, sx, sy);
        let mx = mvx / 2;
        if self.e.g.fmt == 1 {
            let my = mvy / 2;
            cy0 = ly0 / 2;
            ch = bh / 2;
            fx = (mx & 1) as usize;
            fy = (my & 1) as usize;
            sx = cx0 as i32 + (mx >> 1);
            sy = cy0 as i32 + (my >> 1);
        } else {
            cy0 = ly0;
            ch = bh;
            fx = (mx & 1) as usize;
            fy = (mvy & 1) as usize;
            sx = cx0 as i32 + (mx >> 1);
            sy = cy0 as i32 + (mvy >> 1);
        }
        for p in 1..3 {
            mc::block(&mut self.cur.p[p], dv, cx0, cy0, &rf.p[p], rv, sx, sy, fx, fy, 8, ch, avg);
        }
    }
}
