//! Block, transform and prediction tables of the AV1 decoding process (spec sections 5 and 7).
//! The scan orders, quantiser lookups and smooth weights are in `scan_tables.rs` and
//! `quant_tables.rs`; the default CDFs in `cdf_tables.rs`; the transform butterflies in `itx1d.rs`.
//! `COEFF_BASE_CTX_OFFSET` is from rav1e 0.8.1 (src/context/transform_unit.rs), BSD-2-Clause.

// Block sizes, in the spec's order.
pub const BLOCK_4X4: usize = 0;
pub const BLOCK_8X8: usize = 3;
pub const BLOCK_64X64: usize = 12;
pub const BLOCK_128X128: usize = 15;
pub const BLOCK_INVALID: usize = 22;

pub const NUM_4X4_W: [u8; 22] = [1, 1, 2, 2, 2, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 1, 4, 2, 8, 4, 16];
pub const NUM_4X4_H: [u8; 22] = [1, 2, 1, 2, 4, 2, 4, 8, 4, 8, 16, 8, 16, 32, 16, 32, 4, 1, 8, 2, 16, 4];
/// log2 of the width and height in 4-sample units.
pub const MI_W_LOG2: [u8; 22] = [0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4];
pub const MI_H_LOG2: [u8; 22] = [0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2];
pub const SIZE_GROUP: [u8; 22] = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 0, 0, 1, 1, 2, 2];
/// Largest transform that fits a block (Max_Tx_Size_Rect).
pub const MAX_TX_SIZE_RECT: [usize; 22] = [0, 5, 6, 1, 7, 8, 2, 9, 10, 3, 11, 12, 4, 4, 4, 4, 13, 14, 15, 16, 17, 18];
/// Maximum transform depth of a block (Max_Tx_Depth).
pub const MAX_TX_DEPTH_OF: [u8; 22] = [0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 4, 4, 4, 2, 2, 3, 3, 4, 4];

// Transform sizes, in the spec's order.
pub const TX_4X4: usize = 0;
pub const TX_8X8: usize = 1;
pub const TX_16X16: usize = 2;
pub const TX_32X32: usize = 3;
pub const TX_64X64: usize = 4;
pub const TX_16X64: usize = 17;
pub const TX_64X16: usize = 18;

pub const TX_W_LOG2: [u8; 19] = [2, 3, 4, 5, 6, 2, 3, 3, 4, 4, 5, 5, 6, 2, 4, 3, 5, 4, 6];
pub const TX_H_LOG2: [u8; 19] = [2, 3, 4, 5, 6, 3, 2, 4, 3, 5, 4, 6, 5, 4, 2, 5, 3, 6, 4];
/// The transform size split into four (or two) for a deeper transform split (Split_Tx_Size).
pub const SPLIT_TX_SIZE: [usize; 19] = [0, 0, 1, 2, 3, 0, 0, 1, 1, 2, 2, 3, 3, 5, 6, 7, 8, 9, 10];
/// Transform size with 64-sample dimensions clamped to 32 (Adjusted_Tx_Size).
pub const ADJUSTED_TX_SIZE: [usize; 19] = [0, 1, 2, 3, 3, 5, 6, 7, 8, 9, 10, 3, 3, 13, 14, 15, 16, 9, 10];
/// The square transform of the smaller dimension (Tx_Size_Sqr).
pub const TX_SIZE_SQR: [usize; 19] = [0, 1, 2, 3, 4, 0, 0, 1, 1, 2, 2, 3, 3, 0, 0, 1, 1, 2, 2];
/// The square transform of the larger dimension (Tx_Size_Sqr_Up).
pub const TX_SIZE_SQR_UP: [usize; 19] = [0, 1, 2, 3, 4, 1, 1, 2, 2, 3, 3, 4, 4, 2, 2, 3, 3, 4, 4];
/// Row shift of the 2D inverse transform (Transform_Row_Shift).
pub const TRANSFORM_ROW_SHIFT: [u32; 19] = [0, 1, 2, 2, 2, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2];

