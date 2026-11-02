//! Channel layouts shared by the Vorbis and Opus decoders: the order in which Vorbis channels fill
//! an output layout of 1 to 8 channels.

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
