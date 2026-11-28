//! Writers of PGS display sets and DVD subpackets, for tests of the decoders and of the readers and
//! players that feed them. They write what the decoders read, with a known bitmap and palette.

/// One PGS segment: its type, a 16-bit length and the body.
pub fn pgs_segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut s = vec![kind];
    s.extend_from_slice(&(body.len() as u16).to_be_bytes());
    s.extend_from_slice(body);
    s
}

/// The run-length coding of PGS objects for `pixels` (palette indices), `w` by `h`.
pub fn pgs_rle(pixels: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for row in pixels.chunks(w).take(h) {
        let mut x = 0;
        while x < w {
            let c = row[x];
            let n = row[x..].iter().take_while(|&&p| p == c).count().min(16383);
            match (c, n) {
                (c, 1) if c != 0 => out.push(c),
                (0, n) if n < 64 => out.extend_from_slice(&[0, n as u8]),
                (0, n) => out.extend_from_slice(&[0, 0x40 | (n >> 8) as u8, n as u8]),
                (c, n) if n < 64 => out.extend_from_slice(&[0, 0x80 | n as u8, c]),
                (c, n) => out.extend_from_slice(&[0, 0xC0 | (n >> 8) as u8, n as u8, c]),
            }
            x += n;
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// An object to show: its id, position and its pixels (palette indices, `w * h`).
pub struct PgsObject {
    pub id: u16,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub pixels: Vec<u8>,
    /// Crop the object to this (x, y, w, h), or show all of it.
    pub crop: Option<[u16; 4]>,
    pub forced: bool,
}

/// The segments of a display set on a `canvas`: PCS, WDS, PDS (entries are index, Y, Cr, Cb, alpha),
/// the objects as ODS segments (their RLE data cut every `chunk` bytes, if that is not 0) and END.
/// `state` is the composition state (0 normal, 1 acquisition point, 2 epoch start).
pub fn pgs_display_set(canvas: (u16, u16), state: u8, palette: &[[u8; 5]], objects: &[PgsObject], chunk: usize) -> Vec<Vec<u8>> {
    let mut pcs = Vec::new();
    pcs.extend_from_slice(&canvas.0.to_be_bytes());
    pcs.extend_from_slice(&canvas.1.to_be_bytes());
    pcs.extend_from_slice(&[0x10, 0, 1, state << 6, 0, 0, objects.len() as u8]);
    for o in objects {
        pcs.extend_from_slice(&o.id.to_be_bytes());
        pcs.push(0);
        pcs.push(if o.crop.is_some() { 0x80 } else { 0 } | if o.forced { 0x40 } else { 0 });
        pcs.extend_from_slice(&o.x.to_be_bytes());
        pcs.extend_from_slice(&o.y.to_be_bytes());
        if let Some(c) = o.crop {
            c.iter().for_each(|v| pcs.extend_from_slice(&v.to_be_bytes()));
        }
    }
    let mut segments = vec![pgs_segment(0x16, &pcs)];
    if let Some(o) = objects.first() {
        let mut wds = vec![1, 0];
        [o.x, o.y, o.w, o.h].iter().for_each(|v| wds.extend_from_slice(&v.to_be_bytes()));
        segments.push(pgs_segment(0x17, &wds));
        let mut pds = vec![0, 0];
        palette.iter().for_each(|e| pds.extend_from_slice(e));
        segments.push(pgs_segment(0x14, &pds));
    }
    for o in objects {
        let rle = pgs_rle(&o.pixels, o.w as usize, o.h as usize);
        let size = if chunk == 0 { rle.len().max(1) } else { chunk };
        let mut parts: Vec<&[u8]> = rle.chunks(size).collect();
        if parts.is_empty() {
            parts.push(&[]);
        }
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let mut ods = o.id.to_be_bytes().to_vec();
            ods.push(0);
            ods.push(if i == 0 { 0x80 } else { 0 } | if i == last { 0x40 } else { 0 });
            if i == 0 {
                ods.extend_from_slice(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
                ods.extend_from_slice(&o.w.to_be_bytes());
                ods.extend_from_slice(&o.h.to_be_bytes());
            }
            ods.extend_from_slice(part);
            segments.push(pgs_segment(0x15, &ods));
        }
    }
    segments.push(pgs_segment(0x80, &[]));
    segments
}

/// A display set with no objects, which clears the screen.
pub fn pgs_clear_set(canvas: (u16, u16)) -> Vec<Vec<u8>> {
    pgs_display_set(canvas, 0, &[], &[], 0)
}

/// A `.sup` file: each display set (a list of segments) with its time on the 90 kHz clock, every
/// segment behind its own `PG` header.
pub fn sup_file(sets: &[(u32, Vec<Vec<u8>>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (pts, segments) in sets {
        for s in segments {
            out.extend_from_slice(b"PG");
            out.extend_from_slice(&pts.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(s);
        }
    }
    out
}

/// A DVD subpacket for a `w` by `h` picture of 2-bit pixels at (`x`, `y`) with the four colours' palette
/// indices and alphas, shown from the packet's time (`start` date) and, if `stop` is given, until that
/// date (both in 1024/90000 s after the packet's time).
#[allow(clippy::too_many_arguments)]
pub fn dvd_spu(x: u16, y: u16, w: u16, h: u16, pixels: &[u8], colormap: [u8; 4], alpha: [u8; 4], start: u16, stop: Option<u16>) -> Vec<u8> {
    let field = |first: usize| {
        let mut bits: Vec<u8> = Vec::new();
        let mut nibbles = Vec::new();
        for row in (first..h as usize).step_by(2) {
            let line = &pixels[row * w as usize..(row + 1) * w as usize];
            let mut i = 0;
            while i < line.len() {
                let c = line[i] & 3;
                let n = line[i..].iter().take_while(|&&p| p & 3 == c).count();
                if i + n == line.len() {
                    nibbles.extend_from_slice(&[0, 0, 0, c]);
                } else {
                    let n = n.min(255);
                    let v = ((n as u16) << 2) | c as u16;
                    let digits = if n < 4 { 1 } else if n < 16 { 2 } else if n < 64 { 3 } else { 4 };
                    for d in (0..digits).rev() {
                        nibbles.push(((v >> (4 * d)) & 15) as u8);
                    }
                    i += n;
                    continue;
                }
                i += n;
            }
            if nibbles.len() % 2 == 1 {
                nibbles.push(0);
            }
        }
        for p in nibbles.chunks(2) {
            bits.push(p[0] << 4 | p[1]);
        }
        bits
    };
    let (f1, f2) = (field(0), field(1));
    let off1 = 4usize;
    let off2 = off1 + f1.len();
    let cmd = off2 + f2.len();
    let mut seq = Vec::new();
    let next = if stop.is_some() { cmd + 24 } else { cmd };
    seq.extend_from_slice(&start.to_be_bytes());
    seq.extend_from_slice(&(next as u16).to_be_bytes());
    seq.push(0x01);
    seq.extend_from_slice(&[0x03, colormap[3] << 4 | colormap[2], colormap[1] << 4 | colormap[0]]);
    seq.extend_from_slice(&[0x04, alpha[3] << 4 | alpha[2], alpha[1] << 4 | alpha[0]]);
    let (x2, y2) = (x + w - 1, y + h - 1);
    seq.extend_from_slice(&[0x05, (x >> 4) as u8, ((x & 15) << 4) as u8 | (x2 >> 8) as u8, x2 as u8, (y >> 4) as u8, ((y & 15) << 4) as u8 | (y2 >> 8) as u8, y2 as u8]);
    seq.push(0x06);
    seq.extend_from_slice(&(off1 as u16).to_be_bytes());
    seq.extend_from_slice(&(off2 as u16).to_be_bytes());
    seq.push(0xFF);
    assert_eq!(seq.len(), 24);
    if let Some(stop) = stop {
        seq.extend_from_slice(&stop.to_be_bytes());
        seq.extend_from_slice(&((cmd + 24) as u16).to_be_bytes());
        seq.extend_from_slice(&[0x02, 0xFF]);
    }
    let mut spu = Vec::new();
    spu.extend_from_slice(&((cmd + seq.len()) as u16).to_be_bytes());
    spu.extend_from_slice(&(cmd as u16).to_be_bytes());
    spu.extend_from_slice(&f1);
    spu.extend_from_slice(&f2);
    spu.extend_from_slice(&seq);
    spu
}
