use std::f64::consts::{FRAC_1_SQRT_2, PI};

use crate::Error;

/// Crossover frequencies that split each output channel into a low, a mid
/// and a high band (see [`Options::bands`](crate::Options::bands)).
///
/// Each crossover is a fourth-order Linkwitz-Riley filter, so a band falls by
/// 24 dB per octave past its edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Crossover {
    /// Edge between the low and the mid band, in hertz. Must be positive.
    pub low_hz: u32,
    /// Edge between the mid and the high band, in hertz. Must be above
    /// `low_hz`.
    pub high_hz: u32,
}

impl Crossover {
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.low_hz == 0 || self.low_hz >= self.high_hz {
            return Err(Error::InvalidOptions(
                "crossover needs 0 < low_hz < high_hz",
            ));
        }
        Ok(())
    }
}

impl Default for Crossover {
    /// 200 Hz and 2 kHz: bass and kick drums, then voices and most
    /// instruments, then cymbals and sibilants.
    fn default() -> Self {
        Crossover {
            low_hz: 200,
            high_hz: 2000,
        }
    }
}

/// Root mean square level of each band over one output point.
///
/// Levels use the scale of 16-bit samples whatever
/// [`Options::bits`](crate::Options::bits) is, and saturate at
/// `u16::MAX`. A band whose lower edge is at or above half the sample rate
/// is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bands {
    /// Level below [`Crossover::low_hz`].
    pub low: u16,
    /// Level between [`Crossover::low_hz`] and [`Crossover::high_hz`].
    pub mid: u16,
    /// Level above [`Crossover::high_hz`].
    pub high: u16,
}

/// Three-band filter bank for one output channel. State is a fixed number of
/// biquads, so memory does not grow with the input.
#[derive(Debug, Clone)]
pub(crate) struct BandFilter {
    low: [Biquad; 2],
    mid: [Biquad; 4],
    high: [Biquad; 2],
    sums: [f64; 3],
}

impl BandFilter {
    pub(crate) fn new(crossover: &Crossover, sample_rate: u32) -> Self {
        let low_pass = |hz| Biquad::low_pass(hz, sample_rate);
        let high_pass = |hz| Biquad::high_pass(hz, sample_rate);
        BandFilter {
            low: [low_pass(crossover.low_hz); 2],
            mid: [
                high_pass(crossover.low_hz),
                high_pass(crossover.low_hz),
                low_pass(crossover.high_hz),
                low_pass(crossover.high_hz),
            ],
            high: [high_pass(crossover.high_hz); 2],
            sums: [0.0; 3],
        }
    }

    pub(crate) fn push(&mut self, value: i16) {
        let x = f64::from(value);
        for (sum, stages) in
            self.sums
                .iter_mut()
                .zip([&mut self.low[..], &mut self.mid[..], &mut self.high[..]])
        {
            let y = stages.iter_mut().fold(x, |x, stage| stage.process(x));
            *sum += y * y;
        }
    }

    /// Returns the levels of the last `frames` pushes and starts a new point.
    pub(crate) fn take(&mut self, frames: u32) -> Bands {
        let frames = f64::from(frames.max(1));
        // Float-to-int casts saturate, and map NaN to 0.
        let [low, mid, high] = self.sums.map(|sum| (sum / frames).sqrt().round() as u16);
        self.sums = [0.0; 3];
        for stage in self
            .low
            .iter_mut()
            .chain(&mut self.mid)
            .chain(&mut self.high)
        {
            stage.flush_tiny();
        }
        Bands { low, mid, high }
    }
}

