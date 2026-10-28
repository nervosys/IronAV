//! Constant tables for the HEVC (H.265) decoder.
//!
//! Translated from FFmpeg libavcodec/hevc (cabac.c, data.c, dsp.c, filter.c,
//! ps.c, pred_template.c). Values were extracted mechanically from the C
//! sources so they match exactly.

#![allow(dead_code)]

/// Size of the CABAC context state array (HEVC_CONTEXTS in hevcdec.h).
pub const HEVC_CONTEXTS: usize = 199;

/// Contexts actually initialized in C init_values (sum of NUM_BINS); the rest of HEVC_CONTEXTS are zero.
pub const HEVC_CONTEXTS_USED: usize = 179;

/// CABAC context init values [initType][ctxIdx]; the 179 values of C init_values[3][HEVC_CONTEXTS] copied verbatim, zero-padded to HEVC_CONTEXTS as in C; offsets below index into it.
pub const INIT_VALUES: [[u8; 199]; 3] = [
    [
        153, 200, 139, 141, 157, 154, 154, 154, 154, 154, 154, 154, 154, 184, 154, 154,
        154, 184, 63, 139, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154,
        154, 154, 154, 154, 154, 153, 138, 138, 111, 141, 94, 138, 182, 154, 154, 139,
        139, 139, 139, 139, 139, 110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111,
        143, 127, 111, 79, 108, 123, 63, 110, 110, 124, 125, 140, 153, 125, 127, 140,
        109, 111, 143, 127, 111, 79, 108, 123, 63, 91, 171, 134, 141, 111, 111, 125,
        110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179,
        153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136,
        153, 136, 139, 111, 136, 139, 111, 141, 111, 140, 92, 137, 138, 140, 152, 138,
        139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179, 166, 182, 140, 227, 122,
        197, 138, 153, 136, 167, 152, 152, 154, 154, 154, 154, 154, 154, 154, 154, 154,
        154, 154, 154, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0,
    ],
    [
        153, 185, 107, 139, 126, 154, 197, 185, 201, 154, 154, 154, 149, 154, 139, 154,
        154, 154, 152, 139, 110, 122, 95, 79, 63, 31, 31, 153, 153, 153, 153, 140,
        198, 140, 198, 168, 79, 124, 138, 94, 153, 111, 149, 107, 167, 154, 154, 139,
        139, 139, 139, 139, 139, 125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110,
        111, 111, 95, 94, 108, 123, 108, 125, 110, 94, 110, 95, 79, 125, 111, 110,
        78, 110, 111, 111, 95, 94, 108, 123, 108, 121, 140, 61, 154, 155, 154, 139,
        153, 139, 123, 123, 63, 153, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136,
        153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 123, 123, 107, 121, 107, 121,
        167, 151, 183, 140, 151, 183, 140, 140, 140, 154, 196, 196, 167, 154, 152, 167,
        182, 182, 134, 149, 136, 153, 121, 136, 137, 169, 194, 166, 167, 154, 167, 137,
        182, 107, 167, 91, 122, 107, 167, 154, 154, 154, 154, 154, 154, 154, 154, 154,
        154, 154, 154, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0,
    ],
    [
        153, 160, 107, 139, 126, 154, 197, 185, 201, 154, 154, 154, 134, 154, 139, 154,
        154, 183, 152, 139, 154, 137, 95, 79, 63, 31, 31, 153, 153, 153, 153, 169,
        198, 169, 198, 168, 79, 224, 167, 122, 153, 111, 149, 92, 167, 154, 154, 139,
        139, 139, 139, 139, 139, 125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125,
        126, 111, 111, 79, 108, 123, 93, 125, 110, 124, 110, 95, 94, 125, 111, 111,
        79, 125, 126, 111, 111, 79, 108, 123, 93, 121, 140, 61, 154, 170, 154, 139,
        153, 139, 123, 123, 63, 124, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136,
        153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 138, 138, 122, 121, 122, 121,
        167, 151, 183, 140, 151, 183, 140, 140, 140, 154, 196, 167, 167, 154, 152, 167,
        182, 182, 134, 149, 136, 153, 121, 136, 122, 169, 208, 166, 167, 154, 152, 167,
        182, 107, 167, 91, 107, 107, 167, 154, 154, 154, 154, 154, 154, 154, 154, 154,
        154, 154, 154, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0,
    ],
];

/// Offset of the first context of SAO_MERGE_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_MERGE_FLAG_OFFSET: usize = 0;

/// Offset of the first context of SAO_TYPE_IDX in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_TYPE_IDX_OFFSET: usize = 1;