// Transform types, in the spec's order.
pub const DCT_DCT: u8 = 0;
pub const IDTX: u8 = 9;
pub const V_DCT: u8 = 10;
pub const H_DCT: u8 = 11;
/// One-dimensional kinds (column, row) of each 2D transform type: 0 DCT, 1 ADST, 2 flipped ADST,
/// 3 identity.
pub const TX_1D_KINDS: [(u8, u8); 16] = [(0, 0), (1, 0), (0, 1), (1, 1), (2, 0), (0, 2), (2, 2), (1, 2), (2, 1), (3, 3), (0, 3), (3, 0), (1, 3), (3, 1), (2, 3), (3, 2)];
/// The transform type of a chroma block for each intra prediction mode (Mode_To_Txfm).
pub const MODE_TO_TXFM: [u8; 14] = [0, 1, 2, 0, 3, 1, 2, 2, 1, 3, 1, 2, 3, 0];
/// Intra transform types in the two coded sets, by coded index (Tx_Type_Intra_Inv_Set1/2).
pub const TX_TYPE_INTRA_INV_SET1: [u8; 7] = [IDTX, DCT_DCT, V_DCT, H_DCT, 3, 1, 2];
pub const TX_TYPE_INTRA_INV_SET2: [u8; 5] = [IDTX, DCT_DCT, 3, 1, 2];
/// Whether an intra transform type is in each set (0 DCT only, 1 intra set 1, 2 intra set 2).
pub fn tx_type_in_intra_set(set: u8, t: u8) -> bool {
    match set {
        0 => t == DCT_DCT,
        1 => matches!(t, 0 | 1 | 2 | 3 | 9 | 10 | 11),
        _ => matches!(t, 0 | 1 | 2 | 3 | 9),
    }
}

/// Transform class: 0 2D, 1 horizontal, 2 vertical.
pub fn tx_class(t: u8) -> u8 {
    match t {
        10 | 12 | 14 => 2,
        11 | 13 | 15 => 1,
        _ => 0,
    }
}

/// Sig_Ref_Diff_Offset per class: pairs of (row, col) offsets.
pub const SIG_REF_DIFF_OFFSET: [[(usize, usize); 5]; 3] = [
    [(0, 1), (1, 0), (1, 1), (0, 2), (2, 0)],
    [(0, 1), (1, 0), (0, 2), (0, 3), (0, 4)],
    [(0, 1), (1, 0), (2, 0), (3, 0), (4, 0)],
];
/// Mag_Ref_Offset_With_Tx_Class per class.
pub const MAG_REF_OFFSET: [[(usize, usize); 3]; 3] = [[(0, 1), (1, 0), (1, 1)], [(0, 1), (1, 0), (0, 2)], [(0, 1), (1, 0), (2, 0)]];

/// The palette color context of a neighbour-score hash (Palette_Color_Context).
pub const PALETTE_COLOR_CONTEXT: [i8; 9] = [-1, -1, 0, -1, -1, 4, 3, 2, 1];

/// Intra_Filter_Taps: for each filter intra mode, eight output positions of seven neighbours.
pub const INTRA_FILTER_TAPS: [[[i8; 7]; 8]; 5] = [
    [
        [-6, 10, 0, 0, 0, 12, 0],
        [-5, 2, 10, 0, 0, 9, 0],
        [-3, 1, 1, 10, 0, 7, 0],
        [-3, 1, 1, 2, 10, 5, 0],
        [-4, 6, 0, 0, 0, 2, 12],
        [-3, 2, 6, 0, 0, 2, 9],
        [-3, 2, 2, 6, 0, 2, 7],
        [-3, 1, 2, 2, 6, 3, 5],
    ],
    [
        [-10, 16, 0, 0, 0, 10, 0],
        [-6, 0, 16, 0, 0, 6, 0],
        [-4, 0, 0, 16, 0, 4, 0],
        [-2, 0, 0, 0, 16, 2, 0],
        [-10, 16, 0, 0, 0, 0, 10],
        [-6, 0, 16, 0, 0, 0, 6],
        [-4, 0, 0, 16, 0, 0, 4],
        [-2, 0, 0, 0, 16, 0, 2],
    ],
    [
        [-8, 8, 0, 0, 0, 16, 0],
        [-8, 0, 8, 0, 0, 16, 0],
        [-8, 0, 0, 8, 0, 16, 0],
        [-8, 0, 0, 0, 8, 16, 0],
        [-4, 4, 0, 0, 0, 0, 16],
        [-4, 0, 4, 0, 0, 0, 16],
        [-4, 0, 0, 4, 0, 0, 16],
        [-4, 0, 0, 0, 4, 0, 16],
    ],
    [
        [-2, 8, 0, 0, 0, 10, 0],
        [-1, 3, 8, 0, 0, 6, 0],
        [-1, 2, 3, 8, 0, 4, 0],
        [0, 1, 2, 3, 8, 2, 0],
        [-1, 4, 0, 0, 0, 3, 10],
        [-1, 3, 4, 0, 0, 4, 6],
        [-1, 2, 3, 4, 0, 4, 4],
        [-1, 2, 2, 3, 4, 3, 3],
    ],
    [
        [-12, 14, 0, 0, 0, 14, 0],
        [-10, 0, 14, 0, 0, 12, 0],
        [-9, 0, 0, 14, 0, 11, 0],
        [-8, 0, 0, 0, 14, 10, 0],
        [-10, 12, 0, 0, 0, 0, 14],
        [-9, 1, 12, 0, 0, 0, 12],
        [-8, 0, 0, 12, 0, 1, 11],
        [-7, 0, 0, 1, 12, 1, 9],
    ],
];

