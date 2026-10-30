//! A playout (jitter) buffer: packets in as they arrive, one out per frame period.

use ironav_core::{Error, Result};

/// What to play for one frame period.
#[derive(Debug, PartialEq, Eq)]
pub enum Pop<'a> {
    /// The frame's packet: decode it.
    Packet(&'a [u8]),
    /// The frame's packet is missing but later ones have come: conceal it (the decoder's
    /// loss concealment, or silence).
    Missing,
    /// The buffer ran dry: conceal, or play silence. It fills to its target depth again before
    /// it gives packets, so a late burst does not run it dry at once again.
    Underrun,
    /// Filling to the target depth: play silence.
    Buffering,
}

/// Counters a link can report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayoutStats {
    /// Packets given out to be decoded.
    pub played: u64,
    /// Frame periods with no packet while later ones were waiting.
    pub missing: u64,
    /// Times the buffer ran dry while playing.
    pub underruns: u64,
    /// Packets that came after their time had passed.
    pub late: u64,
    /// Packets dropped because the buffer held more than its maximum depth (after a burst).
    pub dropped: u64,
    /// Packets that came twice.
    pub duplicates: u64,
}

/// Holds packets that arrive unevenly (a TCP link that stalls and then delivers a burst, or
/// UDP that reorders) and gives one per frame period, in sequence order, keeping about
/// `target_ms` of audio in hand. Packets are numbered by the sender with a wrapping `u32`;
/// a gap is reported as [`Pop::Missing`] when a later packet is already here.
///
/// When a burst leaves more than `max_ms` waiting, the oldest packets are dropped down to the
/// target, so the delay does not stay high. All storage is sized when it is made: no call
/// allocates.
pub struct Playout {
    frame_ms: f64,
    target: usize,
    max: usize,
    max_bytes: usize,
    /// `slots` packets of up to `max_bytes`, by sequence number modulo `slots`.
    data: Vec<u8>,
    len: Vec<usize>,
    seq: Vec<Option<u32>>,
    /// The sequence number to play next, once known.
    next: Option<u32>,
    /// The highest sequence number received.
    top: Option<u32>,
    filling: bool,
    stats: PlayoutStats,
}

/// `a - b` for wrapping sequence numbers.
fn diff(a: u32, b: u32) -> i64 {
    a.wrapping_sub(b) as i32 as i64
}

impl Playout {
    /// A buffer for packets of `frame_ms` of audio, up to `max_bytes` each, aiming to hold
    /// `target_ms` and never more than `max_ms`.
    pub fn new(frame_ms: f64, target_ms: f64, max_ms: f64, max_bytes: usize) -> Result<Self> {
        if !(frame_ms > 0.0) || !(target_ms >= 0.0) || !(max_ms >= target_ms) || max_bytes == 0 {
            return Err(Error::invalid_arg("a playout buffer needs a frame length above 0, a target of 0 or more, a maximum of at least the target, and room for a packet"));
        }
        let target = (target_ms / frame_ms).ceil() as usize;
        let max = ((max_ms / frame_ms).floor() as usize).max(target + 1);
        if max > 4096 {
            return Err(Error::invalid_arg(format!("{max_ms} ms is more than 4096 frames of {frame_ms} ms")));
        }
        // room for the maximum and a burst beyond it before the drop
        let slots = 2 * max + 2;
        Ok(Self {
            frame_ms,
            target,
            max,
            max_bytes,
            data: vec![0; slots * max_bytes],
            len: vec![0; slots],
            seq: vec![None; slots],
            next: None,
            top: None,
            filling: true,
            stats: PlayoutStats::default(),
        })
    }

    fn slots(&self) -> usize {
        self.seq.len()
    }

    /// Frames from the next to play through the highest received.
    fn span(&self) -> usize {
        match (self.next, self.top) {
            (Some(n), Some(t)) if diff(t, n) >= 0 => diff(t, n) as usize + 1,
            _ => 0,
        }
    }

    /// The audio waiting, in milliseconds: from the next frame to play through the newest
    /// received, gaps included. This is the delay the buffer adds now.
    pub fn depth_ms(&self) -> f64 {
        self.span() as f64 * self.frame_ms
    }

    /// The depth it fills to before playing, in milliseconds.
    pub fn target_ms(&self) -> f64 {
        self.target as f64 * self.frame_ms
    }

    pub fn stats(&self) -> PlayoutStats {
        self.stats
    }

