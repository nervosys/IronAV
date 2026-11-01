//! Picture order of an H.264 or HEVC stream, from its slice headers alone: each picture's picture
//! order count (H.264 8.2.1, HEVC 8.3.1), whether it starts a coded video sequence (where the counts
//! begin again), and the reordering depth its sequence parameter set states. A demuxer uses these to
//! give pictures presentation times when the container has none.

use crate::h264::{self, ps as h264ps, Dpb};
use crate::hevc::ps as hevcps;
use ironav_core::{CodecId, Error, ErrorCode, Result};
use std::sync::Arc;

pub struct PictureOrder {
    /// The most pictures that can precede any picture in decoding order and follow it in output order.
    depth: usize,
    state: State,
}

enum State {
    H264 { dpb: Dpb, spss: Vec<Option<h264ps::Sps>>, ppss: Vec<Option<h264ps::Pps>> },
    Hevc { spss: Vec<Option<Arc<hevcps::Sps>>>, ppss: Vec<Option<Arc<hevcps::Pps>>>, first: bool, lsb: i64, msb: i64, last: Option<Box<hevcps::SliceHeader>> },
}

impl PictureOrder {
    /// The order reader for `codec`, which must be H.264 or HEVC.
    pub fn new(codec: CodecId) -> Option<Self> {
        let state = match codec {
            CodecId::H264 => State::H264 { dpb: Dpb::default(), spss: (0..32).map(|_| None).collect(), ppss: (0..256).map(|_| None).collect() },
            CodecId::Hevc => State::Hevc { spss: (0..16).map(|_| None).collect(), ppss: (0..64).map(|_| None).collect(), first: true, lsb: 0, msb: 0, last: None },
            _ => return None,
        };
        Some(PictureOrder { depth: 0, state })
    }

    /// How many pictures a presentation time can run ahead of decoding order by: the decoder's
    /// reordering depth as the latest sequence parameter set states it.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Stores a sequence or picture parameter set (`nal` with its header). Other NAL units are ignored,
    /// and a set that does not parse is left out (a slice that needs it then fails).
    pub fn parameter_set(&mut self, nal: &[u8]) {
        if nal.len() < 2 {
            return;
        }
        match &mut self.state {
            State::H264 { spss, .. } if nal[0] & 31 == 7 => {
                if let Ok(s) = h264ps::parse_sps(&h264::bits::unescape(&nal[1..])) {
                    self.depth = Dpb::reorder_depth(&s);
                    let id = s.id as usize;
                    if id < spss.len() {
                        spss[id] = Some(s);
                    }
                }
            }
            State::H264 { spss, ppss, .. } if nal[0] & 31 == 8 => {
                if let Ok(p) = h264ps::parse_pps(&h264::bits::unescape(&nal[1..]), &spss[..]) {
                    let id = p.id as usize;
                    if id < ppss.len() {
                        ppss[id] = Some(p);
                    }
                }
            }
            State::Hevc { spss, ppss, .. } if (nal[0] >> 1) & 63 == 33 => {
                if let Ok(s) = hevcps::parse_sps(&h264::bits::unescape(&nal[2..])) {
                    self.depth = s.max_num_reorder as usize;
                    let id = s.id;
                    if id < spss.len() {
                        spss[id] = Some(Arc::new(s));
                    }
                }
            }
            State::Hevc { ppss, .. } if (nal[0] >> 1) & 63 == 34 => {
                if let Ok(p) = hevcps::parse_pps(&h264::bits::unescape(&nal[2..])) {
                    let id = p.id;
                    if id < ppss.len() {
                        ppss[id] = Some(Arc::new(p));
                    }
                }
            }
            _ => {}
        }
    }

    /// A HEVC end of sequence: the next picture starts a sequence, as an IDR picture does.
    pub fn end_of_sequence(&mut self) {
        if let State::Hevc { first, .. } = &mut self.state {
            *first = true;
        }
    }

    /// The picture order count of the picture whose first slice is `nal` (with its header), and
    /// whether the picture starts a coded video sequence. `None` when the parameter sets the slice
    /// needs have not arrived yet. A picture that resets the counts with memory management operation 5
    /// is refused: its counts after the reset are not worked out here.
    pub fn picture(&mut self, nal: &[u8]) -> Result<Option<(i64, bool)>> {
        if nal.len() < 2 {
            return Err(Error::invalid_input("a slice NAL unit is too short"));
        }
        match &mut self.state {
            State::H264 { dpb, spss, ppss } => {
                let ty = nal[0] & 31;
                let ref_idc = (nal[0] >> 5) as u32 & 3;
                let rbsp = h264::bits::unescape(&nal[1..]);
                let Ok(hdr) = h264ps::parse_slice_header(&rbsp, ty, ref_idc, &spss[..], &ppss[..]) else {
                    return Ok(None);
                };
                let Some(pps) = ppss.get(hdr.pps_id as usize).and_then(|p| p.as_ref()) else { return Ok(None) };
                let Some(sps) = spss.get(pps.sps_id as usize).and_then(|s| s.as_ref()) else { return Ok(None) };
                if hdr.marking.ops.iter().any(|op| op.0 == 5) {
                    return Err(Error::new(ErrorCode::UnsupportedCodec, "H.264 picture order resets with memory management operation 5 are not read as a raw stream"));
                }
                // a frame's count is the lesser of its fields'
                let poc = dpb.compute_poc(sps, &hdr);
                Ok(Some((poc[0].min(poc[1]) as i64, hdr.idr)))
            }
            State::Hevc { spss, ppss, first, lsb, msb, last } => {
                let ty = (nal[0] >> 1) & 63;
                let tid = (nal[1] & 7).saturating_sub(1);
                let rbsp = h264::bits::unescape(&nal[2..]);
                let Ok(hdr) = hevcps::parse_slice_header(&rbsp, ty, tid, &spss[..], &ppss[..], last.as_deref()) else {
                    return Ok(None);
                };
                let Some(pps) = ppss.get(hdr.pps_id).and_then(|p| p.as_ref()) else { return Ok(None) };
                let Some(sps) = spss.get(pps.sps_id).and_then(|s| s.as_ref()) else { return Ok(None) };
                let max_lsb = 1i64 << sps.log2_max_poc_lsb;
                let irap = (16..=23).contains(&ty);
                // NoRaslOutputFlag: an IDR or BLA picture, or the first picture of the stream
                let no_rasl = (16..=20).contains(&ty) || (irap && *first);
                let boundary = no_rasl || *first;
                let cur = hdr.poc_lsb as i64;
                let msb_now = if no_rasl {
                    0
                } else if cur < *lsb && *lsb - cur >= max_lsb / 2 {
                    *msb + max_lsb
                } else if cur > *lsb && cur - *lsb > max_lsb / 2 {
                    *msb - max_lsb
                } else {
                    *msb
                };
                // prevTid0Pic is the last picture with TemporalId 0 that is not a RASL, RADL or sub-layer
                // non-reference picture
                let sub_layer_non_ref = ty <= 14 && ty.is_multiple_of(2);
                if tid == 0 && !(6..=9).contains(&ty) && !sub_layer_non_ref {
                    *lsb = cur;
                    *msb = msb_now;
                }
                *first = false;
                *last = Some(Box::new(hdr));
                Ok(Some((msb_now + cur, boundary)))
            }
        }
    }
}