/// Mode_To_Angle for the directional intra modes (V, H, D45, D135, D113, D157, D203, D67).
pub fn mode_to_angle(mode: u8) -> i32 {
    match mode {
        1 => 90,
        2 => 180,
        3 => 45,
        4 => 135,
        5 => 113,
        6 => 157,
        7 => 203,
        8 => 67,
        _ => 0,
    }
}

/// Dr_Intra_Derivative: the reciprocal of the slope of a directional prediction angle.
pub fn dr_intra_derivative(p: i32) -> i32 {
    match p {
        3 => 1023,
        6 => 547,
        9 => 372,
        14 => 273,
        17 => 215,
        20 => 178,
        23 => 151,
        26 => 132,
        29 => 116,
        32 => 102,
        36 => 90,
        39 => 80,
        42 => 71,
        45 => 64,
        48 => 57,
        51 => 51,
        54 => 45,
        58 => 40,
        61 => 35,
        64 => 31,
        67 => 27,
        70 => 23,
        73 => 19,
        76 => 15,
        81 => 11,
        84 => 7,
        87 => 3,
        _ => 0,
    }
}

/// Intra_Edge_Kernel for the three edge filter strengths.
pub const INTRA_EDGE_KERNEL: [[i32; 5]; 3] = [[0, 4, 8, 4, 0], [0, 5, 6, 5, 0], [2, 4, 4, 4, 2]];

/// Coeff_Base_Ctx_Offset (rav1e 0.8.1, src/context/transform_unit.rs): the context offset by transform size and position.
pub static COEFF_BASE_CTX_OFFSET: [[[i8; 5]; 5]; 19] = [ [ [ 0, 1, 6, 6, 0], [ 1, 6, 6, 21, 0], [ 6, 6, 21, 21, 0], [ 6, 21, 21, 21, 0], [ 0, 0, 0, 0, 0] ], [ [ 0, 1, 6, 6, 21], [ 1, 6, 6, 21, 21], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 1, 6, 6, 21], [ 1, 6, 6, 21, 21], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 1, 6, 6, 21], [ 1, 6, 6, 21, 21], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 1, 6, 6, 21], [ 1, 6, 6, 21, 21], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 11, 11, 11, 0], [11, 11, 11, 11, 0], [ 6, 6, 21, 21, 0], [ 6, 21, 21, 21, 0], [21, 21, 21, 21, 0] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [ 0, 0, 0, 0, 0] ], [ [ 0, 11, 11, 11, 11], [11, 11, 11, 11, 11], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21] ], [ [ 0, 11, 11, 11, 11], [11, 11, 11, 11, 11], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21] ], [ [ 0, 11, 11, 11, 11], [11, 11, 11, 11, 11], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21] ], [ [ 0, 11, 11, 11, 0], [11, 11, 11, 11, 0], [ 6, 6, 21, 21, 0], [ 6, 21, 21, 21, 0], [21, 21, 21, 21, 0] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [ 0, 0, 0, 0, 0] ], [ [ 0, 11, 11, 11, 11], [11, 11, 11, 11, 11], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21] ], [ [ 0, 11, 11, 11, 11], [11, 11, 11, 11, 11], [ 6, 6, 21, 21, 21], [ 6, 21, 21, 21, 21], [21, 21, 21, 21, 21] ], [ [ 0, 16, 6, 6, 21], [16, 16, 6, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21], [16, 16, 21, 21, 21] ] ];