/// Butterworth biquad in transposed direct form II.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    const IDENTITY: Biquad = Biquad::new(1.0, 0.0, 0.0, 0.0, 0.0);
    const SILENT: Biquad = Biquad::new(0.0, 0.0, 0.0, 0.0, 0.0);

    const fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Biquad {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn low_pass(cutoff: u32, sample_rate: u32) -> Self {
        // At or above Nyquist the bilinear transform puts the poles on the
        // unit circle, and the band edge is past every frequency anyway.
        if u64::from(cutoff) * 2 >= u64::from(sample_rate) {
            return Biquad::IDENTITY;
        }
        let (half_sin, half_cos, alpha) = Biquad::prewarp(cutoff, sample_rate);
        // 2·sin²(w/2) instead of 1 - cos(w), which cancels at low cutoffs.
        let b = half_sin * half_sin;
        Biquad::normalized(b, 2.0 * b, b, half_cos, half_sin, alpha)
    }

    fn high_pass(cutoff: u32, sample_rate: u32) -> Self {
        if u64::from(cutoff) * 2 >= u64::from(sample_rate) {
            return Biquad::SILENT;
        }
        let (half_sin, half_cos, alpha) = Biquad::prewarp(cutoff, sample_rate);
        let b = half_cos * half_cos;
        Biquad::normalized(b, -2.0 * b, b, half_cos, half_sin, alpha)
    }

    fn prewarp(cutoff: u32, sample_rate: u32) -> (f64, f64, f64) {
        let w = 2.0 * PI * f64::from(cutoff) / f64::from(sample_rate);
        let alpha = w.sin() * FRAC_1_SQRT_2;
        let (half_sin, half_cos) = (w / 2.0).sin_cos();
        (half_sin, half_cos, alpha)
    }

    fn normalized(b0: f64, b1: f64, b2: f64, half_cos: f64, half_sin: f64, alpha: f64) -> Self {
        let a0 = 1.0 + alpha;
        // -2·cos(w) = -2·(cos²(w/2) - sin²(w/2))
        let a1 = -2.0 * (half_cos * half_cos - half_sin * half_sin);
        Biquad::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, (1.0 - alpha) / a0)
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Silence makes the state decay into subnormal numbers, which are slow
    /// on x86. Inputs are integers, so state this small cannot show.
    fn flush_tiny(&mut self) {
        const TINY: f64 = 1e-10;
        if self.z1.abs() < TINY {
            self.z1 = 0.0;
        }
        if self.z2.abs() < TINY {
            self.z2 = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain(biquad: Biquad, hz: f64, sample_rate: u32) -> f64 {
        let mut biquad = biquad;
        let n = sample_rate as usize;
        let mut sum = 0.0;
        for i in 0..2 * n {
            let x = (2.0 * PI * hz * i as f64 / f64::from(sample_rate)).sin();
            let y = biquad.process(x);
            if i >= n {
                sum += y * y;
            }
        }
        (2.0 * sum / n as f64).sqrt()
    }

    #[test]
    fn butterworth_is_3_db_down_at_cutoff() {
        for biquad in [
            Biquad::low_pass(1000, 48000),
            Biquad::high_pass(1000, 48000),
        ] {
            let gain = gain(biquad, 1000.0, 48000);
            assert!((gain - FRAC_1_SQRT_2).abs() < 1e-3, "gain {gain}");
        }
    }

    #[test]
    fn silence_flushes_state_to_zero() {
        let mut filter = BandFilter::new(&Crossover::default(), 48000);
        for i in 0..4800 {
            filter.push(if i % 3 == 0 { i16::MAX } else { i16::MIN });
        }
        filter.take(4800);
        for _ in 0..4800 {
            filter.push(0);
        }
        filter.take(4800);
        let stages = filter.low.iter().chain(&filter.mid).chain(&filter.high);
        for stage in stages {
            assert_eq!((stage.z1, stage.z2), (0.0, 0.0));
        }
    }

    #[test]
    fn low_cutoff_keeps_unit_dc_gain() {
        let mut biquad = Biquad::low_pass(1, 48000);
        let mut y = 0.0;
        for _ in 0..10 * 48000 {
            y = biquad.process(1.0);
        }
        assert!((y - 1.0).abs() < 1e-6, "DC gain {y}");
    }
}
