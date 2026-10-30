//! A streaming resampler: push input as it comes, pull the output that is ready.

use ironav_core::{Error, Result};

/// Converts interleaved samples from one rate to another with the `resample` filter's
/// windowed sinc, a few samples at a time. Its buffers are sized when it is made, so pushing
/// and pulling never allocate.
///
/// An output sample needs the input up to [`delay_ms`](Self::delay_ms) after its own instant,
/// so it comes out that much after the input that makes it: 1 ms for the default filter at
/// any rate (16 input samples at 16 kHz). Output starts at the input's first instant, with no
/// samples to drop, and [`flush`](Self::flush) gives the rest at the end of a stream.
pub struct StreamResampler {
    channels: usize,
    max_push: usize,
    l: usize,
    m: usize,
    half: usize,
    taps: usize,
    in_rate: u32,
    out_rate: u32,
    /// `l` rows of `taps` coefficients.
    table: Vec<f32>,
    /// Per channel: input from index `base` on, `len` samples of it.
    hist: Vec<Vec<f32>>,
    len: usize,
    base: i64,
    /// Index of the next output sample.
    next: i64,
    /// Input frames pushed.
    received: i64,
    /// Output frames still owed by `flush`, once it has run.
    end: Option<i64>,
}

impl StreamResampler {
    /// A resampler for `channels` interleaved channels that takes at most `max_push` frames
    /// (samples per channel) at a time.
    pub fn new(in_rate: u32, out_rate: u32, channels: usize, max_push: usize) -> Result<Self> {
        if in_rate == 0 || out_rate == 0 || !(1..=8).contains(&channels) || max_push == 0 {
            return Err(Error::invalid_arg("a resampler needs two rates above 0, 1 to 8 channels and room for some input"));
        }
        let (l, m, half, table) = crate::resample::design(in_rate, out_rate);
        if l * 2 * half > 1 << 20 {
            return Err(Error::invalid_arg(format!("{in_rate} to {out_rate} Hz needs too large a filter; convert through a rate with a simpler ratio")));
        }
        let taps = 2 * half;
        // the window's reach, a whole push (or the zeros that end the stream), and the lead-in
        let cap = 2 * taps + max_push + half;
        let mut hist = vec![Vec::with_capacity(cap); channels];
        // before the first sample the signal is zero
        for h in hist.iter_mut() {
            h.resize(cap, 0.0);
        }
        Ok(Self {
            channels,
            max_push,
            l,
            m,
            half,
            taps,
            in_rate,
            out_rate,
            table: table.iter().map(|&c| c as f32).collect(),
            hist,
            len: half - 1,
            base: -(half as i64 - 1),
            next: 0,
            received: 0,
            end: None,
        })
    }

    /// How long after its instant an output sample can be pulled: the filter's look-ahead.
    pub fn delay_ms(&self) -> f64 {
        self.half as f64 * 1000.0 / self.in_rate as f64
    }

    /// The look-ahead in input samples.
    pub fn delay_input_samples(&self) -> usize {
        self.half
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    /// Output frames that one pull could give now, at most.
    pub fn ready(&self) -> usize {
        let have = self.base + self.len as i64;
        // output j needs input up to floor(j * m / l) + half
        let mut j = self.next;
        let mut n = 0;
        while self.end.is_none_or(|e| j < e) && (j * self.m as i64) / self.l as i64 + (self.half as i64) < have {
            j += 1;
            n += 1;
        }
        n
    }

    /// Adds interleaved input: at most `max_push` frames, and no more than the buffer has room
    /// for until the output is pulled (pull after every push and it always has).
    pub fn push(&mut self, input: &[f32]) -> Result<()> {
        self.push_with(input.len(), |i| input[i])
    }

    /// [`push`](Self::push) for 16-bit samples.
    pub fn push_i16(&mut self, input: &[i16]) -> Result<()> {
        self.push_with(input.len(), |i| input[i] as f32 / 32768.0)
    }

    fn push_with(&mut self, n: usize, get: impl Fn(usize) -> f32) -> Result<()> {
        if self.end.is_some() {
            return Err(Error::invalid_arg("the resampler has been flushed"));
        }
        if n % self.channels != 0 {
            return Err(Error::invalid_arg(format!("{n} samples are not a whole number of {}-channel frames", self.channels)));
        }
        let frames = n / self.channels;
        if frames > self.max_push {
            return Err(Error::invalid_arg(format!("{frames} frames is more than the {} the resampler was made for", self.max_push)));
        }
        self.compact();
        if self.len + frames > self.hist[0].len() {
            return Err(Error::invalid_arg(format!("{frames} frames do not fit: pull the output first, or make the resampler with a larger max_push")));
        }
        for (c, h) in self.hist.iter_mut().enumerate() {
            for (k, v) in h[self.len..self.len + frames].iter_mut().enumerate() {
                *v = get(k * self.channels + c);
            }
        }
        self.len += frames;
        self.received += frames as i64;
        Ok(())
    }

    /// Moves the input no output still needs out of the buffer.
    fn compact(&mut self) {
        let keep_from = (self.next * self.m as i64) / self.l as i64 - self.half as i64 + 1;
        let drop = ((keep_from - self.base).max(0) as usize).min(self.len);
        if drop > 0 {
            for h in self.hist.iter_mut() {
                h.copy_within(drop..self.len, 0);
            }
            self.len -= drop;
            self.base += drop as i64;
        }
    }


    /// Writes the output that is ready, interleaved, to the start of `out`, as many whole frames
    /// as fit, and returns how many samples it wrote.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        self.pull_with(out.len(), |i, v| out[i] = v)
    }

