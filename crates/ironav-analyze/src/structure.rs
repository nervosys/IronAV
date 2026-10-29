//! Packet timing and size, from compressed packets alone.

use crate::{round, Analyzer, Input, Observation};
use ironav_core::StreamInfo;
use serde_json::{json, Value};

/// No packet for this long after the last one ended is a gap. Video packets arrive out of
/// presentation order, so less than this is ordinary reordering.
const GAP: f64 = 1.0;
/// Timestamps going back by more than this are a discontinuity rather than reordering.
const BACKWARDS: f64 = 2.0;

pub struct Structure {
    tb: f64,
    /// The latest end time of any packet so far.
    end: Option<f64>,
    first: Option<f64>,
    packets: u64,
    bytes: u64,
    all_packets: u64,
    all_bytes: u64,
    gaps: u64,
    discontinuities: u64,
}

impl Structure {
    pub fn new(s: &StreamInfo) -> Self {
        Structure { tb: s.time_base.as_f64(), end: None, first: None, packets: 0, bytes: 0, all_packets: 0, all_bytes: 0, gaps: 0, discontinuities: 0 }
    }
}

impl Analyzer for Structure {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Packet { packet, t } = input else { return };
        let t_end = t + packet.duration as f64 * self.tb;
        self.first.get_or_insert(*t);
        match self.end {
            Some(end) if *t > end + GAP => {
                self.gaps += 1;
                out.push(Observation::new("timestamp_gap", "structure", end, json!({ "seconds": round(t - end, 3) })).until(*t));
                self.end = Some(t_end);
            }
            Some(end) if *t < end - BACKWARDS => {
                self.discontinuities += 1;
                out.push(Observation::new("discontinuity", "structure", *t, json!({ "seconds": round(end - t, 3) })));
                self.end = Some(t_end);
            }
            Some(end) => self.end = Some(end.max(t_end)),
            None => self.end = Some(t_end),
        }
        self.packets += 1;
        self.bytes += packet.data.len() as u64;
    }

    fn digest(&mut self, seconds: f64) -> Option<Value> {
        let kbps = if seconds > 0.0 { round(self.bytes as f64 * 8.0 / seconds / 1000.0, 1) } else { 0.0 };
        let v = json!({ "packets": self.packets, "kbit_s": kbps });
        self.all_packets += self.packets;
        self.all_bytes += self.bytes;
        (self.packets, self.bytes) = (0, 0);
        Some(v)
    }

    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        self.digest(0.0);
        let span = self.end.zip(self.first).map_or(0.0, |(e, f)| e - f);
        let kbps = if span > 0.0 { round(self.all_bytes as f64 * 8.0 / span / 1000.0, 1) } else { 0.0 };
        Some(json!({ "packets": self.all_packets, "bytes": self.all_bytes, "kbit_s": kbps, "gaps": self.gaps, "discontinuities": self.discontinuities }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{CodecId, Packet, Rational, StreamParams};

    #[test]
    fn a_hole_in_the_timestamps_is_a_gap_and_a_jump_back_a_discontinuity() {
        let s = StreamInfo {
            index: 0,
            codec: CodecId::Aac,
            time_base: Rational { num: 1, den: 1000 },
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 48000, channels: 2 },
        };
        let mut a = Structure::new(&s);
        let mut out = Vec::new();
        for ms in (0..1000).step_by(20).chain((4000..5000).step_by(20)).chain((0..500).step_by(20)) {
            let p = Packet { stream: 0, pts: ms, duration: 20, data: vec![0; 100] };
            a.observe(&Input::Packet { packet: &p, t: ms as f64 / 1000.0 }, &mut out);
        }
        let got: Vec<_> = out.iter().map(|o| (o.kind, o.t, o.t_end)).collect();
        assert_eq!(got, [("timestamp_gap", 1.0, Some(4.0)), ("discontinuity", 0.0, None)]);
        let sum = a.finish(0.5, &mut out).unwrap();
        assert_eq!(sum["packets"], 125);
    }
}
