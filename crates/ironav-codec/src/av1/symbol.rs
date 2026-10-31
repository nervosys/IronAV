//! The symbol decoder of the tile data (spec section 8.2) and the adaptive CDF contexts it reads.
//! A CDF is stored as in `cdf_tables.rs`: the inverse of the spec's cumulative values, the last
//! symbol's entry implied, and the adaptation counter in the final element.

use super::cdf_tables::*;

const EC_PROB_SHIFT: u32 = 6;
const EC_MIN_PROB: u32 = 4;

pub struct SymbolDecoder<'a> {
    data: &'a [u8],
    bit_pos: usize,
    value: u32,
    range: u32,
    max_bits: i64,
    /// Whether CDFs adapt (disable_cdf_update is 0).
    pub adapt: bool,
}

impl<'a> SymbolDecoder<'a> {
    /// init_symbol over a tile's bytes.
    pub fn new(data: &'a [u8]) -> Self {
        let sz = data.len();
        let num_bits = (sz * 8).min(15) as u32;
        let mut d = SymbolDecoder { data, bit_pos: 0, value: 0, range: 1 << 15, max_bits: 8 * sz as i64 - 15, adapt: true };
        let buf = d.bits(num_bits);
        let padded = buf << (15 - num_bits);
        d.value = ((1 << 15) - 1) ^ padded;
        d
    }

    fn bits(&mut self, n: u32) -> u32 {
        let mut x = 0;
        for _ in 0..n {
            let byte = self.data.get(self.bit_pos >> 3).copied().unwrap_or(0);
            x = (x << 1) | ((byte >> (7 - (self.bit_pos & 7))) & 1) as u32;
            self.bit_pos += 1;
        }
        x
    }

    fn renormalize(&mut self) {
        let bits = 15 - (31 - self.range.leading_zeros());
        self.range <<= bits;
        let num_bits = (bits as i64).min(self.max_bits.max(0)) as u32;
        let new_data = self.bits(num_bits);
        let padded = new_data << (bits - num_bits);
        self.value = padded ^ (((self.value + 1) << bits) - 1);
        self.max_bits -= bits as i64;
    }