    /// Adds packet `seq`. Late packets and duplicates are counted and ignored; one larger than
    /// `max_bytes` is refused.
    pub fn insert(&mut self, seq: u32, packet: &[u8]) -> Result<()> {
        if packet.len() > self.max_bytes {
            return Err(Error::invalid_arg(format!("a packet of {} bytes is larger than the {} the buffer was made for", packet.len(), self.max_bytes)));
        }
        let next = *self.next.get_or_insert(seq);
        if diff(seq, next) < 0 {
            self.stats.late += 1;
            return Ok(());
        }
        // far ahead (a sender that restarted, say): start again from it
        if diff(seq, next) as usize >= self.slots() {
            self.seq.fill(None);
            (self.next, self.top, self.filling) = (Some(seq), None, true);
        }
        let i = seq as usize % self.slots();
        if self.seq[i] == Some(seq) {
            self.stats.duplicates += 1;
            return Ok(());
        }
        self.seq[i] = Some(seq);
        self.len[i] = packet.len();
        self.data[i * self.max_bytes..i * self.max_bytes + packet.len()].copy_from_slice(packet);
        if self.top.is_none_or(|t| diff(seq, t) > 0) {
            self.top = Some(seq);
        }
        // after a burst, drop the oldest down to the target
        if self.span() > self.max {
            while self.span() > self.target.max(1) {
                let n = self.next.unwrap();
                let j = n as usize % self.slots();
                if self.seq[j] == Some(n) {
                    self.seq[j] = None;
                    self.stats.dropped += 1;
                }
                self.next = Some(n.wrapping_add(1));
            }
        }
        Ok(())
    }

    /// What to play for the next frame period: call once per period.
    pub fn pop(&mut self) -> Pop<'_> {
        let span = self.span();
        if self.filling {
            if span == 0 || span < self.target.max(1) {
                return Pop::Buffering;
            }
            self.filling = false;
        }
        let Some(n) = self.next.filter(|_| span > 0) else {
            self.stats.underruns += 1;
            self.filling = true;
            return Pop::Underrun;
        };
        self.next = Some(n.wrapping_add(1));
        let i = n as usize % self.slots();
        if self.seq[i] == Some(n) {
            self.seq[i] = None;
            self.stats.played += 1;
            Pop::Packet(&self.data[i * self.max_bytes..i * self.max_bytes + self.len[i]])
        } else {
            self.stats.missing += 1;
            Pop::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(seq: u32) -> [u8; 4] {
        seq.to_le_bytes()
    }

    fn got(p: Pop) -> Option<u32> {
        match p {
            Pop::Packet(b) => Some(u32::from_le_bytes(b.try_into().unwrap())),
            _ => None,
        }
    }

    #[test]
    fn packets_come_out_in_order_after_the_target_depth() {
        let mut b = Playout::new(10.0, 30.0, 100.0, 64).unwrap();
        assert_eq!(b.pop(), Pop::Buffering);
        for s in [5u32, 7, 6] {
            b.insert(s, &pk(s)).unwrap();
        }
        assert_eq!(b.depth_ms(), 30.0);
        assert_eq!(got(b.pop()), Some(5));
        assert_eq!(got(b.pop()), Some(6));
        // a late packet is ignored
        b.insert(5, &pk(5)).unwrap();
        assert_eq!(b.stats().late, 1);
        assert_eq!(got(b.pop()), Some(7));
        assert_eq!(b.pop(), Pop::Underrun);
        // then it fills again before playing
        b.insert(8, &pk(8)).unwrap();
        assert_eq!(b.pop(), Pop::Buffering);
        b.insert(9, &pk(9)).unwrap();
        b.insert(10, &pk(10)).unwrap();
        assert_eq!(got(b.pop()), Some(8));
    }

    #[test]
    fn a_gap_is_missing_and_a_burst_is_cut_back_to_the_target() {
        let mut b = Playout::new(10.0, 20.0, 60.0, 64).unwrap();
        for s in [0u32, 2] {
            b.insert(s, &pk(s)).unwrap();
        }
        assert_eq!(got(b.pop()), Some(0));
        assert_eq!(b.pop(), Pop::Missing);
        assert_eq!(got(b.pop()), Some(2));
        // a stall, then 10 packets at once: past the maximum (60 ms), the oldest go down to the
        // target, and the rest of the burst stacks on that
        for s in 3..13u32 {
            b.insert(s, &pk(s)).unwrap();
        }
        assert_eq!(b.stats().dropped, 5);
        assert_eq!(b.depth_ms(), 50.0);
        assert_eq!(got(b.pop()), Some(8));
        assert_eq!(got(b.pop()), Some(9));
        assert_eq!(b.stats(), PlayoutStats { played: 4, missing: 1, underruns: 0, late: 0, dropped: 5, duplicates: 0 });
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut b = Playout::new(20.0, 20.0, 100.0, 8).unwrap();
        for s in [u32::MAX - 1, u32::MAX, 0, 1] {
            b.insert(s, &pk(s)).unwrap();
        }
        let order: Vec<_> = (0..4).map(|_| got(b.pop())).collect();
        assert_eq!(order, [Some(u32::MAX - 1), Some(u32::MAX), Some(0), Some(1)]);
        b.insert(1, &pk(1)).unwrap();
        assert_eq!(b.stats().late, 1);
        assert!(b.insert(2, &[0; 9]).is_err());
    }
}
