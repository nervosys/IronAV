//! Sample adaptive offset decisions: per CTB, the band or edge offsets that cut the squared error of
//! the deblocked picture most, net of their bits at lambda, or the left or up CTB's parameters when
//! those cut it more. The decoder's own SAO filter applies the chosen parameters.

use super::super::frame::{CurPic, Sao};
use super::super::ps::Sps;
use super::cu::Source;

/// The largest offset magnitude of 8-bit video.
const MAX_OFF: i32 = 7;
/// The edge classes: the offsets to the two neighbours of each.
const EDGE: [[(isize, isize); 2]; 4] = [[(-1, 0), (1, 0)], [(0, -1), (0, 1)], [(-1, -1), (1, 1)], [(1, -1), (-1, 1)]];

/// Per CTB and component: (count, sum of source minus deblocked sample) per band, and per edge
/// class and category (category 0 takes no offset).
#[derive(Clone, Copy, Default)]
struct Stats {
    band: [(i64, i64); 32],
    edge: [[(i64, i64); 5]; 4],
}

/// The squared error an offset `o` removes from `n` samples whose errors sum to `s`.
fn gain(s: i64, n: i64, o: i32) -> f64 {
    let o = o as f64;
    2.0 * o * s as f64 - o * o * n as f64
}

fn best_offset(s: i64, n: i64, lo: i32, hi: i32) -> i32 {
    if n == 0 {
        return 0;
    }
    ((s as f64 / n as f64).round() as i32).clamp(lo, hi)
}

/// Statistics of the samples of the window `r` (x0, y0, x1, y1) of a plane of `pw` x `ph`.
fn stats(src: &[u16], rec: &[u16], pw: usize, ph: usize, r: (usize, usize, usize, usize)) -> Stats {
    let mut st = Stats::default();
    for y in r.1..r.3 {
        for x in r.0..r.2 {
            let v = rec[y * pw + x] as i32;
            let e = src[y * pw + x] as i64 - v as i64;
            let b = (v >> 3) as usize;
            st.band[b].0 += 1;
            st.band[b].1 += e;
            for (k, nb) in EDGE.iter().enumerate() {
                let at = |(dx, dy): (isize, isize)| -> Option<i32> {
                    let (nx, ny) = (x as isize + dx, y as isize + dy);
                    (nx >= 0 && ny >= 0 && (nx as usize) < pw && (ny as usize) < ph).then(|| rec[ny as usize * pw + nx as usize] as i32)
                };
                if let (Some(a), Some(b)) = (at(nb[0]), at(nb[1])) {
                    let s = 2 + (v - a).signum() + (v - b).signum();
                    let cat = [1usize, 2, 0, 3, 4][s as usize];
                    if cat > 0 {
                        st.edge[k][cat].0 += 1;
                        st.edge[k][cat].1 += e;
                    }
                }
            }
        }
    }
    st
}

/// The bits of four offsets (magnitude, a unary tail, and a sign for band offsets).
fn offset_bits(off: &[i16; 4], band: bool) -> f64 {
    off.iter().map(|&o| o.abs() as f64 + if o.abs() < MAX_OFF as i16 { 1.0 } else { 0.0 } + if band && o != 0 { 1.0 } else { 0.0 }).sum()
}

/// The best band offsets of a component: (gain, bits, first band, offsets).
fn best_band(st: &Stats) -> (f64, f64, u8, [i16; 4]) {
    let mut o = [0i32; 32];
    let mut g = [0f64; 32];
    for b in 0..32 {
        let (n, s) = st.band[b];
        o[b] = best_offset(s, n, -MAX_OFF, MAX_OFF);
        g[b] = gain(s, n, o[b]);
    }
    let (mut best, mut pos) = (f64::MIN, 0usize);
    for p in 0..32 {
        let tot: f64 = (0..4).map(|k| g[(p + k) & 31]).sum();
        if tot > best {
            best = tot;
            pos = p;
        }
    }
    let off: [i16; 4] = std::array::from_fn(|k| o[(pos + k) & 31] as i16);
    (best, offset_bits(&off, true) + 5.0, pos as u8, off)
}

/// The best edge offsets of a component for edge class `class`: (gain, bits, offsets). Categories
/// 1 and 2 take positive offsets, 3 and 4 negative ones.
fn best_edge(st: &Stats, class: usize) -> (f64, f64, [i16; 4]) {
    let mut g = 0.0;
    let off: [i16; 4] = std::array::from_fn(|k| {
        let (n, s) = st.edge[class][k + 1];
        let (lo, hi) = if k < 2 { (0, MAX_OFF) } else { (-MAX_OFF, 0) };
        let o = best_offset(s, n, lo, hi);
        g += gain(s, n, o);
        o as i16
    });
    (g, offset_bits(&off, false), off)
}

/// The squared error the SAO parameters `s` remove from component `c`.
fn gain_of(st: &Stats, s: &Sao, c: usize) -> f64 {
    let off = s.offset[c];
    match s.kind[c] {
        1 => (0..4).map(|k| gain(st.band[(s.band_pos[c] as usize + k) & 31].1, st.band[(s.band_pos[c] as usize + k) & 31].0, off[k] as i32)).sum(),
        2 => (0..4).map(|k| {
            let (n, sm) = st.edge[s.eo_class[c] as usize][k + 1];
            gain(sm, n, off[k] as i32)
        }).sum(),
        _ => 0.0,
    }
}