/// Offset of the first context of SAO_EO_CLASS in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_EO_CLASS_OFFSET: usize = 2;

/// Offset of the first context of SAO_BAND_POSITION in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_BAND_POSITION_OFFSET: usize = 2;

/// Offset of the first context of SAO_OFFSET_ABS in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_OFFSET_ABS_OFFSET: usize = 2;

/// Offset of the first context of SAO_OFFSET_SIGN in INIT_VALUES[initType] (ctxIdx 0).
pub const SAO_OFFSET_SIGN_OFFSET: usize = 2;

/// Offset of the first context of END_OF_SLICE_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const END_OF_SLICE_FLAG_OFFSET: usize = 2;

/// Offset of the first context of SPLIT_CODING_UNIT_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SPLIT_CODING_UNIT_FLAG_OFFSET: usize = 2;

/// Offset of the first context of CU_TRANSQUANT_BYPASS_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const CU_TRANSQUANT_BYPASS_FLAG_OFFSET: usize = 5;

/// Offset of the first context of SKIP_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SKIP_FLAG_OFFSET: usize = 6;

/// Offset of the first context of CU_QP_DELTA in INIT_VALUES[initType] (ctxIdx 0).
pub const CU_QP_DELTA_OFFSET: usize = 9;

/// Offset of the first context of PRED_MODE_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const PRED_MODE_FLAG_OFFSET: usize = 12;

/// Offset of the first context of PART_MODE in INIT_VALUES[initType] (ctxIdx 0).
pub const PART_MODE_OFFSET: usize = 13;

/// Offset of the first context of PCM_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const PCM_FLAG_OFFSET: usize = 17;

/// Offset of the first context of PREV_INTRA_LUMA_PRED_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const PREV_INTRA_LUMA_PRED_FLAG_OFFSET: usize = 17;

/// Offset of the first context of MPM_IDX in INIT_VALUES[initType] (ctxIdx 0).
pub const MPM_IDX_OFFSET: usize = 18;

/// Offset of the first context of REM_INTRA_LUMA_PRED_MODE in INIT_VALUES[initType] (ctxIdx 0).
pub const REM_INTRA_LUMA_PRED_MODE_OFFSET: usize = 18;

/// Offset of the first context of INTRA_CHROMA_PRED_MODE in INIT_VALUES[initType] (ctxIdx 0).
pub const INTRA_CHROMA_PRED_MODE_OFFSET: usize = 18;

/// Offset of the first context of MERGE_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const MERGE_FLAG_OFFSET: usize = 20;

/// Offset of the first context of MERGE_IDX in INIT_VALUES[initType] (ctxIdx 0).
pub const MERGE_IDX_OFFSET: usize = 21;

/// Offset of the first context of INTER_PRED_IDC in INIT_VALUES[initType] (ctxIdx 0).
pub const INTER_PRED_IDC_OFFSET: usize = 22;

/// Offset of the first context of REF_IDX_L0 in INIT_VALUES[initType] (ctxIdx 0).
pub const REF_IDX_L0_OFFSET: usize = 27;

/// Offset of the first context of REF_IDX_L1 in INIT_VALUES[initType] (ctxIdx 0).
pub const REF_IDX_L1_OFFSET: usize = 29;

/// Offset of the first context of ABS_MVD_GREATER0_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const ABS_MVD_GREATER0_FLAG_OFFSET: usize = 31;

/// Offset of the first context of ABS_MVD_GREATER1_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const ABS_MVD_GREATER1_FLAG_OFFSET: usize = 33;

/// Offset of the first context of ABS_MVD_MINUS2 in INIT_VALUES[initType] (ctxIdx 0).
pub const ABS_MVD_MINUS2_OFFSET: usize = 35;

/// Offset of the first context of MVD_SIGN_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const MVD_SIGN_FLAG_OFFSET: usize = 35;

/// Offset of the first context of MVP_LX_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const MVP_LX_FLAG_OFFSET: usize = 35;

/// Offset of the first context of NO_RESIDUAL_DATA_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const NO_RESIDUAL_DATA_FLAG_OFFSET: usize = 36;

/// Offset of the first context of SPLIT_TRANSFORM_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SPLIT_TRANSFORM_FLAG_OFFSET: usize = 37;

/// Offset of the first context of CBF_LUMA in INIT_VALUES[initType] (ctxIdx 0).
pub const CBF_LUMA_OFFSET: usize = 40;

