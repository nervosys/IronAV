//! The AC-3 frame header: the sync information and the bit stream information (ATSC A/52 section
//! 5.3 and 5.4.1), and the sync word search. Translated from FFmpeg's `libavcodec/ac3_parser.c`
//! (`ff_ac3_find_syncword`, `ff_ac3_parse_header`).

use super::bits::BitReader;
use super::tables::FRAME_SIZE;

const SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];
/// Full-bandwidth channels for each audio coding mode (acmod).
const CHANNELS: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// The fields of a frame header this decoder needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Header {
    /// Index into the sample rate table (fscod).
    pub sr_code: usize,
    pub sample_rate: u32,
    /// Shift of the sample rate and the band tables for the reduced-rate bit stream ids 9 and 10.
    pub sr_shift: u32,
    pub bsid: u32,
    /// Audio coding mode (acmod), the channel mode.
    pub acmod: usize,
    pub lfe_on: bool,
    /// All channels, LFE included.
    pub channels: usize,
    /// Bytes in the frame.
    pub frame_size: usize,
}

/// The offset of the sync word in `buf`, searched as FFmpeg does: the word may be byte-swapped
/// (0x770B), so both byte orders are matched at odd positions.
pub(super) fn find_syncword(buf: &[u8]) -> Option<usize> {
    let n = buf.len();
    let mut i = 1;
    while i < n {
        if buf[i] == 0x77 || buf[i] == 0x0B {
            if (buf[i] ^ buf[i - 1]) == (0x77 ^ 0x0B) {
                i -= 1;
                break;
            } else if i + 1 < n && (buf[i] ^ buf[i + 1]) == (0x77 ^ 0x0B) {
                break;
            }
        }
        i += 2;
    }
    (i < n).then_some(i)
}

/// Parses the header at the start of the reader's data (the sync word). The reader is left at the
/// first audio block. An E-AC-3 frame (bsid above 10) is an error, as the decoder does not
/// implement it.
pub(super) fn parse(r: &mut BitReader) -> Result<Header, String> {
    if r.bits(16) != 0x0B77 {
        return Err("frame sync error".into());
    }
    let mut peek = BitReader::new(r.data());
    peek.pos = r.pos + 16 + 2 + 6;
    let bsid = peek.bits(5);
    if bsid > 10 {
        return Err(format!("bit stream id {bsid}: only AC-3 (bsid up to 10) is decoded, not E-AC-3"));
    }
    r.skip(16); // crc1, which is not checked
    let sr_code = r.bits(2) as usize;
    if sr_code == 3 {
        return Err("invalid sample rate code".into());
    }
    let frame_size_code = r.bits(6) as usize;
    if frame_size_code > 37 {
        return Err("invalid frame size code".into());
    }
    r.skip(5); // bsid, already read
    let _bsmod = r.bits(3);
    let acmod = r.bits(3) as usize;
    if acmod == 2 {
        r.skip(2); // dolby surround mode
    } else {
        if acmod & 1 != 0 && acmod != 1 {
            r.skip(2); // center mix level
        }
        if acmod & 4 != 0 {
            r.skip(2); // surround mix level
        }
    }
    let lfe_on = r.bit();
    let sr_shift = bsid.max(8) - 8;
    let sample_rate = SAMPLE_RATES[sr_code] >> sr_shift;
    let channels = CHANNELS[acmod] + lfe_on as usize;
    let frame_size = FRAME_SIZE[frame_size_code][sr_code] as usize * 2;

    // bit stream information
    let blocks = if acmod == 0 { 2 } else { 1 }; // dual mono carries two sets of volume controls
    for _ in 0..blocks {
        r.skip(5); // dialnorm
        if r.bit() {
            r.skip(8); // compr
        }
        if r.bit() {
            r.skip(8); // langcode
        }
        if r.bit() {
            r.skip(7); // audprodinfo
        }
    }
    r.skip(2); // copyright and original bit stream
    // timecodes; for bsid 6 these are the alternate bit stream syntax's mix levels and the
    // Dolby surround EX and headphone modes, which are 14 bits each as well
    for _ in 0..2 {
        if r.bit() {
            r.skip(14);
        }
    }
    if r.bit() {
        let n = r.bits(6) as usize;
        r.skip(8 * (n + 1)); // additional bit stream information
    }
    Ok(Header { sr_code, sample_rate, sr_shift, bsid, acmod, lfe_on, channels, frame_size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_sync_word_after_junk() {
        let buf = [0x00, 0x00, 0x00, 0x0B, 0x77, 0x00];
        assert_eq!(find_syncword(&buf), Some(3));
        let swapped = [0x00, 0x00, 0x77, 0x0B, 0x00];
        assert_eq!(find_syncword(&swapped), Some(2));
        assert_eq!(find_syncword(&[0u8; 9]), None);
    }
}