    /// read_symbol with a stored CDF of `icdf.len()` symbols, without adaptation.
    fn decode(&mut self, icdf: &[u16]) -> usize {
        let n = icdf.len() as u32;
        let mut cur = self.range;
        let mut prev;
        let mut sym = 0u32;
        loop {
            prev = cur;
            let f = if sym + 1 < n { icdf[sym as usize] as u32 } else { 0 };
            cur = (((self.range >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)) + EC_MIN_PROB * (n - sym - 1);
            if self.value >= cur {
                break;
            }
            sym += 1;
        }
        self.range = prev - cur;
        self.value -= cur;
        self.renormalize();
        sym as usize
    }

    /// read_symbol with adaptation of the CDF.
    pub fn symbol(&mut self, cdf: &mut [u16]) -> usize {
        let sym = self.decode(cdf);
        if self.adapt {
            update_cdf(cdf, sym);
        }
        sym
    }

    /// A bool with probability 1/2 (read_bool).
    pub fn bool(&mut self) -> bool {
        self.decode(&[1 << 14, 0]) == 1
    }

    /// A value of `n` bits, most significant first (L(n)).
    pub fn literal(&mut self, n: u32) -> u32 {
        let mut x = 0;
        for _ in 0..n {
            x = (x << 1) | self.bool() as u32;
        }
        x
    }

    /// NS(n) read through literal bits.
    pub fn ns(&mut self, n: u32) -> u32 {
        let w = 32 - n.leading_zeros();
        let m = (1u32 << w) - n;
        let v = self.literal(w - 1);
        if v < m {
            return v;
        }
        let extra = self.literal(1);
        (v << 1) - m + extra
    }

    /// A value of `n` bits with a probability given by a single stored CDF entry (used for the
    /// split-or-partition decisions at frame edges).
    pub fn bool_with_prob(&mut self, icdf0: u16) -> bool {
        self.decode(&[icdf0, 0]) == 1
    }
}

/// The adaptation of a CDF after symbol `sym` was read.
pub(crate) fn update_cdf(cdf: &mut [u16], sym: usize) {
    let n = cdf.len();
    let count = cdf[n - 1];
    let rate = 3 + (count >> 4) as u32 + (n >> 1).min(2) as u32;
    cdf[n - 1] = count + (count < 32) as u16;
    for (i, v) in cdf[..n - 1].iter_mut().enumerate() {
        if i >= sym {
            *v -= *v >> rate;
        } else {
            *v += (32768 - *v) >> rate;
        }
    }
}

/// The CDFs of one motion vector component.
#[derive(Clone)]
pub struct MvComp {
    pub sign: [u16; 2],
    pub class: [u16; 11],
    pub class0: [u16; 2],
    pub class0_fr: [[u16; 4]; 2],
    pub class0_hp: [u16; 2],
    pub fr: [u16; 4],
    pub hp: [u16; 2],
    pub bits: [[u16; 2]; 10],
}

/// The motion vector CDFs of one MV context.
#[derive(Clone)]
pub struct MvCdfs {
    pub joint: [u16; 4],
    pub comp: [MvComp; 2],
}

impl MvCdfs {
    fn new() -> MvCdfs {
        let c = MvComp {
            sign: default_mv_sign_cdf,
            class: default_mv_class_cdf,
            class0: default_mv_class0_cdf,
            class0_fr: default_mv_class0_fr_cdf,
            class0_hp: default_mv_class0_hp_cdf,
            fr: default_mv_fr_cdf,
            hp: default_mv_hp_cdf,
            bits: default_mv_bits_cdf,
        };
        MvCdfs { joint: default_mv_joint_cdf, comp: [c.clone(), c] }
    }
}

impl Counted for MvCdfs {
    fn clear_counts(&mut self) {
        self.joint.clear_counts();
        for c in self.comp.iter_mut() {
            c.sign.clear_counts();
            c.class.clear_counts();
            c.class0.clear_counts();
            c.class0_fr.clear_counts();
            c.class0_hp.clear_counts();
            c.fr.clear_counts();
            c.hp.clear_counts();
            c.bits.clear_counts();
        }
    }
}

/// Every adaptive CDF of a tile. Intra frames use the first group, inter frames all of them.
#[derive(Clone)]
pub struct Cdfs {
    pub kf_y_mode: [[[u16; INTRA_MODES]; KF_MODE_CONTEXTS]; KF_MODE_CONTEXTS],
    pub angle_delta: [[u16; 2 * MAX_ANGLE_DELTA + 1]; DIRECTIONAL_MODES],
    pub uv_mode: [[u16; INTRA_MODES]; INTRA_MODES],
    pub uv_mode_cfl: [[u16; UV_INTRA_MODES]; INTRA_MODES],
    pub partition_w8: [[u16; 4]; PARTITION_TYPES],
    pub partition: [[u16; EXT_PARTITION_TYPES]; 3 * PARTITION_TYPES],
    pub partition_w128: [[u16; 8]; PARTITION_TYPES],
    pub intra_tx_1: [[[u16; 7]; INTRA_MODES]; TX_SIZE_SQR_CONTEXTS],
    pub intra_tx_2: [[[u16; 5]; INTRA_MODES]; TX_SIZE_SQR_CONTEXTS],
    pub cfl_sign: [u16; CFL_JOINT_SIGNS],
    pub cfl_alpha: [[u16; CFL_ALPHABET_SIZE]; CFL_ALPHA_CONTEXTS],
    pub palette_y_size: [[u16; PALETTE_SIZES]; PALETTE_BSIZE_CTXS],
    pub palette_uv_size: [[u16; PALETTE_SIZES]; PALETTE_BSIZE_CTXS],
    pub palette_y_mode: [[[u16; 2]; PALETTE_Y_MODE_CONTEXTS]; PALETTE_BSIZE_CTXS],
    pub palette_uv_mode: [[u16; 2]; PALETTE_UV_MODE_CONTEXTS],
    pub palette_y_color: [[[u16; PALETTE_COLORS]; PALETTE_COLOR_INDEX_CONTEXTS]; PALETTE_SIZES],
    pub palette_uv_color: [[[u16; PALETTE_COLORS]; PALETTE_COLOR_INDEX_CONTEXTS]; PALETTE_SIZES],
    pub intrabc: [u16; 2],
    pub filter_intra_mode: [u16; FILTER_INTRA_MODES],
    pub filter_intra: [[u16; 2]; BLOCK_SIZES_ALL],
    pub skip: [[u16; 2]; SKIP_CONTEXTS],
    pub delta_q: [u16; DELTA_Q_PROBS + 1],
    pub delta_lf_multi: [[u16; DELTA_LF_PROBS + 1]; FRAME_LF_COUNT],
    pub delta_lf: [u16; DELTA_LF_PROBS + 1],
    pub seg_tree: [u16; MAX_SEGMENTS],
    pub spatial_seg: [[u16; MAX_SEGMENTS]; SPATIAL_PREDICTION_PROBS],
    pub tx_size_8x8: [[u16; MAX_TX_DEPTH]; TX_SIZE_CONTEXTS],
    pub tx_size: [[[u16; MAX_TX_DEPTH + 1]; TX_SIZE_CONTEXTS]; BIG_TX_CATS],
    pub switchable_restore: [u16; RESTORE_SWITCHABLE_TYPES],
    pub wiener: [u16; 2],
    pub sgrproj: [u16; 2],
    pub dc_sign: [[[u16; 2]; DC_SIGN_CONTEXTS]; PLANE_TYPES],
    pub txb_skip: [[[u16; 2]; TXB_SKIP_CONTEXTS]; TX_SIZES],
    pub eob_extra: [[[[u16; 2]; EOB_COEF_CONTEXTS]; PLANE_TYPES]; TX_SIZES],
    pub eob16: [[[u16; 5]; 2]; PLANE_TYPES],
    pub eob32: [[[u16; 6]; 2]; PLANE_TYPES],
    pub eob64: [[[u16; 7]; 2]; PLANE_TYPES],
    pub eob128: [[[u16; 8]; 2]; PLANE_TYPES],
    pub eob256: [[[u16; 9]; 2]; PLANE_TYPES],
    pub eob512: [[[u16; 10]; 2]; PLANE_TYPES],
    pub eob1024: [[[u16; 11]; 2]; PLANE_TYPES],
    pub coeff_base_eob: [[[[u16; 3]; SIG_COEF_CONTEXTS_EOB]; PLANE_TYPES]; TX_SIZES],
    pub coeff_base: [[[[u16; 4]; SIG_COEF_CONTEXTS]; PLANE_TYPES]; TX_SIZES],
    pub coeff_br: [[[[u16; BR_CDF_SIZE]; LEVEL_CONTEXTS]; PLANE_TYPES]; TX_SIZES],
    pub y_mode: [[u16; INTRA_MODES]; BLOCK_SIZE_GROUPS],
    pub inter_tx_1: [[u16; 16]; TX_SIZE_SQR_CONTEXTS],
    pub inter_tx_2: [[u16; 12]; TX_SIZE_SQR_CONTEXTS],
    pub inter_tx_3: [[u16; 2]; TX_SIZE_SQR_CONTEXTS],
    pub interp_filter: [[u16; 3]; 16],
    pub newmv: [[u16; 2]; 6],
    pub zeromv: [[u16; 2]; 2],
    pub refmv: [[u16; 2]; 6],
    pub drl: [[u16; 2]; 3],
    pub compound_mode: [[u16; 8]; 8],
    pub interintra: [[u16; 2]; BLOCK_SIZE_GROUPS],
    pub interintra_mode: [[u16; 4]; BLOCK_SIZE_GROUPS],
    pub wedge_interintra: [[u16; 2]; BLOCK_SIZES_ALL],
    pub compound_type: [[u16; 2]; BLOCK_SIZES_ALL],
    pub wedge_idx: [[u16; 16]; BLOCK_SIZES_ALL],
    pub motion_mode: [[u16; 3]; BLOCK_SIZES_ALL],
    pub obmc: [[u16; 2]; BLOCK_SIZES_ALL],
    pub intra_inter: [[u16; 2]; 4],
    pub comp_mode: [[u16; 2]; 5],
    pub comp_ref_type: [[u16; 2]; 5],
    pub uni_comp_ref: [[[u16; 2]; 3]; 3],
    pub single_ref: [[[u16; 2]; 6]; 3],
    pub comp_ref: [[[u16; 2]; 3]; 3],
    pub comp_bwdref: [[[u16; 2]; 2]; 3],
    pub txfm_partition: [[u16; 2]; 21],
    pub skip_mode: [[u16; 2]; 3],
    pub compound_idx: [[u16; 2]; 6],
    pub comp_group_idx: [[u16; 2]; 6],
    pub segment_pred: [[u16; 2]; 3],
    /// By MV context: 0 inter prediction, 1 intra block copy.
    pub mv: [MvCdfs; 2],
}

/// The quantiser context of the coefficient CDFs for a base quantiser index.
pub fn coeff_qctx(base_q_idx: i32) -> usize {
    match base_q_idx {
        ..=20 => 0,
        21..=60 => 1,
        61..=120 => 2,
        _ => 3,
    }
}

impl Cdfs {
    /// The default CDFs for a frame whose base quantiser index is `base_q_idx` (init_non_coeff_cdfs
    /// and init_coeff_cdfs).
    pub fn new(base_q_idx: i32) -> Box<Cdfs> {
        let q = coeff_qctx(base_q_idx);
        Box::new(Cdfs {
            kf_y_mode: default_kf_y_mode_cdf,
            angle_delta: default_angle_delta_cdf,
            uv_mode: default_uv_mode_cdf,
            uv_mode_cfl: default_uv_mode_cfl_cdf,
            partition_w8: default_partition_w8_cdf,
            partition: default_partition_cdf,
            partition_w128: default_partition_w128_cdf,
            intra_tx_1: default_intra_tx_1_cdf,
            intra_tx_2: default_intra_tx_2_cdf,
            cfl_sign: default_cfl_sign_cdf,
            cfl_alpha: default_cfl_alpha_cdf,
            palette_y_size: default_palette_y_size_cdf,
            palette_uv_size: default_palette_uv_size_cdf,
            palette_y_mode: default_palette_y_mode_cdfs,
            palette_uv_mode: default_palette_uv_mode_cdfs,
            palette_y_color: default_palette_y_color_index_cdf,
            palette_uv_color: default_palette_uv_color_index_cdf,
            intrabc: default_intrabc_cdf,
            filter_intra_mode: default_filter_intra_mode_cdf,
            filter_intra: default_filter_intra_cdfs,
            skip: default_skip_cdfs,
            delta_q: default_delta_q_cdf,
            delta_lf_multi: default_delta_lf_multi_cdf,
            delta_lf: default_delta_lf_cdf,
            seg_tree: default_seg_tree_cdf,
            spatial_seg: default_spatial_pred_seg_tree_cdf,
            tx_size_8x8: default_tx_size_8x8_cdf,
            tx_size: default_tx_size_cdf,
            switchable_restore: default_switchable_restore_cdf,
            wiener: default_wiener_restore_cdf,
            sgrproj: default_sgrproj_restore_cdf,
            dc_sign: av1_default_dc_sign_cdfs[q],
            txb_skip: av1_default_txb_skip_cdfs[q],
            eob_extra: av1_default_eob_extra_cdfs[q],
            eob16: av1_default_eob_multi16_cdfs[q],
            eob32: av1_default_eob_multi32_cdfs[q],
            eob64: av1_default_eob_multi64_cdfs[q],
            eob128: av1_default_eob_multi128_cdfs[q],
            eob256: av1_default_eob_multi256_cdfs[q],
            eob512: av1_default_eob_multi512_cdfs[q],
            eob1024: av1_default_eob_multi1024_cdfs[q],
            coeff_base_eob: av1_default_coeff_base_eob_multi_cdfs[q],
            coeff_base: av1_default_coeff_base_multi_cdfs[q],
            coeff_br: av1_default_coeff_lps_multi_cdfs[q],
            y_mode: default_if_y_mode_cdf,
            inter_tx_1: default_inter_tx_1_cdf,
            inter_tx_2: default_inter_tx_2_cdf,
            inter_tx_3: default_inter_tx_3_cdf,
            interp_filter: default_switchable_interp_cdf,
            newmv: default_newmv_cdf,
            zeromv: default_zeromv_cdf,
            refmv: default_refmv_cdf,
            drl: default_drl_cdf,
            compound_mode: default_compound_mode_cdf,
            interintra: default_interintra_cdf,
            interintra_mode: default_interintra_mode_cdf,
            wedge_interintra: default_wedge_interintra_cdf,
            compound_type: default_compound_type_cdf,
            wedge_idx: default_wedge_idx_cdf,
            motion_mode: default_motion_mode_cdf,
            obmc: default_obmc_cdf,
            intra_inter: default_intra_inter_cdf,
            comp_mode: default_comp_mode_cdf,
            comp_ref_type: default_comp_ref_type_cdf,
            uni_comp_ref: default_uni_comp_ref_cdf,
            single_ref: default_single_ref_cdf,
            comp_ref: default_comp_ref_cdf,
            comp_bwdref: default_comp_bwdref_cdf,
            txfm_partition: default_txfm_partition_cdf,
            skip_mode: default_skip_mode_cdfs,
            compound_idx: default_compound_idx_cdfs,
            comp_group_idx: default_comp_group_idx_cdfs,
            segment_pred: default_segment_pred_cdf,
            mv: [MvCdfs::new(), MvCdfs::new()],
        })
    }