/// The SAO parameters of a CTB: the kind of each component, the band or class and the offsets.
/// Chroma shares the kind and the edge class, and has its own band and offsets.
fn set(s: &mut Sao, c: usize, kind: u8, class: u8, pos: u8, off: [i16; 4]) {
    s.kind[c] = kind;
    s.band_pos[c] = if kind == 1 { pos } else { 0 };
    s.eo_class[c] = if kind == 2 { class } else { 0 };
    s.offset[c] = if kind == 0 { [0; 4] } else { off };
}

/// The SAO parameters decided for each CTB of the deblocked picture `pic`, against its source.
pub fn decide(pic: &CurPic, src: &Source, sps: &Sps, lambda: f64) -> Vec<Sao> {
    let (pw, ph) = (pic.frame.width, pic.frame.height);
    let (cw, ch) = (pic.frame.cwidth, pic.frame.cheight);
    let (ctb_w, ctb_h) = (sps.ctb_w, sps.ctb_h);
    let ctb = 1usize << sps.log2_ctb;
    let mut out = vec![Sao::default(); ctb_w * ctb_h];
    let rec = &pic.frame.planes;
    for ry in 0..ctb_h {
        for rx in 0..ctb_w {
            let rs = ry * ctb_w + rx;
            let mut st = [Stats::default(); 3];
            for (c, s) in st.iter_mut().enumerate() {
                let (pwc, phc, cs) = if c == 0 { (pw, ph, ctb) } else { (cw, ch, ctb / 2) };
                let r = (rx * cs, ry * cs, ((rx + 1) * cs).min(pwc), ((ry + 1) * cs).min(phc));
                *s = stats(&src.planes[c], &rec[c], pwc, phc, r);
            }
            // the cheapest choice for luma, then for the two chroma components together
            let mut best_l = (lambda, Sao::default());
            let mut best_c = (lambda, Sao::default());
            let mut try_l = |cost: f64, s: Sao| {
                if cost < best_l.0 {
                    best_l = (cost, s);
                }
            };
            let mut try_c = |cost: f64, s: Sao| {
                if cost < best_c.0 {
                    best_c = (cost, s);
                }
            };
            let (gb, bb, pb, ob) = best_band(&st[0]);
            let mut s = Sao::default();
            set(&mut s, 0, 1, 0, pb, ob);
            try_l(lambda * (bb + 2.0) - gb, s);
            let (mut gcb, mut bcb, mut pcb, mut ocb) = (0.0, 0.0, [0u8; 2], [[0i16; 4]; 2]);
            for c in 1..3 {
                let (g, b, p, o) = best_band(&st[c]);
                gcb += g;
                bcb += b;
                pcb[c - 1] = p;
                ocb[c - 1] = o;
            }
            let mut sc = Sao::default();
            set(&mut sc, 1, 1, 0, pcb[0], ocb[0]);
            set(&mut sc, 2, 1, 0, pcb[1], ocb[1]);
            try_c(lambda * (bcb + 2.0) - gcb, sc);
            for class in 0..4u8 {
                let (g, b, o) = best_edge(&st[0], class as usize);
                let mut s = Sao::default();
                set(&mut s, 0, 2, class, 0, o);
                try_l(lambda * (b + 2.0 + 2.0) - g, s);
                let (mut g, mut b) = (0.0, 0.0);
                let mut oc = [[0i16; 4]; 2];
                for c in 1..3 {
                    let (gc, bc, oo) = best_edge(&st[c], class as usize);
                    g += gc;
                    b += bc;
                    oc[c - 1] = oo;
                }
                let mut sc = Sao::default();
                set(&mut sc, 1, 2, class, 0, oc[0]);
                set(&mut sc, 2, 2, class, 0, oc[1]);
                try_c(lambda * (b + 2.0 + 2.0) - g, sc);
            }
            let mut choice = best_l.1;
            for c in 1..3 {
                choice.kind[c] = best_c.1.kind[c];
                choice.band_pos[c] = best_c.1.band_pos[c];
                choice.eo_class[c] = best_c.1.eo_class[c];
                choice.offset[c] = best_c.1.offset[c];
            }
            let mut cost = best_l.0 + best_c.0;
            // a neighbour's parameters: one flag, and the gain they still give here
            let mut try_merge = |n: &Sao| {
                let g: f64 = (0..3).map(|c| gain_of(&st[c], n, c)).sum();
                let m = lambda - g;
                if m < cost {
                    cost = m;
                    choice = *n;
                }
            };
            if rx > 0 && pic.ctb_slice_addr[rs - 1] == pic.ctb_slice_addr[rs] {
                try_merge(&out[rs - 1]);
            }
            if ry > 0 && pic.ctb_slice_addr[rs - ctb_w] == pic.ctb_slice_addr[rs] {
                try_merge(&out[rs - ctb_w]);
            }
            out[rs] = choice;
        }
    }
    out
}
