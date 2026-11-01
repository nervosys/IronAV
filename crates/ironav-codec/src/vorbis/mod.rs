//! Vorbis I decoder (floor 1; residues 0, 1 and 2; up to 8 channels in the usual layouts).
//!
//! The three header packets (identification, comments, setup) arrive as stream extradata in Xiph
//! lacing, as Matroska stores them. Each audio packet is decoded as the specification lays out:
//! floor curves, residue vectors, inverse channel coupling, the product of the two, an inverse MDCT,
//! a window and an overlap-add with the previous block. The first packet produces no samples;
//! afterwards a packet of block size `n` after one of size `p` yields `(n + p) / 4` samples per
//! channel.
//!
//! Not handled: floor type 0 (obsolete, refused when the setup header names it) and streams whose parameters change mid-way.

mod enc;
mod tables;

pub use enc::{headers as encoder_headers, VorbisEncoder};

use crate::opus::mdct::Dct4;
use ironav_core::{AudioFrame, Decoder, Error, ErrorCode, Frame, Packet, Result, StreamParams};
use tables::FLOOR1_INVERSE_DB;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("Vorbis: {msg}"))
}

fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

// ---- bits ----------------------------------------------------------------------------------------

/// A little-endian bit reader; reading past the end gives zeros and sets `over`.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    over: bool,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0, over: false }
    }
    fn bit(&mut self) -> u32 {
        let Some(&b) = self.d.get(self.pos >> 3) else {
            self.over = true;
            return 0;
        };
        let v = (b >> (self.pos & 7)) & 1;
        self.pos += 1;
        v as u32
    }
    /// The next `n` (<= 16) bits without consuming them; bits past the end read as zero.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        let at = self.pos >> 3;
        let mut w = 0u32;
        for i in 0..3 {
            w |= (self.d.get(at + i).copied().unwrap_or(0) as u32) << (8 * i);
        }
        (w >> (self.pos & 7)) & ((1 << n) - 1)
    }
    /// Bits not yet read.
    fn left(&self) -> usize {
        (self.d.len() * 8).saturating_sub(self.pos)
    }
    fn get(&mut self, n: u32) -> u32 {
        if n <= 16 && self.left() >= n as usize {
            let v = self.peek(n);
            self.pos += n as usize;
            return v;
        }
        let mut v = 0u32;
        for i in 0..n {
            v |= self.bit() << i;
        }
        v
    }
}

// ---- codebooks -----------------------------------------------------------------------------------

const FAST_BITS: u32 = 10;

struct Codebook {
    dim: usize,
    /// The prefix code as a tree: `nodes[i][bit]` is a child (> 0), `!entry` (< 0) or 0 (no code).
    nodes: Vec<[i32; 2]>,
    /// Indexed by the next FAST_BITS bits (first bit lowest): `entry << 8 | length` for a code that
    /// short, else 0 (walk the tree).
    fast: Vec<u32>,
    /// `entries * dim` values, or empty for a book without a value lookup.
    vectors: Vec<f32>,
}

impl Codebook {
    fn read(&self, b: &mut Bits) -> Option<usize> {
        // codes of up to FAST_BITS bits resolve with one table lookup
        let hit = self.fast[b.peek(FAST_BITS) as usize];
        if hit != 0 {
            let len = (hit & 0xFF) as usize;
            if b.left() < len {
                b.over = true;
                return None;
            }
            b.pos += len;
            return Some((hit >> 8) as usize);
        }
        let mut at = 0usize;
        loop {
            let next = self.nodes.get(at)?[b.bit() as usize];
            if b.over || next == 0 {
                return None;
            }
            if next < 0 {
                return Some(!next as usize);
            }
            at = next as usize;
        }
    }
    fn vector(&self, entry: usize) -> &[f32] {
        &self.vectors[entry * self.dim..(entry + 1) * self.dim]
    }
}

fn unpack_float(v: u32) -> f32 {
    let mant = (v & 0x1F_FFFF) as f64;
    let exp = ((v & 0x7FE0_0000) >> 21) as i32;
    let m = if v & 0x8000_0000 != 0 { -mant } else { mant };
    (m * 2f64.powi(exp - 788)) as f32
}

/// The largest `v` with `v^dim <= entries`.
fn nth_root(entries: u32, dim: u32) -> u32 {
    let mut v = 0u64;
    loop {
        let next = v + 1;
        let mut p = 1u64;
        for _ in 0..dim {
            p = p.saturating_mul(next);
            if p > entries as u64 {
                return v as u32;
            }
        }
        v = next;
    }
}

