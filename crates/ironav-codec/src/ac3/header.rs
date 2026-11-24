//! The AC-3 frame header: the sync information and the bit stream information (ATSC A/52 section
//! 5.3 and 5.4.1) and the E-AC-3 syncframe header (ETSI TS 102 366 annex E.1.2 and E.1.3), and the
//! sync word search. Translated from FFmpeg's `libavcodec/ac3_parser.c` (`ff_ac3_find_syncword`,
//! `ff_ac3_parse_header`).

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
    /// Whether this is an E-AC-3 frame (bit stream id 11 to 16).
    pub eac3: bool,
    /// The E-AC-3 stream type: 0 independent, 1 dependent, 2 converted from AC-3 (also what an AC-3
    /// frame is given, as FFmpeg does).
    pub frame_type: u8,
    /// The E-AC-3 substream id; 0 for AC-3.
    pub substream_id: u8,
    /// Audio blocks in the frame: 6 for AC-3, 1, 2, 3 or 6 for E-AC-3.
    pub num_blocks: usize,
}

impl Header {
    /// Samples per channel in the frame.
    pub fn samples(&self) -> usize {
        self.num_blocks * 256
    }

    /// Whether the decoder leaves the frame out: an E-AC-3 dependent stream or a substream above 0.
    pub fn skipped(&self) -> bool {
        self.eac3 && (self.frame_type == 1 || self.substream_id != 0)
    }
}