/// Offset of the first context of CBF_CB_CR in INIT_VALUES[initType] (ctxIdx 0).
pub const CBF_CB_CR_OFFSET: usize = 42;

/// Offset of the first context of TRANSFORM_SKIP_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const TRANSFORM_SKIP_FLAG_OFFSET: usize = 47;

/// Offset of the first context of EXPLICIT_RDPCM_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const EXPLICIT_RDPCM_FLAG_OFFSET: usize = 49;

/// Offset of the first context of EXPLICIT_RDPCM_DIR_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const EXPLICIT_RDPCM_DIR_FLAG_OFFSET: usize = 51;

/// Offset of the first context of LAST_SIGNIFICANT_COEFF_X_PREFIX in INIT_VALUES[initType] (ctxIdx 0).
pub const LAST_SIGNIFICANT_COEFF_X_PREFIX_OFFSET: usize = 53;

/// Offset of the first context of LAST_SIGNIFICANT_COEFF_Y_PREFIX in INIT_VALUES[initType] (ctxIdx 0).
pub const LAST_SIGNIFICANT_COEFF_Y_PREFIX_OFFSET: usize = 71;

/// Offset of the first context of LAST_SIGNIFICANT_COEFF_X_SUFFIX in INIT_VALUES[initType] (ctxIdx 0).
pub const LAST_SIGNIFICANT_COEFF_X_SUFFIX_OFFSET: usize = 89;

/// Offset of the first context of LAST_SIGNIFICANT_COEFF_Y_SUFFIX in INIT_VALUES[initType] (ctxIdx 0).
pub const LAST_SIGNIFICANT_COEFF_Y_SUFFIX_OFFSET: usize = 89;

/// Offset of the first context of SIGNIFICANT_COEFF_GROUP_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SIGNIFICANT_COEFF_GROUP_FLAG_OFFSET: usize = 89;

/// Offset of the first context of SIGNIFICANT_COEFF_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const SIGNIFICANT_COEFF_FLAG_OFFSET: usize = 93;

/// Offset of the first context of COEFF_ABS_LEVEL_GREATER1_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const COEFF_ABS_LEVEL_GREATER1_FLAG_OFFSET: usize = 137;

/// Offset of the first context of COEFF_ABS_LEVEL_GREATER2_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const COEFF_ABS_LEVEL_GREATER2_FLAG_OFFSET: usize = 161;

/// Offset of the first context of COEFF_ABS_LEVEL_REMAINING in INIT_VALUES[initType] (ctxIdx 0).
pub const COEFF_ABS_LEVEL_REMAINING_OFFSET: usize = 167;

/// Offset of the first context of COEFF_SIGN_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const COEFF_SIGN_FLAG_OFFSET: usize = 167;

/// Offset of the first context of LOG2_RES_SCALE_ABS in INIT_VALUES[initType] (ctxIdx 0).
pub const LOG2_RES_SCALE_ABS_OFFSET: usize = 167;

/// Offset of the first context of RES_SCALE_SIGN_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const RES_SCALE_SIGN_FLAG_OFFSET: usize = 175;

/// Offset of the first context of CU_CHROMA_QP_OFFSET_FLAG in INIT_VALUES[initType] (ctxIdx 0).
pub const CU_CHROMA_QP_OFFSET_FLAG_OFFSET: usize = 177;

/// Offset of the first context of CU_CHROMA_QP_OFFSET_IDX in INIT_VALUES[initType] (ctxIdx 0).
pub const CU_CHROMA_QP_OFFSET_IDX_OFFSET: usize = 178;

/// Number of contexts (NUM_BINS) for SAO_MERGE_FLAG.
pub const SAO_MERGE_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for SAO_TYPE_IDX.
pub const SAO_TYPE_IDX_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for SAO_EO_CLASS.
pub const SAO_EO_CLASS_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for SAO_BAND_POSITION.
pub const SAO_BAND_POSITION_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for SAO_OFFSET_ABS.
pub const SAO_OFFSET_ABS_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for SAO_OFFSET_SIGN.
pub const SAO_OFFSET_SIGN_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for END_OF_SLICE_FLAG.
pub const END_OF_SLICE_FLAG_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for SPLIT_CODING_UNIT_FLAG.
pub const SPLIT_CODING_UNIT_FLAG_COUNT: usize = 3;

/// Number of contexts (NUM_BINS) for CU_TRANSQUANT_BYPASS_FLAG.
pub const CU_TRANSQUANT_BYPASS_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for SKIP_FLAG.
pub const SKIP_FLAG_COUNT: usize = 3;