fn read_codebook(b: &mut Bits) -> Result<Codebook> {
    if b.get(24) != 0x56_4342 {
        return Err(bad("a codebook does not start with its sync pattern"));
    }
    let dim = b.get(16) as usize;
    let entries = b.get(24) as usize;
    if dim == 0 || entries > 1 << 16 {
        return Err(bad("a codebook has no dimensions or too many entries"));
    }
    let mut lens = vec![0u8; entries];
    if b.bit() == 0 {
        let sparse = b.bit() == 1;
        for l in lens.iter_mut() {
            if !sparse || b.bit() == 1 {
                *l = b.get(5) as u8 + 1;
            }
        }
    } else {
        let mut at = 0usize;
        let mut len = b.get(5) + 1;
        while at < entries {
            if len > 32 {
                return Err(bad("a codebook's code lengths run out of range"));
            }
            let n = b.get(ilog((entries - at) as u32)) as usize;
            if at + n > entries {
                return Err(bad("a codebook has more code lengths than entries"));
            }
            lens[at..at + n].fill(len as u8);
            at += n;
            len += 1;
        }
    }
    if b.over {
        return Err(bad("the setup header ends inside a codebook"));
    }

    // code assignment: the first used entry takes the all-zero code, every later one the lowest
    // free code of its length (the first bit read is bit 0 of `codes`)
    let mut codes = vec![0u32; entries];
    let mut exit = [0u32; 34];
    let mut first = true;
    for p in 0..entries {
        let len = lens[p] as usize;
        if len == 0 {
            continue;
        }
        if first {
            first = false;
            for i in 0..len {
                exit[i + 1] = 1u32 << i;
            }
            continue;
        }
        let Some(at) = (1..=len).rev().find(|&i| exit[i] != 0) else {
            return Err(bad("a codebook's code lengths over-fill the code space"));
        };
        let code = exit[at];
        exit[at] = 0;
        for j in at + 1..=len {
            exit[j] = code + (1u32 << (j - 1));
        }
        codes[p] = code;
    }
    let mut nodes: Vec<[i32; 2]> = vec![[0, 0]];
    for p in 0..entries {
        let len = lens[p] as usize;
        if len == 0 {
            continue;
        }
        let mut at = 0usize;
        for j in 0..len {
            let bit = ((codes[p] >> j) & 1) as usize;
            if j + 1 == len {
                nodes[at][bit] = !(p as i32);
            } else {
                if nodes[at][bit] <= 0 {
                    nodes.push([0, 0]);
                    nodes[at][bit] = nodes.len() as i32 - 1;
                }
                at = nodes[at][bit] as usize;
            }
        }
    }

    let mut fast = vec![0u32; 1 << FAST_BITS];
    for p in 0..entries {
        let len = lens[p] as u32;
        if len == 0 || len > FAST_BITS {
            continue;
        }
        for hi in 0..1u32 << (FAST_BITS - len) {
            fast[(codes[p] | hi << len) as usize] = (p as u32) << 8 | len;
        }
    }

    let lookup = b.get(4);
    let mut vectors = Vec::new();
    match lookup {
        0 => {}
        1 | 2 => {
            let min = unpack_float(b.get(32));
            let delta = unpack_float(b.get(32));
            let bits = b.get(4) + 1;
            let seq = b.bit() == 1;
            let values = if lookup == 1 { nth_root(entries as u32, dim as u32) as usize } else { entries * dim };
            if values.saturating_mul(bits as usize) > b.left() || entries * dim > 1 << 22 {
                return Err(bad("a codebook's value table is larger than the header"));
            }
            let mult: Vec<u32> = (0..values).map(|_| b.get(bits)).collect();
            if b.over {
                return Err(bad("the setup header ends inside a codebook"));
            }
            vectors = vec![0f32; entries * dim];
            for e in 0..entries {
                if lens[e] == 0 {
                    continue;
                }
                let mut last = 0f32;
                let mut idx = e;
                for k in 0..dim {
                    let m = if lookup == 1 {
                        let m = mult[idx % values];
                        idx /= values;
                        m
                    } else {
                        mult[e * dim + k]
                    };
                    // single-precision steps, in the order the reference decoders take them: the
                    // sign of a sum that should be exactly zero can steer inverse coupling
                    let v = m as f32 * delta + min + last;
                    if seq {
                        last = v;
                    }
                    vectors[e * dim + k] = v;
                }
            }
        }
        _ => return Err(bad("a codebook has an unknown lookup type")),
    }
    Ok(Codebook { dim, nodes, fast, vectors })
}

// ---- setup ---------------------------------------------------------------------------------------

struct Floor1 {
    class_dim: [usize; 16],
    class_sub: [u32; 16],
    class_master: [usize; 16],
    sub_books: [[i32; 8]; 16],
    partition_class: Vec<usize>,
    mult: i32,
    xs: Vec<i32>,
    /// For each point from index 2: the nearest lower and higher X among the points before it.
    low: Vec<usize>,
    high: Vec<usize>,
    /// Point indices by ascending X.
    order: Vec<usize>,
}

struct Residue {
    kind: u32,
    begin: usize,
    end: usize,
    psize: usize,
    classes: usize,
    classbook: usize,
    books: Vec<[i32; 8]>,
    maxpass: usize,
}

