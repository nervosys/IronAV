//! Parametric stereo payload and differential parameter rows from 3GPP SP-040428, §8.4 and §8.5.

use super::{
    read_huff, Bits, Par, Ps, HUFF_ICC_DF, HUFF_ICC_DT, HUFF_IID_DF0, HUFF_IID_DF1,
    HUFF_IID_DT0, HUFF_IID_DT1, HUFF_IPD_DF, HUFF_IPD_DT, HUFF_OPD_DF, HUFF_OPD_DT,
    MAX_ENV, MAX_PAR, SLOTS,
};

const PARAMETER_COUNTS: [usize; 6] = [10, 20, 34, 10, 20, 34];
const PHASE_COUNTS: [usize; 6] = [5, 11, 17, 5, 11, 17];
const ENV_COUNTS: [[usize; 4]; 2] = [[0, 1, 2, 4], [1, 2, 3, 4]];
const LAST_SLOT: i32 = SLOTS as i32 - 1;

#[derive(Clone, Copy)]
enum Domain {
    Bounded { min: i32, max: i32 },
    Phase,
}

impl Domain {
    fn index(&self, value: i32) -> Result<i8, ()> {
        match *self {
            Self::Phase => Ok(value.rem_euclid(8) as i8),
            Self::Bounded { min, max } if (min..=max).contains(&value) => Ok(value as i8),
            Self::Bounded { .. } => Err(()),
        }
    }
}

struct Coding {
    count: usize,
    books: [usize; 2],
    domain: Domain,
}

impl Coding {
    fn envelope(&self, bits: &mut Bits, grid: &mut Par, row: usize, previous_frame_rows: usize) -> Result<(), ()> {
        let across_time = bits.flag();
        let table = self.books[usize::from(across_time)];
        let mut reference = [0; MAX_PAR];
        if across_time {
            let previous = row.checked_sub(1).unwrap_or_else(|| previous_frame_rows.saturating_sub(1));
            reference = grid[previous];
        }
        for i in 0..self.count.min(MAX_PAR) {
            let delta = read_huff(bits, table).ok_or(())?;
            let value = self.domain.index(i32::from(reference[i]) + delta)?;
            grid[row][i] = value;
            if !across_time && i + 1 < self.count.min(MAX_PAR) {
                reference[i + 1] = value;
            }
        }
        Ok(())
    }

    fn frame(&self, bits: &mut Bits, grid: &mut Par, rows: usize, previous_frame_rows: usize) -> Result<(), ()> {
        for row in 0..rows {
            self.envelope(bits, grid, row, previous_frame_rows)?;
        }
        Ok(())
    }
}

fn frame_grid(bits: &mut Bits) -> Result<(usize, [i32; MAX_ENV + 1]), ()> {
    let variable = bits.flag();
    let count = ENV_COUNTS[usize::from(variable)][bits.read(2) as usize];
    let mut edges = [-1; MAX_ENV + 1];
    for i in 1..=count {
        let edge = if variable { bits.read(5) as i32 } else { (i * SLOTS / count) as i32 - 1 };
        if edge < edges[i - 1] {
            return Err(());
        }
        edges[i] = edge;
    }
    Ok((count, edges))
}

impl Ps {
    fn configure(&mut self, bits: &mut Bits) -> Result<(), ()> {
        let intensity = bits.flag().then(|| bits.read(3) as usize);
        let coherence = bits.flag().then(|| bits.read(3) as usize);
        self.enable_ext = bits.flag();
        self.enable_iid = intensity.is_some();
        self.enable_icc = coherence.is_some();
        if let Some(mode) = intensity {
            self.nr_iid_par = *PARAMETER_COUNTS.get(mode).ok_or(())?;
            self.nr_ipdopd_par = PHASE_COUNTS[mode];
            self.iid_quant = mode >= 3;
        }
        if let Some(mode) = coherence {
            self.nr_icc_par = *PARAMETER_COUNTS.get(mode).ok_or(())?;
            self.icc_mode = mode;
        }
        Ok(())
    }

    fn phase_extension(&mut self, bits: &mut Bits) -> Result<(), ()> {
        self.enable_ipdopd = bits.flag();
        if self.enable_ipdopd {
            let ipd = Coding { count: self.nr_ipdopd_par, books: [HUFF_IPD_DF, HUFF_IPD_DT], domain: Domain::Phase };
            let opd = Coding { books: [HUFF_OPD_DF, HUFF_OPD_DT], ..ipd };
            for row in 0..self.num_env {
                ipd.envelope(bits, &mut self.ipd_par, row, self.num_env_old)?;
                opd.envelope(bits, &mut self.opd_par, row, self.num_env_old)?;
            }
        }
        bits.skip(1);
        Ok(())
    }