/// E-AC-3 blocks per frame, by numblkscod.
const EAC3_BLOCKS: [usize; 4] = [1, 2, 3, 6];

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
/// first audio block of an AC-3 frame; for E-AC-3 (bsid 11 to 16) it is left at the audio frame
/// element, except for a frame the decoder leaves out (`Header::skipped`), whose header is not read
/// past its first fields.
pub(super) fn parse(r: &mut BitReader) -> Result<Header, String> {
    if r.bits(16) != 0x0B77 {
        return Err("frame sync error".into());
    }
    let mut peek = BitReader::new(r.data());
    peek.pos = r.pos + 16 + 2 + 6; // the bit stream id is at bit 40 of both syntaxes
    let bsid = peek.bits(5);
    if bsid > 16 {
        return Err(format!("invalid bit stream id {bsid}"));
    }
    if bsid > 10 {
        return parse_eac3(r, bsid);
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
    Ok(Header { sr_code, sample_rate, sr_shift, bsid, acmod, lfe_on, channels, frame_size, eac3: false, frame_type: 2, substream_id: 0, num_blocks: 6 })
}

/// The E-AC-3 syncframe header (E.1.2.1) and bit stream information (E.1.2.2), after the sync word.
fn parse_eac3(r: &mut BitReader, bsid: u32) -> Result<Header, String> {
    let frame_type = r.bits(2) as u8;
    if frame_type == 3 {
        return Err("E-AC-3: reserved stream type".into());
    }
    let substream_id = r.bits(3) as u8;
    let frame_size = (r.bits(11) as usize + 1) * 2;
    if frame_size < 7 {
        return Err("E-AC-3: invalid frame size".into());
    }
    let sr_code = r.bits(2) as usize;
    let (sample_rate, sr_shift, num_blocks);
    if sr_code == 3 {
        let sr_code2 = r.bits(2) as usize;
        if sr_code2 == 3 {
            return Err("invalid sample rate code".into());
        }
        sample_rate = SAMPLE_RATES[sr_code2] / 2;
        sr_shift = 1;
        num_blocks = 6;
    } else {
        num_blocks = EAC3_BLOCKS[r.bits(2) as usize];
        sample_rate = SAMPLE_RATES[sr_code];
        sr_shift = 0;
    }
    let acmod = r.bits(3) as usize;
    let lfe_on = r.bit();
    let channels = CHANNELS[acmod] + lfe_on as usize;
    let hdr = Header { sr_code, sample_rate, sr_shift, bsid, acmod, lfe_on, channels, frame_size, eac3: true, frame_type, substream_id, num_blocks };
    if hdr.skipped() {
        return Ok(hdr);
    }
    r.skip(5); // bsid, already read

    // volume control: dialnorm and compr (dual mono has two sets)
    let sets = if acmod == 0 { 2 } else { 1 };
    for _ in 0..sets {
        r.skip(5);
        if r.bit() {
            r.skip(8);
        }
    }
    // mixing metadata
    if r.bit() {
        if acmod > 2 {
            r.skip(2); // preferred downmix mode
            if acmod & 1 != 0 {
                r.skip(6); // center mix levels
            }
            if acmod & 4 != 0 {
                r.skip(6); // surround mix levels
            }
        }
        if lfe_on && r.bit() {
            r.skip(5); // LFE mix level
        }
        if frame_type == 0 {
            for _ in 0..sets {
                if r.bit() {
                    r.skip(6); // program scale factor
                }
            }
            if r.bit() {
                r.skip(6); // external program scale factor
            }
            match r.bits(2) {
                1 => r.skip(5),
                2 => r.skip(12),
                3 => {
                    let n = (r.bits(5) as usize + 2) * 8;
                    r.skip(n);
                }
                _ => {}
            }
            if acmod < 2 {
                for _ in 0..sets {
                    if r.bit() {
                        r.skip(14); // pan information of a mono or dual mono source
                    }
                }
            }
            if r.bit() {
                for _ in 0..num_blocks {
                    if num_blocks == 1 || r.bit() {
                        r.skip(5); // mixing configuration
                    }
                }
            }
        }
    }
    // informational metadata
    if r.bit() {
        r.skip(3 + 2); // bsmod, copyright and original bit stream
        if acmod == 2 {
            r.skip(4); // Dolby surround and headphone modes
        }
        if acmod >= 6 {
            r.skip(2); // Dolby surround EX mode
        }
        for _ in 0..sets {
            if r.bit() {
                r.skip(8); // mix level, room type, A/D converter type
            }
        }
        if sr_code != 3 {
            r.skip(1); // source sample rate code
        }
    }
    if frame_type == 0 && num_blocks != 6 {
        r.skip(1); // converter synchronisation flag
    }
    if frame_type == 2 && (num_blocks == 6 || r.bit()) {
        r.skip(6); // the frame size code of the AC-3 stream this was converted from
    }
    // additional bit stream information
    if r.bit() {
        let n = r.bits(6) as usize + 1;
        r.skip(8 * n);
    }
    Ok(hdr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of an E-AC-3 header: sync, strmtyp, substreamid, frmsiz, fscod (and fscod2 or
    /// numblkscod), acmod, lfeon, bsid, then `rest` zero bits.
    fn eac3_header(strmtyp: u32, substream: u32, frmsiz: u32, fscod: u32, next: u32, acmod: u32, lfe: u32, bsid: u32, rest: usize) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        for (v, n) in [(0x0B77u32, 16), (strmtyp, 2), (substream, 3), (frmsiz, 11), (fscod, 2), (next, 2), (acmod, 3), (lfe, 1), (bsid, 5)] {
            for i in (0..n).rev() {
                bits.push(v >> i & 1 != 0);
            }
        }
        bits.resize(bits.len() + rest, false);
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i))).collect()
    }

    #[test]
    fn parses_an_eac3_header() {
        // 48 kHz, 6 blocks, 3/2 with LFE, 1000 words; the rest of the bit stream information is zeros
        let data = eac3_header(0, 0, 999, 0, 3, 7, 1, 16, 100);
        let h = parse(&mut BitReader::new(&data)).unwrap();
        assert_eq!((h.eac3, h.frame_type, h.substream_id, h.frame_size, h.sample_rate, h.num_blocks, h.channels, h.bsid), (true, 0, 0, 2000, 48000, 6, 6, 16));
        assert_eq!((h.acmod, h.lfe_on, h.samples(), h.skipped()), (7, true, 1536, false));
        // blocks per frame by numblkscod, and the sample rates
        for (code, blocks) in [(0, 1), (1, 2), (2, 3), (3, 6)] {
            let h = parse(&mut BitReader::new(&eac3_header(0, 0, 99, 1, code, 2, 0, 16, 100))).unwrap();
            assert_eq!((h.num_blocks, h.sample_rate, h.channels), (blocks, 44100, 2));
        }
        // fscod 3 gives the half rates, six blocks
        let h = parse(&mut BitReader::new(&eac3_header(0, 0, 99, 3, 1, 1, 0, 16, 100))).unwrap();
        assert_eq!((h.sr_code, h.sample_rate, h.num_blocks, h.channels), (3, 22050, 6, 1));
        assert!(parse(&mut BitReader::new(&eac3_header(0, 0, 99, 3, 3, 1, 0, 16, 100))).is_err());
        // dependent frames and other substreams are marked to be skipped, and their headers are not read on
        let h = parse(&mut BitReader::new(&eac3_header(1, 0, 99, 0, 3, 2, 0, 16, 0))).unwrap();
        assert!(h.skipped() && h.frame_type == 1);
        let h = parse(&mut BitReader::new(&eac3_header(0, 2, 99, 0, 3, 2, 0, 16, 0))).unwrap();
        assert!(h.skipped() && h.substream_id == 2);
        // the converted-from-AC-3 type is decoded
        assert!(!parse(&mut BitReader::new(&eac3_header(2, 0, 99, 0, 3, 2, 0, 16, 100))).unwrap().skipped());
    }

    #[test]
    fn refuses_bad_eac3_headers() {
        assert!(parse(&mut BitReader::new(&eac3_header(3, 0, 99, 0, 3, 2, 0, 16, 100))).is_err(), "the reserved stream type");
        assert!(parse(&mut BitReader::new(&eac3_header(0, 0, 2, 0, 3, 2, 0, 16, 100))).is_err(), "a frame of 6 bytes");
        assert!(parse(&mut BitReader::new(&eac3_header(0, 0, 99, 0, 3, 2, 0, 17, 100))).is_err(), "bit stream id 17");
        assert!(parse(&mut BitReader::new(&eac3_header(0, 0, 99, 0, 3, 2, 0, 31, 100))).is_err(), "bit stream id 31");
        assert!(parse(&mut BitReader::new(&[0x0B, 0x78, 0, 0, 0, 0, 0, 0])).is_err(), "no sync word");
    }

    #[test]
    fn finds_the_sync_word_after_junk() {
        let buf = [0x00, 0x00, 0x00, 0x0B, 0x77, 0x00];
        assert_eq!(find_syncword(&buf), Some(3));
        let swapped = [0x00, 0x00, 0x77, 0x0B, 0x00];
        assert_eq!(find_syncword(&swapped), Some(2));
        assert_eq!(find_syncword(&[0u8; 9]), None);
    }
}