struct Mapping {
    submaps: usize,
    /// (magnitude, angle) channel pairs.
    coupling: Vec<(usize, usize)>,
    mux: Vec<usize>,
    floor: Vec<usize>,
    residue: Vec<usize>,
}

struct Mode {
    long: bool,
    mapping: usize,
}

struct Setup {
    channels: usize,
    rate: u32,
    blocksize: [usize; 2],
    books: Vec<Codebook>,
    floors: Vec<Floor1>,
    residues: Vec<Residue>,
    mappings: Vec<Mapping>,
    modes: Vec<Mode>,
}

/// Split Xiph-laced extradata into the identification, comment and setup headers.
pub fn split_headers(extra: &[u8]) -> Result<[&[u8]; 3]> {
    if extra.first() != Some(&2) {
        return Err(bad("the codec data does not hold three laced headers"));
    }
    let mut at = 1;
    let mut size = [0usize; 2];
    for s in size.iter_mut() {
        loop {
            let b = *extra.get(at).ok_or_else(|| bad("the codec data is truncated"))?;
            at += 1;
            *s += b as usize;
            if b != 255 {
                break;
            }
        }
    }
    let end0 = at + size[0];
    let end1 = end0 + size[1];
    if end1 > extra.len() {
        return Err(bad("the codec data is truncated"));
    }
    Ok([&extra[at..end0], &extra[end0..end1], &extra[end1..]])
}

/// Join the three header packets into Xiph-laced extradata.
pub fn join_headers(h: [&[u8]; 3]) -> Vec<u8> {
    let mut v = vec![2u8];
    for p in &h[..2] {
        let mut n = p.len();
        while n >= 255 {
            v.push(255);
            n -= 255;
        }
        v.push(n as u8);
    }
    for p in h {
        v.extend_from_slice(p);
    }
    v
}

impl Setup {
    fn parse(extra: &[u8]) -> Result<Setup> {
        let [id, _comments, setup] = split_headers(extra)?;

        // identification header
        if id.len() < 30 || id[0] != 1 || &id[1..7] != b"vorbis" {
            return Err(bad("the identification header is missing or damaged"));
        }
        let mut b = Bits::new(&id[7..]);
        if b.get(32) != 0 {
            return Err(bad("unknown Vorbis version"));
        }
        let channels = b.get(8) as usize;
        let rate = b.get(32);
        b.get(32);
        b.get(32);
        b.get(32);
        let (bl0, bl1) = (b.get(4), b.get(4));
        if channels == 0 || rate == 0 || !(6..=13).contains(&bl0) || !(6..=13).contains(&bl1) || bl0 > bl1 || b.bit() == 0 {
            return Err(bad("the identification header holds impossible values"));
        }
        let blocksize = [1usize << bl0, 1usize << bl1];

        // setup header
        if setup.len() < 7 || setup[0] != 5 || &setup[1..7] != b"vorbis" {
            return Err(bad("the setup header is missing or damaged"));
        }
        let mut b = Bits::new(&setup[7..]);
        let nbooks = b.get(8) as usize + 1;
        let mut books = Vec::with_capacity(nbooks);
        for _ in 0..nbooks {
            books.push(read_codebook(&mut b)?);
        }
        for _ in 0..b.get(6) + 1 {
            if b.get(16) != 0 {
                return Err(bad("a time-domain transform is set (none is defined)"));
            }
        }

        let mut floors = Vec::new();
        for _ in 0..b.get(6) + 1 {
            if b.get(16) != 1 {
                return Err(Error::new(ErrorCode::UnsupportedCodec, "Vorbis: floor type 0 is not supported"));
            }
            floors.push(read_floor1(&mut b, books.len(), blocksize[1])?);
        }

        let mut residues = Vec::new();
        for _ in 0..b.get(6) + 1 {
            let kind = b.get(16);
            if kind > 2 {
                return Err(bad("a residue has an unknown type"));
            }
            let begin = b.get(24) as usize;
            let end = b.get(24) as usize;
            let psize = b.get(24) as usize + 1;
            let classes = b.get(6) as usize + 1;
            let classbook = b.get(8) as usize;
            if classbook >= books.len() || begin > end {
                return Err(bad("a residue refers to a missing codebook or ends before it begins"));
            }
            let mut cascade = Vec::new();
            for _ in 0..classes {
                let low = b.get(3);
                let high = if b.bit() == 1 { b.get(5) } else { 0 };
                cascade.push((high << 3) | low);
            }
            let mut bk = Vec::new();
            let mut maxpass = 0;
            for &c in &cascade {
                let mut row = [-1i32; 8];
                for (k, slot) in row.iter_mut().enumerate() {
                    if c & (1 << k) != 0 {
                        let n = b.get(8) as usize;
                        if n >= books.len() {
                            return Err(bad("a residue refers to a missing codebook"));
                        }
                        *slot = n as i32;
                        maxpass = maxpass.max(k);
                    }
                }
                bk.push(row);
            }
            residues.push(Residue { kind, begin, end, psize, classes, classbook, books: bk, maxpass });
        }

        let mut mappings = Vec::new();
        for _ in 0..b.get(6) + 1 {
            if b.get(16) != 0 {
                return Err(bad("a mapping has an unknown type"));
            }
            let submaps = if b.bit() == 1 { b.get(4) as usize + 1 } else { 1 };
            let mut coupling = Vec::new();
            if b.bit() == 1 {
                let steps = b.get(8) as usize + 1;
                let nb = ilog(channels as u32 - 1);
                for _ in 0..steps {
                    let (m, a) = (b.get(nb) as usize, b.get(nb) as usize);
                    if m >= channels || a >= channels || m == a {
                        return Err(bad("channel coupling names a missing channel"));
                    }
                    coupling.push((m, a));
                }
            }
            if b.get(2) != 0 {
                return Err(bad("a mapping's reserved bits are set"));
            }
            let mux = if submaps > 1 { (0..channels).map(|_| b.get(4) as usize).collect() } else { vec![0; channels] };
            let (mut floor, mut residue) = (Vec::new(), Vec::new());
            for _ in 0..submaps {
                b.get(8);
                let (f, r) = (b.get(8) as usize, b.get(8) as usize);
                if f >= floors.len() || r >= residues.len() {
                    return Err(bad("a mapping refers to a missing floor or residue"));
                }
                floor.push(f);
                residue.push(r);
            }
            if mux.iter().any(|&m| m >= submaps) {
                return Err(bad("a mapping assigns a channel to a missing submap"));
            }
            mappings.push(Mapping { submaps, coupling, mux, floor, residue });
        }

        let mut modes = Vec::new();
        for _ in 0..b.get(6) + 1 {
            let long = b.bit() == 1;
            let (w, t) = (b.get(16), b.get(16));
            let mapping = b.get(8) as usize;
            if w != 0 || t != 0 || mapping >= mappings.len() {
                return Err(bad("a mode is invalid"));
            }
            modes.push(Mode { long, mapping });
        }
        if b.bit() == 0 || b.over {
            return Err(bad("the setup header is damaged (framing bit)"));
        }
        Ok(Setup { channels, rate, blocksize, books, floors, residues, mappings, modes })
    }
}