    /// Zeroes every adaptation counter, as a frame's saved CDFs are stored.
    pub fn clear_counts(&mut self) {
        macro_rules! clear {
            ($($f:ident),*) => { $(self.$f.clear_counts();)* };
        }
        clear!(kf_y_mode, angle_delta, uv_mode, uv_mode_cfl, partition_w8, partition, partition_w128, intra_tx_1, intra_tx_2, cfl_sign, cfl_alpha);
        clear!(palette_y_size, palette_uv_size, palette_y_mode, palette_uv_mode, intrabc, filter_intra_mode, filter_intra, skip, delta_q, delta_lf_multi);
        clear!(delta_lf, seg_tree, spatial_seg, tx_size_8x8, tx_size, switchable_restore, wiener, sgrproj, dc_sign, txb_skip, eob_extra, eob16, eob32);
        clear!(eob64, eob128, eob256, eob512, eob1024, coeff_base_eob, coeff_base, coeff_br);
        clear!(y_mode, inter_tx_1, inter_tx_2, inter_tx_3, interp_filter, newmv, zeromv, refmv, drl, compound_mode, interintra, interintra_mode);
        clear!(wedge_interintra, compound_type, wedge_idx, motion_mode, obmc, intra_inter, comp_mode, comp_ref_type, uni_comp_ref, single_ref);
        clear!(comp_ref, comp_bwdref, txfm_partition, skip_mode, compound_idx, comp_group_idx, segment_pred, mv);
        // a palette colour CDF of n symbols keeps its counter at n - 1
        for (i, (y, uv)) in self.palette_y_color.iter_mut().zip(self.palette_uv_color.iter_mut()).enumerate() {
            for c in y.iter_mut().chain(uv.iter_mut()) {
                c[i + 1] = 0;
            }
        }
    }
}

trait Counted {
    fn clear_counts(&mut self);
}

impl<const N: usize> Counted for [u16; N] {
    fn clear_counts(&mut self) {
        self[N - 1] = 0;
    }
}

impl<T: Counted, const M: usize> Counted for [T; M] {
    fn clear_counts(&mut self) {
        self.iter_mut().for_each(Counted::clear_counts);
    }
}