    fn extensions(&mut self, bits: &mut Bits) -> Result<(), ()> {
        let raw_len = bits.read(4) as usize;
        let byte_len = if raw_len == 15 { raw_len + bits.read(8) as usize } else { raw_len };
        let end = bits.pos + byte_len * 8;
        while end.saturating_sub(bits.pos) >= 8 {
            if bits.read(2) == 0 {
                self.phase_extension(bits)?;
            }
            if bits.left() < 0 || bits.pos > end {
                return Err(());
            }
        }
        bits.skip((end - bits.pos) as u32);
        Ok(())
    }

    fn close_frame(&mut self, iid_limit: i32) -> Result<(), ()> {
        if self.num_env > 0 && self.border[self.num_env] == LAST_SLOT {
            return Ok(());
        }
        let end = self.num_env;
        if end == MAX_ENV {
            return Err(());
        }
        // Extend the final cue set through the last slot. A zero-envelope frame reuses
        // the final cues of the previous frame.
        if let Some(last) = end.checked_sub(1).or_else(|| self.num_env_old.checked_sub(1)) {
            for (enabled, grid) in [
                (self.enable_iid, &mut self.iid_par),
                (self.enable_icc, &mut self.icc_par),
                (self.enable_ipdopd, &mut self.ipd_par),
                (self.enable_ipdopd, &mut self.opd_par),
            ] {
                if enabled {
                    grid[end] = grid[last];
                }
            }
        }
        if (self.enable_iid && self.iid_par[end][..self.nr_iid_par].iter().any(|&v| i32::from(v).abs() > iid_limit))
            || (self.enable_icc && self.icc_par[end][..self.nr_icc_par].iter().any(|v| !(0..=7).contains(v)))
        {
            return Err(());
        }
        self.num_env = end + 1;
        self.border[end + 1] = LAST_SLOT;
        Ok(())
    }

    fn decode_payload(&mut self, bits: &mut Bits) -> Result<(), ()> {
        let header = bits.flag();
        if header {
            self.configure(bits)?;
        }
        self.num_env_old = self.num_env;
        (self.num_env, self.border) = frame_grid(bits)?;
        self.enable_ipdopd = false;
        let iid_limit = if self.iid_quant { 15 } else { 7 };
        if self.enable_iid {
            let books = if self.iid_quant { [HUFF_IID_DF1, HUFF_IID_DT1] } else { [HUFF_IID_DF0, HUFF_IID_DT0] };
            let iid = Coding { count: self.nr_iid_par, books, domain: Domain::Bounded { min: -iid_limit, max: iid_limit } };
            iid.frame(bits, &mut self.iid_par, self.num_env, self.num_env_old)?;
        } else {
            self.iid_par.fill([0; MAX_PAR]);
        }
        if self.enable_icc {
            let icc = Coding { count: self.nr_icc_par, books: [HUFF_ICC_DF, HUFF_ICC_DT], domain: Domain::Bounded { min: 0, max: 7 } };
            icc.frame(bits, &mut self.icc_par, self.num_env, self.num_env_old)?;
        } else {
            self.icc_par.fill([0; MAX_PAR]);
        }
        if self.enable_ext {
            self.extensions(bits)?;
        }
        self.close_frame(iid_limit)?;

        self.is34_old = self.is34;
        if self.enable_iid || self.enable_icc {
            self.is34 = (self.enable_iid && self.nr_iid_par == 34) || (self.enable_icc && self.nr_icc_par == 34);
        }
        if !self.enable_ipdopd {
            self.ipd_par.fill([0; MAX_PAR]);
            self.opd_par.fill([0; MAX_PAR]);
        }
        self.start |= header;
        Ok(())
    }

    /// Consume one PS payload. Invalid or truncated parameters disable PS until a valid header.
    pub fn read(&mut self, input: &mut Bits, bits_left: i32) -> i32 {
        let mut probe = Bits { data: input.data, pos: input.pos };
        if self.decode_payload(&mut probe).is_ok() && probe.left() >= 0 {
            let used = (probe.pos - input.pos) as i32;
            if used <= bits_left {
                input.pos = probe.pos;
                return used;
            }
        }
        self.start = false;
        for grid in [&mut self.iid_par, &mut self.icc_par, &mut self.ipd_par, &mut self.opd_par] {
            grid.fill([0; MAX_PAR]);
        }
        input.pos += bits_left.max(0) as usize;
        bits_left
    }
}