fn read_floor1(b: &mut Bits, nbooks: usize, long_block: usize) -> Result<Floor1> {
    let parts = b.get(5) as usize;
    let partition_class: Vec<usize> = (0..parts).map(|_| b.get(4) as usize).collect();
    let classes = partition_class.iter().copied().max().map_or(0, |m| m + 1);
    let mut f = Floor1 {
        class_dim: [0; 16],
        class_sub: [0; 16],
        class_master: [0; 16],
        sub_books: [[-1; 8]; 16],
        partition_class,
        mult: 0,
        xs: vec![0, 0],
        low: Vec::new(),
        high: Vec::new(),
        order: Vec::new(),
    };
    for c in 0..classes {
        f.class_dim[c] = b.get(3) as usize + 1;
        f.class_sub[c] = b.get(2);
        if f.class_sub[c] != 0 {
            f.class_master[c] = b.get(8) as usize;
            if f.class_master[c] >= nbooks {
                return Err(bad("a floor refers to a missing codebook"));
            }
        }
        for k in 0..1 << f.class_sub[c] {
            let v = b.get(8) as i32 - 1;
            if v >= nbooks as i32 {
                return Err(bad("a floor refers to a missing codebook"));
            }
            f.sub_books[c][k] = v;
        }
    }
    f.mult = b.get(2) as i32 + 1;
    let rangebits = b.get(4);
    f.xs[1] = 1 << rangebits;
    if f.xs[1] as usize > long_block / 2 {
        return Err(bad("a floor's range is wider than the long block"));
    }
    for &pc in &f.partition_class {
        for _ in 0..f.class_dim[pc] {
            f.xs.push(b.get(rangebits) as i32);
        }
    }
    let n = f.xs.len();
    f.low = vec![0; n];
    f.high = vec![1; n];
    for i in 2..n {
        for j in 2..i {
            let t = f.xs[j];
            if t < f.xs[i] {
                if t > f.xs[f.low[i]] {
                    f.low[i] = j;
                }
            } else if t < f.xs[f.high[i]] {
                f.high[i] = j;
            }
        }
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| f.xs[i]);
    if order.windows(2).any(|w| f.xs[w[0]] == f.xs[w[1]]) {
        return Err(bad("a floor repeats an X coordinate"));
    }
    f.order = order;
    Ok(f)
}

// ---- floor 1 -------------------------------------------------------------------------------------

fn render_line(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut [f32]) {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let base = dy / adx;
    let sy = if dy < 0 { base - 1 } else { base + 1 };
    let ady = dy.abs() - base.abs() * adx;
    let (mut x, mut y, mut err) = (x0, y0, 0);
    let at = |y: i32| FLOOR1_INVERSE_DB[y.clamp(0, 255) as usize];
    if let Some(o) = out.get_mut(x as usize) {
        *o = at(y);
    }
    x += 1;
    while x < x1 {
        err += ady;
        if err >= adx {
            err -= adx;
            y += sy;
        } else {
            y += base;
        }
        if let Some(o) = out.get_mut(x as usize) {
            *o = at(y);
        }
        x += 1;
    }
}