/// Number of contexts (NUM_BINS) for CU_QP_DELTA.
pub const CU_QP_DELTA_COUNT: usize = 3;

/// Number of contexts (NUM_BINS) for PRED_MODE_FLAG.
pub const PRED_MODE_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for PART_MODE.
pub const PART_MODE_COUNT: usize = 4;

/// Number of contexts (NUM_BINS) for PCM_FLAG.
pub const PCM_FLAG_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for PREV_INTRA_LUMA_PRED_FLAG.
pub const PREV_INTRA_LUMA_PRED_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for MPM_IDX.
pub const MPM_IDX_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for REM_INTRA_LUMA_PRED_MODE.
pub const REM_INTRA_LUMA_PRED_MODE_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for INTRA_CHROMA_PRED_MODE.
pub const INTRA_CHROMA_PRED_MODE_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for MERGE_FLAG.
pub const MERGE_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for MERGE_IDX.
pub const MERGE_IDX_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for INTER_PRED_IDC.
pub const INTER_PRED_IDC_COUNT: usize = 5;

/// Number of contexts (NUM_BINS) for REF_IDX_L0.
pub const REF_IDX_L0_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for REF_IDX_L1.
pub const REF_IDX_L1_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for ABS_MVD_GREATER0_FLAG.
pub const ABS_MVD_GREATER0_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for ABS_MVD_GREATER1_FLAG.
pub const ABS_MVD_GREATER1_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for ABS_MVD_MINUS2.
pub const ABS_MVD_MINUS2_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for MVD_SIGN_FLAG.
pub const MVD_SIGN_FLAG_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for MVP_LX_FLAG.
pub const MVP_LX_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for NO_RESIDUAL_DATA_FLAG.
pub const NO_RESIDUAL_DATA_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for SPLIT_TRANSFORM_FLAG.
pub const SPLIT_TRANSFORM_FLAG_COUNT: usize = 3;

/// Number of contexts (NUM_BINS) for CBF_LUMA.
pub const CBF_LUMA_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for CBF_CB_CR.
pub const CBF_CB_CR_COUNT: usize = 5;

/// Number of contexts (NUM_BINS) for TRANSFORM_SKIP_FLAG.
pub const TRANSFORM_SKIP_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for EXPLICIT_RDPCM_FLAG.
pub const EXPLICIT_RDPCM_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for EXPLICIT_RDPCM_DIR_FLAG.
pub const EXPLICIT_RDPCM_DIR_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for LAST_SIGNIFICANT_COEFF_X_PREFIX.
pub const LAST_SIGNIFICANT_COEFF_X_PREFIX_COUNT: usize = 18;

/// Number of contexts (NUM_BINS) for LAST_SIGNIFICANT_COEFF_Y_PREFIX.
pub const LAST_SIGNIFICANT_COEFF_Y_PREFIX_COUNT: usize = 18;

/// Number of contexts (NUM_BINS) for LAST_SIGNIFICANT_COEFF_X_SUFFIX.
pub const LAST_SIGNIFICANT_COEFF_X_SUFFIX_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for LAST_SIGNIFICANT_COEFF_Y_SUFFIX.
pub const LAST_SIGNIFICANT_COEFF_Y_SUFFIX_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for SIGNIFICANT_COEFF_GROUP_FLAG.
pub const SIGNIFICANT_COEFF_GROUP_FLAG_COUNT: usize = 4;

/// Number of contexts (NUM_BINS) for SIGNIFICANT_COEFF_FLAG.
pub const SIGNIFICANT_COEFF_FLAG_COUNT: usize = 44;

/// Number of contexts (NUM_BINS) for COEFF_ABS_LEVEL_GREATER1_FLAG.
pub const COEFF_ABS_LEVEL_GREATER1_FLAG_COUNT: usize = 24;

/// Number of contexts (NUM_BINS) for COEFF_ABS_LEVEL_GREATER2_FLAG.
pub const COEFF_ABS_LEVEL_GREATER2_FLAG_COUNT: usize = 6;

/// Number of contexts (NUM_BINS) for COEFF_ABS_LEVEL_REMAINING.
pub const COEFF_ABS_LEVEL_REMAINING_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for COEFF_SIGN_FLAG.
pub const COEFF_SIGN_FLAG_COUNT: usize = 0;

/// Number of contexts (NUM_BINS) for LOG2_RES_SCALE_ABS.
pub const LOG2_RES_SCALE_ABS_COUNT: usize = 8;

/// Number of contexts (NUM_BINS) for RES_SCALE_SIGN_FLAG.
pub const RES_SCALE_SIGN_FLAG_COUNT: usize = 2;