    /// [`pull`](Self::pull) as 16-bit samples, rounded and clipped.
    pub fn pull_i16(&mut self, out: &mut [i16]) -> usize {
        self.pull_with(out.len(), |i, v| out[i] = (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
    }

    fn pull_with(&mut self, room: usize, mut put: impl FnMut(usize, f32)) -> usize {
        let have = self.base + self.len as i64;
        let mut w = 0;
        while w + self.channels <= room && self.end.is_none_or(|e| self.next < e) {
            let pos = self.next * self.m as i64;
            let (i0, p) = (pos / self.l as i64, (pos % self.l as i64) as usize);
            // the window covers input i0 - half + 1 ..= i0 + half; it must all have arrived
            if i0 + self.half as i64 >= have {
                break;
            }
            let first = (i0 - self.half as i64 + 1 - self.base) as usize;
            let row = &self.table[p * self.taps..(p + 1) * self.taps];
            for c in 0..self.channels {
                put(w + c, self.hist[c][first..first + self.taps].iter().zip(row).map(|(a, b)| a * b).sum());
            }
            w += self.channels;
            self.next += 1;
        }
        w
    }

    /// At the end of the stream: the output still owed, as [`pull`](Self::pull) gives it (as
    /// much as the input's length asks for). Call it until it returns 0; nothing can be pushed
    /// after it.
    pub fn flush(&mut self, out: &mut [f32]) -> usize {
        self.finish();
        self.pull(out)
    }

    /// [`flush`](Self::flush) as 16-bit samples.
    pub fn flush_i16(&mut self, out: &mut [i16]) -> usize {
        self.finish();
        self.pull_i16(out)
    }

    fn finish(&mut self) {
        if self.end.is_some() {
            return;
        }
        self.end = Some((self.received * self.l as i64 + self.m as i64 - 1) / self.m as i64);
        // the window reaches past the last sample, where the signal is zero
        self.compact();
        let zeros = self.taps.min(self.hist[0].len() - self.len);
        for h in self.hist.iter_mut() {
            h[self.len..self.len + zeros].fill(0.0);
        }
        self.len += zeros;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone(rate: u32, hz: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (0.5 * (2.0 * PI * hz * i as f64 / rate as f64).sin()) as f32).collect()
    }

    /// Pushes `x` in pieces of `piece` frames, pulling after each, then flushes.
    fn stream(r: &mut StreamResampler, x: &[f32], piece: usize) -> Vec<f32> {
        let ch = r.channels();
        let mut out = Vec::new();
        let mut buf = vec![0f32; 4096];
        for p in x.chunks(piece * ch) {
            r.push(p).unwrap();
            let n = r.pull(&mut buf);
            out.extend_from_slice(&buf[..n]);
        }
        loop {
            let n = r.flush(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn voice_rates_convert_a_tone_with_no_offset_and_the_same_as_the_filter() {
        for (a, b) in [(16_000u32, 48_000u32), (48_000, 16_000), (16_000, 24_000), (24_000, 16_000), (44_100, 48_000)] {
            let hz = 440.0;
            let x = tone(a, hz, a as usize / 4);
            // 20 ms pieces, and awkward ones
            for piece in [a as usize / 50, 7] {
                let mut r = StreamResampler::new(a, b, 1, piece).unwrap();
                assert_eq!(r.delay_ms(), r.delay_input_samples() as f64 * 1000.0 / a as f64);
                assert!(r.delay_ms() <= 1.0 + 1e-9, "{a}->{b}: {} ms", r.delay_ms());
                let out = stream(&mut r, &x, piece);
                assert_eq!(out.len(), (x.len() * b as usize).div_ceil(a as usize), "{a}->{b}");
                for (j, &v) in out.iter().enumerate().skip(100).take(out.len() - 200) {
                    let want = 0.5 * (2.0 * PI * hz * j as f64 / b as f64).sin();
                    assert!((v as f64 - want).abs() < 2e-3, "{a}->{b} piece {piece} sample {j}: {v} vs {want}");
                }
            }
        }
    }

    #[test]
    fn stereo_and_i16_keep_their_channels_apart() {
        let n = 1600;
        let x: Vec<i16> = (0..n).flat_map(|i| [(i as i16) * 10, -1000]).collect();
        let mut r = StreamResampler::new(16_000, 48_000, 2, 160).unwrap();
        let mut out = vec![0i16; 2 * 3 * 160 + 16];
        let mut all = Vec::new();
        for p in x.chunks(320) {
            r.push_i16(p).unwrap();
            let k = r.pull_i16(&mut out);
            assert_eq!(k % 2, 0);
            all.extend_from_slice(&out[..k]);
        }
        // the constant channel stays constant away from the start
        assert!(all.chunks(2).skip(100).all(|f| (f[1] + 1000).abs() <= 1), "right channel");
        // the ramp stays a ramp, at a third of its slope
        let left: Vec<i16> = all.chunks(2).map(|f| f[0]).collect();
        assert!(left.windows(2).skip(100).take(1000).all(|w| (w[1] - w[0] - 3).abs() <= 1), "left channel");
    }

    #[test]
    fn too_much_input_is_refused_until_output_is_pulled() {
        let mut r = StreamResampler::new(16_000, 48_000, 1, 320).unwrap();
        assert!(r.push(&[0.0; 321]).is_err());
        // without pulling, the buffer fills
        let mut pushed = 0;
        while r.push(&[0.0; 320]).is_ok() {
            pushed += 1;
            assert!(pushed < 10);
        }
        assert!(pushed >= 1 && r.ready() > 0);
        let mut out = vec![0f32; 3 * 320 * 10];
        assert_eq!(r.pull(&mut out), 3 * (320 * pushed - 16));
        r.push(&[0.0; 320]).unwrap();
        assert!(StreamResampler::new(16_000, 48_000, 2, 160).unwrap().push(&[0.0; 3]).is_err(), "half a frame");
    }
}