impl Floor1 {
    /// Decode one channel's floor into `out` (length n/2). `None` when the channel is unused.
    fn decode(&self, books: &[Codebook], b: &mut Bits, out: &mut [f32]) -> Option<()> {
        if b.bit() == 0 {
            return None;
        }
        let range = [256i32, 128, 86, 64][self.mult as usize - 1];
        let n = self.xs.len();
        let mut y = vec![0i32; n];
        let rb = ilog(range as u32 - 1);
        y[0] = b.get(rb) as i32;
        y[1] = b.get(rb) as i32;
        let mut at = 2;
        for &pc in &self.partition_class {
            let (cdim, cbits) = (self.class_dim[pc], self.class_sub[pc]);
            let csub = (1u32 << cbits) - 1;
            let mut cval = if cbits > 0 { books[self.class_master[pc]].read(b)? as u32 } else { 0 };
            for j in 0..cdim {
                let book = self.sub_books[pc][(cval & csub) as usize];
                cval >>= cbits;
                y[at + j] = if book >= 0 { books[book as usize].read(b)? as i32 } else { 0 };
            }
            at += cdim;
        }
        if b.over {
            return None;
        }

        let mut fin = vec![0i32; n];
        let mut step = vec![false; n];
        fin[0] = y[0];
        fin[1] = y[1];
        step[0] = true;
        step[1] = true;
        for i in 2..n {
            let (lo, hi) = (self.low[i], self.high[i]);
            let dy = fin[hi] - fin[lo];
            let adx = self.xs[hi] - self.xs[lo];
            let off = dy.abs() * (self.xs[i] - self.xs[lo]) / adx;
            let predicted = if dy < 0 { fin[lo] - off } else { fin[lo] + off };
            let val = y[i];
            let highroom = range - predicted;
            let lowroom = predicted;
            let room = highroom.min(lowroom) * 2;
            if val != 0 {
                step[lo] = true;
                step[hi] = true;
                step[i] = true;
                fin[i] = if val >= room {
                    if highroom > lowroom { val - lowroom + predicted } else { predicted - val + highroom - 1 }
                } else if val & 1 == 1 {
                    predicted - (val + 1) / 2
                } else {
                    predicted + val / 2
                };
            } else {
                fin[i] = predicted;
            }
        }

        // draw the curve into a buffer that also holds points beyond the block's width
        let len = out.len();
        let mut wide = vec![0f32; len.max(self.xs[1] as usize) + 1];
        let (mut lx, mut ly) = (0i32, fin[0] * self.mult);
        for &p in &self.order[1..] {
            if step[p] {
                let (x1, y1) = (self.xs[p], fin[p] * self.mult);
                render_line(lx, ly, x1, y1, &mut wide);
                lx = x1;
                ly = y1;
            }
        }
        if (lx as usize) < len {
            render_line(lx, ly, len as i32, ly, &mut wide);
        }
        out.copy_from_slice(&wide[..len]);
        Some(())
    }
}

// ---- residue -------------------------------------------------------------------------------------

impl Residue {
    /// Add the residue into `vecs` (one vector of `len` values per channel in the submap).
    /// `skip[j]` marks channels that carry no residue.
    fn decode(&self, books: &[Codebook], b: &mut Bits, vecs: &mut [Vec<f32>], skip: &[bool]) {
        let ch = vecs.len();
        // type 2 decodes all channels as one interleaved vector
        let type2 = self.kind == 2;
        let mut joined: Vec<Vec<f32>> = Vec::new();
        if type2 {
            if skip.iter().all(|&s| s) {
                return;
            }
            joined.push(vec![0f32; vecs[0].len() * ch]);
        }
        let (work, kind): (&mut [Vec<f32>], u32) = if type2 { (&mut joined[..], 1) } else { (&mut vecs[..], self.kind) };
        let used = work.len();
        let no_skip = vec![false; used];
        let skip: &[bool] = if type2 { &no_skip } else { skip };

        let actual = work[0].len();
        let begin = self.begin.min(actual);
        let end = self.end.min(actual);
        let parts = (end - begin) / self.psize;
        if parts == 0 {
            return;
        }
        let cbook = &books[self.classbook];
        let cpc = cbook.dim;
        let mut classes = vec![vec![0usize; parts + cpc]; used];

        for pass in 0..=self.maxpass {
            let mut p = 0;
            while p < parts {
                if pass == 0 {
                    for j in 0..used {
                        if skip[j] {
                            continue;
                        }
                        let Some(mut word) = cbook.read(b) else { return };
                        for i in (0..cpc).rev() {
                            classes[j][p + i] = word % self.classes;
                            word /= self.classes;
                        }
                    }
                }
                for _ in 0..cpc {
                    if p >= parts {
                        break;
                    }
                    for j in 0..used {
                        if skip[j] {
                            continue;
                        }
                        let book = self.books[classes[j][p]][pass];
                        if book < 0 {
                            continue;
                        }
                        let book = &books[book as usize];
                        if book.vectors.is_empty() {
                            return;
                        }
                        let offset = begin + p * self.psize;
                        let dim = book.dim;
                        let step = self.psize / dim;
                        let dst = &mut work[j];
                        for k in 0..step {
                            let Some(e) = book.read(b) else { return };
                            let v = book.vector(e);
                            if kind == 0 {
                                for l in 0..dim {
                                    dst[offset + k + l * step] += v[l];
                                }
                            } else {
                                for l in 0..dim {
                                    dst[offset + k * dim + l] += v[l];
                                }
                            }
                        }
                    }
                    p += 1;
                }
            }
        }
        if type2 {
            for (c, v) in vecs.iter_mut().enumerate() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = joined[0][i * ch + c];
                }
            }
        }
    }
}