/// Number of contexts (NUM_BINS) for CU_CHROMA_QP_OFFSET_FLAG.
pub const CU_CHROMA_QP_OFFSET_FLAG_COUNT: usize = 1;

/// Number of contexts (NUM_BINS) for CU_CHROMA_QP_OFFSET_IDX.
pub const CU_CHROMA_QP_OFFSET_IDX_COUNT: usize = 1;

/// sig_coeff_flag ctxIdxMap per scan (0 diag,1 horiz,2 vert): 3 rows of 5 blocks x 16 (block0 = log2TrafoSize 2, block1..3 = prev_sig 0..2, block4 = transform-skip).
pub const CTX_IDX_MAP: [[u8; 80]; 3] = [
    [
        0, 2, 1, 6, 3, 4, 7, 6, 4, 5, 7, 8, 5, 8, 8, 8,
        1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        2, 1, 2, 0, 1, 2, 0, 0, 1, 2, 0, 0, 1, 0, 0, 0,
        2, 2, 1, 2, 1, 0, 2, 1, 0, 0, 1, 0, 0, 0, 0, 0,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    ],
    [
        0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8,
        1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
        2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0,
        2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    ],
    [
        0, 2, 6, 7, 1, 3, 6, 7, 4, 4, 8, 8, 5, 5, 8, 8,
        1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
        2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0,
        2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    ],
];

/// Horizontal 2x2 scan, x per scan position (C horiz_scan2x2_x).
pub const HORIZ_SCAN2X2_X: [u8; 4] = [0, 1, 0, 1];

/// Horizontal 2x2 scan, y per scan position (C horiz_scan2x2_y).
pub const HORIZ_SCAN2X2_Y: [u8; 4] = [0, 0, 1, 1];

/// Horizontal 4x4 scan, x per scan position (C horiz_scan4x4_x).
pub const HORIZ_SCAN4X4_X: [u8; 16] = [0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3];

/// Horizontal 4x4 scan, y per scan position (C horiz_scan4x4_y).
pub const HORIZ_SCAN4X4_Y: [u8; 16] = [0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];

/// Horizontal 8x8 inverse scan [y][x] to scan position (C horiz_scan8x8_inv).
pub const HORIZ_SCAN8X8_INV: [[u8; 8]; 8] = [
    [0, 1, 2, 3, 16, 17, 18, 19],
    [4, 5, 6, 7, 20, 21, 22, 23],
    [8, 9, 10, 11, 24, 25, 26, 27],
    [12, 13, 14, 15, 28, 29, 30, 31],
    [32, 33, 34, 35, 48, 49, 50, 51],
    [36, 37, 38, 39, 52, 53, 54, 55],
    [40, 41, 42, 43, 56, 57, 58, 59],
    [44, 45, 46, 47, 60, 61, 62, 63],
];

/// Up-right diagonal 2x2 scan, x per scan position (C diag_scan2x2_x).
pub const DIAG_SCAN2X2_X: [u8; 4] = [0, 0, 1, 1];

/// Up-right diagonal 2x2 scan, y per scan position (C diag_scan2x2_y).
pub const DIAG_SCAN2X2_Y: [u8; 4] = [0, 1, 0, 1];

/// Up-right diagonal 2x2 inverse scan [y][x] to scan position (C diag_scan2x2_inv).
pub const DIAG_SCAN2X2_INV: [[u8; 2]; 2] = [
    [0, 2],
    [1, 3],
];

/// Up-right diagonal 4x4 inverse scan [y][x] to scan position (C diag_scan4x4_inv).
pub const DIAG_SCAN4X4_INV: [[u8; 4]; 4] = [
    [0, 2, 5, 9],
    [1, 4, 8, 12],
    [3, 7, 11, 14],
    [6, 10, 13, 15],
];

/// Up-right diagonal 8x8 inverse scan [y][x] to scan position (C diag_scan8x8_inv).
pub const DIAG_SCAN8X8_INV: [[u8; 8]; 8] = [
    [0, 2, 5, 9, 14, 20, 27, 35],
    [1, 4, 8, 13, 19, 26, 34, 42],
    [3, 7, 12, 18, 25, 33, 41, 48],
    [6, 11, 17, 24, 32, 40, 47, 53],
    [10, 16, 23, 31, 39, 46, 52, 57],
    [15, 22, 30, 38, 45, 51, 56, 60],
    [21, 29, 37, 44, 50, 55, 59, 62],
    [28, 36, 43, 49, 54, 58, 61, 63],
];