// ---- the decoder ---------------------------------------------------------------------------------

/// Rising half of the Vorbis window, `len` samples long.
fn slope(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let s = (std::f64::consts::FRAC_PI_2 * (i as f64 + 0.5) / len as f64).sin();
            (std::f64::consts::FRAC_PI_2 * s * s).sin() as f32
        })
        .collect()
}

/// Order in which Vorbis channels fill the output layout (`out[i] = vorbis[ORDER[n - 1][i]]`).
pub(crate) const ORDER: [&[usize]; 8] = [
    &[0],
    &[0, 1],
    &[0, 2, 1],
    &[0, 1, 2, 3],
    &[0, 2, 1, 3, 4],
    &[0, 2, 1, 5, 3, 4],
    &[0, 2, 1, 6, 5, 3, 4],
    &[0, 2, 1, 7, 5, 6, 3, 4],
];

struct Prev {
    /// Block size of the previous block.
    n: usize,
    /// Its right half per channel, before windowing.
    tail: Vec<Vec<f32>>,
}

pub struct VorbisDecoder {
    setup: Setup,
    mdct: [Dct4; 2],
    slopes: [Vec<f32>; 2],
    prev: Option<Prev>,
    legacy: bool,
    /// Identification and comment headers met among the packets, waiting for their setup header.
    headers: Vec<Vec<u8>>,
}

impl VorbisDecoder {
    pub fn new(extradata: &[u8], params: &StreamParams) -> Result<Self> {
        let setup = Self::setup(extradata)?;
        if let StreamParams::Audio { sample_rate, channels } = params {
            if *sample_rate != setup.rate || *channels as usize != setup.channels {
                return Err(bad("the container's channel count or rate disagrees with the stream's headers"));
            }
        }
        let mdct = [Dct4::new(setup.blocksize[0] / 2), Dct4::new(setup.blocksize[1] / 2)];
        let slopes = [slope(setup.blocksize[0] / 2), slope(setup.blocksize[1] / 2)];
        Ok(VorbisDecoder { setup, mdct, slopes, prev: None, legacy: false, headers: Vec::new() })
    }

    fn setup(extradata: &[u8]) -> Result<Setup> {
        let setup = Setup::parse(extradata)?;
        if setup.channels > 8 {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "Vorbis: more than 8 channels are not supported"));
        }
        Ok(setup)
    }

    /// A header packet among the audio: a chained stream's next link starts with its three
    /// headers, and the decoder starts over with them (its rate and channels may differ).
    fn header(&mut self, p: &[u8]) -> Result<()> {
        match p.first() {
            Some(1) => self.headers = vec![p.to_vec()],
            Some(3) if self.headers.len() == 1 => self.headers.push(p.to_vec()),
            Some(5) if self.headers.len() == 2 => {
                let extra = join_headers([&self.headers[0][..], &self.headers[1], p]);
                self.headers.clear();
                let setup = Self::setup(&extra)?;
                self.mdct = [Dct4::new(setup.blocksize[0] / 2), Dct4::new(setup.blocksize[1] / 2)];
                self.slopes = [slope(setup.blocksize[0] / 2), slope(setup.blocksize[1] / 2)];
                self.setup = setup;
                self.prev = None;
            }
            _ => self.headers.clear(),
        }
        Ok(())
    }

    /// Undo channel coupling the way FFmpeg's SSE code did until May 2026, treating a magnitude of exactly
    /// zero as positive. The specification (and libvorbis, and FFmpeg since the fix) treat it as not
    /// positive; the two differ in the sign of the angle channel wherever the magnitude is zero and the
    /// angle is not, which is common in stereo streams. This exists to compare against older builds.
    pub fn legacy_zero_coupling(mut self, on: bool) -> Self {
        self.legacy = on;
        self
    }

    /// Decode the block's spectra and transform them: per channel, `n` raw time samples.
    fn block(&mut self, mode: &Mode, b: &mut Bits) -> Vec<Vec<f32>> {
        let legacy = self.legacy;
        let s = &self.setup;
        let n = s.blocksize[mode.long as usize];
        let half = n / 2;
        let ch = s.channels;
        let map = &s.mappings[mode.mapping];

        let mut floors = vec![vec![0f32; half]; ch];
        let mut unused = vec![true; ch];
        for j in 0..ch {
            let f = &s.floors[map.floor[map.mux[j]]];
            unused[j] = f.decode(&s.books, b, &mut floors[j]).is_none();
        }
        for &(m, a) in map.coupling.iter().rev() {
            if !(unused[m] && unused[a]) {
                unused[m] = false;
                unused[a] = false;
            }
        }

        let mut spec = vec![vec![0f32; half]; ch];
        for sm in 0..map.submaps {
            let chans: Vec<usize> = (0..ch).filter(|&j| map.mux[j] == sm).collect();
            if chans.is_empty() {
                continue;
            }
            let mut vecs: Vec<Vec<f32>> = chans.iter().map(|_| vec![0f32; half]).collect();
            let skip: Vec<bool> = chans.iter().map(|&j| unused[j]).collect();
            s.residues[map.residue[sm]].decode(&s.books, b, &mut vecs, &skip);
            for (v, &j) in vecs.into_iter().zip(&chans) {
                spec[j] = v;
            }
        }

        for &(m, a) in map.coupling.iter().rev() {
            let (lo, hi) = (m.min(a), m.max(a));
            let (left, right) = spec.split_at_mut(hi);
            let (first, second) = (&mut left[lo], &mut right[0]);
            let (mag, ang) = if m < a { (first, second) } else { (second, first) };
            for (mv, av) in mag.iter_mut().zip(ang.iter_mut()) {
                let (mi, ai) = (*mv, *av);
                if mi > 0.0 || (legacy && mi == 0.0) {
                    if ai > 0.0 {
                        *av = mi - ai;
                    } else {
                        *av = mi;
                        *mv = mi + ai;
                    }
                } else if ai > 0.0 {
                    *av = mi + ai;
                } else {
                    *av = mi;
                    *mv = mi - ai;
                }
            }
        }

        let mdct = &mut self.mdct[mode.long as usize];
        let mut c = vec![0f32; half];
        let r = (0..ch)
            .map(|j| {
                let mut y = vec![0f32; n];
                if unused[j] {
                    return y;
                }
                for (x, f) in spec[j].iter_mut().zip(&floors[j]) {
                    *x *= f;
                }
                mdct.run(&spec[j], 1, &mut c);
                let q = half / 2;
                y[..q].copy_from_slice(&c[q..half]);
                for (o, &v) in y[q..q + half].iter_mut().zip(c.iter().rev()) {
                    *o = -v;
                }
                for (o, &v) in y[q + half..].iter_mut().zip(&c[..q]) {
                    *o = -v;
                }
                y
            })
            .collect::<Vec<_>>();
        r
    }
}

impl Decoder for VorbisDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut b = Bits::new(&packet.data);
        if b.bit() != 0 {
            self.header(&packet.data)?;
            return Ok(Vec::new());
        }
        let modes = self.setup.modes.len();
        let m = b.get(ilog(modes as u32 - 1)) as usize;
        let Some(mode) = self.setup.modes.get(m) else { return Ok(Vec::new()) };
        let (long, mapping) = (mode.long, mode.mapping);
        if long {
            b.get(2);
        }
        let mode = Mode { long, mapping };
        let n = self.setup.blocksize[long as usize];
        let ys = self.block(&mode, &mut b);
        let ch = self.setup.channels;

        let out = self.prev.as_ref().map(|p| {
            let np = p.n;
            let ls = n.min(np) / 2; // length of the overlap slope
            let sl = &self.slopes[(n.min(np) == self.setup.blocksize[1]) as usize];
            let count = (np + n) / 4;
            let mut pcm = vec![0f64; count * ch];
            for (j, y) in ys.iter().enumerate() {
                let dst = ORDER[ch - 1].iter().position(|&o| o == j).unwrap_or(j);
                let mut buf = vec![0f32; count];
                // the previous block's falling edge: 1, then the slope reversed, then 0
                let lead = (np / 2 - ls) / 2;
                let ones = lead.min(count);
                buf[..ones].copy_from_slice(&p.tail[j][..ones]);
                for a in lead..(lead + ls).min(count) {
                    buf[a] = p.tail[j][a] * sl[ls - 1 - (a - lead)];
                }
                // the current block's left half starts (n - np) / 4 samples before `a`: 0, then the
                // slope, then 1
                let shift = (n as isize - np as isize) / 4;
                let lead_c = (n / 2 - ls) / 2;
                let within = |blo: usize, bhi: usize| -> (usize, usize) {
                    let lo = (blo as isize).max(shift);
                    let hi = (bhi as isize).min(count as isize + shift).max(lo);
                    (lo as usize, hi as usize)
                };
                let (lo, hi) = within(lead_c, lead_c + ls);
                for bi in lo..hi {
                    buf[(bi as isize - shift) as usize] += y[bi] * sl[bi - lead_c];
                }
                let (lo, hi) = within(lead_c + ls, n / 2);
                for bi in lo..hi {
                    buf[(bi as isize - shift) as usize] += y[bi];
                }
                for (a, &v) in buf.iter().enumerate() {
                    pcm[a * ch + dst] = (v as f64).clamp(-1.0, 1.0);
                }
            }
            pcm
        });
        self.prev = Some(Prev { n, tail: ys.into_iter().map(|y| y[n / 2..].to_vec()).collect() });
        Ok(match out {
            Some(samples) if !samples.is_empty() => {
                vec![Frame::Audio(AudioFrame { sample_rate: self.setup.rate, channels: ch as u16, pts: packet.pts, samples })]
            }
            _ => Vec::new(),
        })
    }
}