/// Up-right diagonal 4x4 scan, x per scan position (C ff_hevc_diag_scan4x4_x).
pub const DIAG_SCAN4X4_X: [u8; 16] = [0, 0, 1, 0, 1, 2, 0, 1, 2, 3, 1, 2, 3, 2, 3, 3];

/// Up-right diagonal 4x4 scan, y per scan position (C ff_hevc_diag_scan4x4_y).
pub const DIAG_SCAN4X4_Y: [u8; 16] = [0, 1, 0, 2, 1, 0, 3, 2, 1, 0, 3, 2, 1, 3, 2, 3];

/// Up-right diagonal 8x8 scan, x per scan position (C ff_hevc_diag_scan8x8_x).
pub const DIAG_SCAN8X8_X: [u8; 64] = [
    0, 0, 1, 0, 1, 2, 0, 1, 2, 3, 0, 1, 2, 3, 4, 0,
    1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 6, 0, 1, 2, 3,
    4, 5, 6, 7, 1, 2, 3, 4, 5, 6, 7, 2, 3, 4, 5, 6,
    7, 3, 4, 5, 6, 7, 4, 5, 6, 7, 5, 6, 7, 6, 7, 7,
];

/// Up-right diagonal 8x8 scan, y per scan position (C ff_hevc_diag_scan8x8_y).
pub const DIAG_SCAN8X8_Y: [u8; 64] = [
    0, 1, 0, 2, 1, 0, 3, 2, 1, 0, 4, 3, 2, 1, 0, 5,
    4, 3, 2, 1, 0, 6, 5, 4, 3, 2, 1, 0, 7, 6, 5, 4,
    3, 2, 1, 0, 7, 6, 5, 4, 3, 2, 1, 7, 6, 5, 4, 3,
    2, 7, 6, 5, 4, 3, 7, 6, 5, 4, 7, 6, 5, 7, 6, 7,
];

/// Default intra 8x8 scaling matrix, row by row (C default_scaling_list_intra).
pub const DEFAULT_SCALING_LIST_INTRA: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25,
    16, 16, 17, 18, 20, 22, 25, 29, 16, 16, 18, 21, 24, 27, 31, 36,
    17, 17, 20, 24, 30, 35, 41, 47, 18, 19, 22, 27, 35, 44, 54, 65,
    21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36, 47, 65, 88, 115,
];

/// Default inter 8x8 scaling matrix, row by row (C default_scaling_list_inter).
pub const DEFAULT_SCALING_LIST_INTER: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25,
    16, 16, 17, 18, 20, 24, 25, 28, 16, 17, 18, 20, 24, 25, 28, 33,
    17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25, 28, 33, 41, 54,
    20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33, 41, 54, 71, 91,
];

/// Deblocking tC table indexed by Q (C tctable[54]).
pub const TC_TABLE: [u8; 54] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13,
    14, 16, 18, 20, 22, 24,
];

/// Deblocking beta table indexed by Q (C betatable[52]).
pub const BETA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24,
    26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56,
    58, 60, 62, 64,
];

/// ChromaArrayType==1 QpC for qPi 30..43 (C qp_c[qPi - 30]); qPi<30 gives QpC=qPi, qPi>43 gives QpC=qPi-6.
pub const QP_C_TABLE: [u8; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];

/// The intra prediction mode of 4:2:2 chroma for each mode derived for it (Table 8-3; C tab_mode_idx).
pub const MODE_422: [u8; 35] = [0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24, 25, 25, 26, 27, 27, 28, 28, 29, 29, 30, 31];

/// 32x32 DCT matrix [coefficient row][sample col] (C transform[32][32]); NxN uses rows k*(32/N).
pub const TRANSFORM: [[i8; 32]; 32] = [
    [
        64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
        64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    ],
    [
        90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4,
        -4, -13, -22, -31, -38, -46, -54, -61, -67, -73, -78, -82, -85, -88, -90, -90,
    ],
    [
        90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90,
        -90, -87, -80, -70, -57, -43, -25, -9, 9, 25, 43, 57, 70, 80, 87, 90,
    ],
    [
        90, 82, 67, 46, 22, -4, -31, -54, -73, -85, -90, -88, -78, -61, -38, -13,
        13, 38, 61, 78, 88, 90, 85, 73, 54, 31, 4, -22, -46, -67, -82, -90,
    ],
    [
        89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89,
        89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89,
    ],
    [
        88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22,
        -22, -61, -85, -90, -73, -38, 4, 46, 78, 90, 82, 54, 13, -31, -67, -88,
    ],
    [
        87, 57, 9, -43, -80, -90, -70, -25, 25, 70, 90, 80, 43, -9, -57, -87,
        -87, -57, -9, 43, 80, 90, 70, 25, -25, -70, -90, -80, -43, 9, 57, 87,
    ],
    [
        85, 46, -13, -67, -90, -73, -22, 38, 82, 88, 54, -4, -61, -90, -78, -31,
        31, 78, 90, 61, 4, -54, -88, -82, -38, 22, 73, 90, 67, 13, -46, -85,
    ],
    [
        83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83,
        83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83,
    ],
    [
        82, 22, -54, -90, -61, 13, 78, 85, 31, -46, -90, -67, 4, 73, 88, 38,
        -38, -88, -73, -4, 67, 90, 46, -31, -85, -78, -13, 61, 90, 54, -22, -82,
    ],
    [
        80, 9, -70, -87, -25, 57, 90, 43, -43, -90, -57, 25, 87, 70, -9, -80,
        -80, -9, 70, 87, 25, -57, -90, -43, 43, 90, 57, -25, -87, -70, 9, 80,
    ],
    [
        78, -4, -82, -73, 13, 85, 67, -22, -88, -61, 31, 90, 54, -38, -90, -46,
        46, 90, 38, -54, -90, -31, 61, 88, 22, -67, -85, -13, 73, 82, 4, -78,
    ],
    [
        75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75,
        75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75,
    ],
    [
        73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54,
        -54, -85, 4, 88, 46, -61, -82, 13, 90, 38, -67, -78, 22, 90, 31, -73,
    ],
    [
        70, -43, -87, 9, 90, 25, -80, -57, 57, 80, -25, -90, -9, 87, 43, -70,
        -70, 43, 87, -9, -90, -25, 80, 57, -57, -80, 25, 90, 9, -87, -43, 70,
    ],
    [
        67, -54, -78, 38, 85, -22, -90, 4, 90, 13, -88, -31, 82, 46, -73, -61,
        61, 73, -46, -82, 31, 88, -13, -90, -4, 90, 22, -85, -38, 78, 54, -67,
    ],
    [
        64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
        64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
    ],
    [
        61, -73, -46, 82, 31, -88, -13, 90, -4, -90, 22, 85, -38, -78, 54, 67,
        -67, -54, 78, 38, -85, -22, 90, 4, -90, 13, 88, -31, -82, 46, 73, -61,
    ],
    [
        57, -80, -25, 90, -9, -87, 43, 70, -70, -43, 87, 9, -90, 25, 80, -57,
        -57, 80, 25, -90, 9, 87, -43, -70, 70, 43, -87, -9, 90, -25, -80, 57,
    ],
    [
        54, -85, -4, 88, -46, -61, 82, 13, -90, 38, 67, -78, -22, 90, -31, -73,
        73, 31, -90, 22, 78, -67, -38, 90, -13, -82, 61, 46, -88, 4, 85, -54,
    ],
    [
        50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50,
        50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50,
    ],
    [
        46, -90, 38, 54, -90, 31, 61, -88, 22, 67, -85, 13, 73, -82, 4, 78,
        -78, -4, 82, -73, -13, 85, -67, -22, 88, -61, -31, 90, -54, -38, 90, -46,
    ],
    [
        43, -90, 57, 25, -87, 70, 9, -80, 80, -9, -70, 87, -25, -57, 90, -43,
        -43, 90, -57, -25, 87, -70, -9, 80, -80, 9, 70, -87, 25, 57, -90, 43,
    ],
    [
        38, -88, 73, -4, -67, 90, -46, -31, 85, -78, 13, 61, -90, 54, 22, -82,
        82, -22, -54, 90, -61, -13, 78, -85, 31, 46, -90, 67, 4, -73, 88, -38,
    ],
    [
        36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36,
        36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36,
    ],
    [
        31, -78, 90, -61, 4, 54, -88, 82, -38, -22, 73, -90, 67, -13, -46, 85,
        -85, 46, 13, -67, 90, -73, 22, 38, -82, 88, -54, -4, 61, -90, 78, -31,
    ],
    [
        25, -70, 90, -80, 43, 9, -57, 87, -87, 57, -9, -43, 80, -90, 70, -25,
        -25, 70, -90, 80, -43, -9, 57, -87, 87, -57, 9, 43, -80, 90, -70, 25,
    ],
    [
        22, -61, 85, -90, 73, -38, -4, 46, -78, 90, -82, 54, -13, -31, 67, -88,
        88, -67, 31, 13, -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22,
    ],
    [
        18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18,
        18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18,
    ],
    [
        13, -38, 61, -78, 88, -90, 85, -73, 54, -31, 4, 22, -46, 67, -82, 90,
        -90, 82, -67, 46, -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13,
    ],
    [
        9, -25, 43, -57, 70, -80, 87, -90, 90, -87, 80, -70, 57, -43, 25, -9,
        -9, 25, -43, 57, -70, 80, -87, 90, -90, 87, -80, 70, -57, 43, -25, 9,
    ],
    [
        4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90,
        90, -90, 88, -85, 82, -78, 73, -67, 61, -54, 46, -38, 31, -22, 13, -4,
    ],
];