/// Works out how many samples each packet adds, for container readers that must give packets times.
pub struct PacketTimer {
    blocks: [usize; 2],
    long: Vec<bool>,
    mode_bits: u32,
    prev: Option<usize>,
}

impl PacketTimer {
    pub fn new(extradata: &[u8]) -> Result<Self> {
        let s = Setup::parse(extradata)?;
        Ok(PacketTimer { blocks: s.blocksize, long: s.modes.iter().map(|m| m.long).collect(), mode_bits: ilog(s.modes.len() as u32 - 1), prev: None })
    }
    pub fn channels_and_rate(extradata: &[u8]) -> Result<(u16, u32)> {
        let s = Setup::parse(extradata)?;
        Ok((s.channels as u16, s.rate))
    }
    /// Samples per channel the packet adds to the output (0 for the first packet and for non-audio ones).
    pub fn next(&mut self, packet: &[u8]) -> u32 {
        let mut b = Bits::new(packet);
        if b.bit() != 0 {
            return 0;
        }
        let Some(&long) = self.long.get(b.get(self.mode_bits) as usize) else { return 0 };
        let n = self.blocks[long as usize];
        let samples = self.prev.map_or(0, |p| (p + n) / 4);
        self.prev = Some(n);
        samples as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip_through_lacing() {
        let a = vec![1u8; 30];
        let b = vec![3u8; 300];
        let c = vec![5u8; 7];
        let joined = join_headers([&a, &b, &c]);
        let [x, y, z] = split_headers(&joined).unwrap();
        assert_eq!((x, y, z), (&a[..], &b[..], &c[..]));
        assert!(split_headers(&joined[..40]).is_err());
    }

    #[test]
    fn float_unpacking_matches_the_specification() {
        // mantissa 1, exponent 788 -> 1.0
        assert_eq!(unpack_float(1 | (788 << 21)), 1.0f32);
        assert_eq!(unpack_float(3 | (787 << 21) | 0x8000_0000), -1.5f32);
    }

    #[test]
    fn nth_root_is_the_largest_whose_power_fits() {
        assert_eq!(nth_root(81, 4), 3);
        assert_eq!(nth_root(80, 4), 2);
        assert_eq!(nth_root(1, 8), 1);
        assert_eq!(nth_root(1024, 1), 1024);
    }

    #[test]
    fn code_lengths_make_a_prefix_code() {
        // entries with lengths 1, 2, 3, 3 form the code 0, 10, 110, 111
        let mut bits: Vec<u8> = Vec::new();
        let mut put = |v: u32, n: u32| bits.extend((0..n).map(|i| ((v >> i) & 1) as u8));
        put(0x564342, 24);
        put(1, 16);
        put(4, 24);
        put(0, 1); // unordered
        put(0, 1); // not sparse
        for l in [1, 2, 3, 3] {
            put(l - 1, 5);
        }
        put(0, 4); // no lookup
        let mut bytes = vec![0u8; bits.len().div_ceil(8) + 1];
        for (i, &v) in bits.iter().enumerate() {
            bytes[i / 8] |= v << (i % 8);
        }
        let book = read_codebook(&mut Bits::new(&bytes)).unwrap();
        // the stream 0 | 10 | 110 | 111, first bit first
        let stream = [0u8, 1, 0, 1, 1, 0, 1, 1, 1];
        let mut data = vec![0u8; 2];
        for (i, &v) in stream.iter().enumerate() {
            data[i / 8] |= v << (i % 8);
        }
        let mut r = Bits::new(&data);
        assert_eq!([book.read(&mut r), book.read(&mut r), book.read(&mut r), book.read(&mut r)], [Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn floor_lines_follow_the_integer_rule() {
        let mut out = vec![0f32; 8];
        render_line(0, 0, 8, 8, &mut out);
        // y rises by one per sample: table entries 0..8
        for (i, &v) in out.iter().enumerate() {
            assert_eq!(v, FLOOR1_INVERSE_DB[i]);
        }
    }
}