/// Luma 8-tap filters [frac 0..3][tap] (C ff_hevc_qpel_filters[4][16]: each 16-byte row is the 8 taps duplicated, first 8 kept; row 0 unused).
pub const QPEL_FILTERS: [[i8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [-1, 4, -10, 58, 17, -5, 1, 0],
    [-1, 4, -11, 40, 40, -11, 4, -1],
    [0, 1, -5, 17, 58, -10, 4, -1],
];

/// Chroma 4-tap filters [frac 0..7][tap] (C ff_hevc_epel_filters[8][4]; row 0 unused).
pub const EPEL_FILTERS: [[i8; 4]; 8] = [
    [0, 0, 0, 0],
    [-2, 58, 10, -2],
    [-4, 54, 16, -2],
    [-6, 46, 28, -4],
    [-4, 36, 36, -4],
    [-4, 28, 46, -6],
    [-2, 16, 54, -4],
    [-2, 10, 58, -2],
];

/// Intra angle indexed by mode - 2 for modes 2..34 (C intra_pred_angle[mode - 2]).
pub const INTRA_PRED_ANGLE: [i32; 33] = [
    32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26,
    -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26,
    32,
];

/// Intra inverse angle indexed by mode - 11 for modes 11..25 (C inv_angle[mode - 11]).
pub const INV_ANGLE: [i32; 15] = [-4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_table_shape() {
        assert_eq!(INIT_VALUES.len(), 3);
        assert_eq!(INIT_VALUES[0].len(), HEVC_CONTEXTS);
        assert_eq!(HEVC_CONTEXTS, 199);
        assert_eq!(SIGNIFICANT_COEFF_FLAG_COUNT, 44);
        assert_eq!(SAO_MERGE_FLAG_OFFSET, 0);
    }

    #[test]
    fn deblocking_tables() {
        assert_eq!(TC_TABLE.len(), 54);
        assert_eq!(BETA_TABLE.len(), 52);
        assert_eq!(TC_TABLE[0], 0);
        assert_eq!(TC_TABLE[53], 24);
        assert_eq!(BETA_TABLE[0], 0);
        assert_eq!(BETA_TABLE[51], 64);
        assert_eq!(QP_C_TABLE[0], 29);
        assert_eq!(QP_C_TABLE[13], 37);
    }

    #[test]
    fn transform_and_filters() {
        assert!(TRANSFORM[0].iter().all(|&c| c == 64));
        for row in QPEL_FILTERS.iter().skip(1) {
            assert_eq!(row.iter().map(|&c| c as i32).sum::<i32>(), 64);
        }
        for row in EPEL_FILTERS.iter().skip(1) {
            assert_eq!(row.iter().map(|&c| c as i32).sum::<i32>(), 64);
        }
    }

    #[test]
    fn scans_and_scaling_lists() {
        assert_eq!(DIAG_SCAN4X4_X.len(), 16);
        assert_eq!(DIAG_SCAN8X8_Y.len(), 64);
        assert_eq!(DEFAULT_SCALING_LIST_INTRA.len(), 64);
        assert_eq!(DEFAULT_SCALING_LIST_INTER.len(), 64);
        assert_eq!(DEFAULT_SCALING_LIST_INTRA[0], 16);
        assert_eq!(DEFAULT_SCALING_LIST_INTRA[63], 115);
        assert_eq!(DEFAULT_SCALING_LIST_INTER[63], 91);
    }

    #[test]
    fn intra_angles() {
        assert_eq!(INTRA_PRED_ANGLE.len(), 33);
        assert_eq!(INTRA_PRED_ANGLE[0], 32);
        assert_eq!(INTRA_PRED_ANGLE[32], 32);
        assert_eq!(INV_ANGLE.len(), 15);
        assert_eq!(INV_ANGLE[0], -4096);
    }
}
