//! Biquad filters: Butterworth/Linkwitz–Riley crossovers and the BS.1770 K-weighting curve.

use std::f64::consts::PI;

/// A second-order IIR section (transposed direct form II), computed in f64.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// From coefficients normalised so that a0 = 1.
    pub const fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self { b0: b[0], b1: b[1], b2: b[2], a1: a[0], a2: a[1], z1: 0.0, z2: 0.0 }
    }

    /// Passes everything unchanged.
    pub const fn identity() -> Self {
        Self::new([1.0, 0.0, 0.0], [0.0, 0.0])
    }

    /// 2nd-order Butterworth low-pass (RBJ cookbook, Q = 1/√2).
    pub fn lowpass(cutoff: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::omega(cutoff, sample_rate);
        let a0 = 1.0 + alpha;
        Self::new(
            [(1.0 - cos) / 2.0 / a0, (1.0 - cos) / a0, (1.0 - cos) / 2.0 / a0],
            [-2.0 * cos / a0, (1.0 - alpha) / a0],
        )
    }

    /// 2nd-order Butterworth high-pass (RBJ cookbook, Q = 1/√2).
    pub fn highpass(cutoff: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::omega(cutoff, sample_rate);
        let a0 = 1.0 + alpha;
        Self::new(
            [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            [-2.0 * cos / a0, (1.0 - alpha) / a0],
        )
    }

    fn omega(cutoff: f64, sample_rate: f64) -> (f64, f64) {
        let w0 = 2.0 * PI * cutoff / sample_rate;
        (w0.cos(), w0.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2))
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    pub fn coefficients(&self) -> ([f64; 3], [f64; 2]) {
        ([self.b0, self.b1, self.b2], [self.a1, self.a2])
    }
}

/// Two biquads in series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cascade2(pub Biquad, pub Biquad);

impl Cascade2 {
    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        self.1.process(self.0.process(x))
    }

    pub fn reset(&mut self) {
        self.0.reset();
        self.1.reset();
    }

    /// 4th-order Linkwitz–Riley low-pass: two Butterworth sections. A cutoff at or above
    /// Nyquist becomes a pass-through.
    pub fn lr4_lowpass(cutoff: f64, sample_rate: f64) -> Self {
        if cutoff >= 0.49 * sample_rate {
            return Self(Biquad::identity(), Biquad::identity());
        }
        let section = Biquad::lowpass(cutoff, sample_rate);
        Self(section, section)
    }

    /// 4th-order Linkwitz–Riley high-pass.
    pub fn lr4_highpass(cutoff: f64, sample_rate: f64) -> Self {
        let section = Biquad::highpass(cutoff, sample_rate);
        Self(section, section)
    }

    /// The ITU-R BS.1770 K-weighting filter (high shelf, then high-pass) for any sample rate,
    /// using the analogue prototype parameters so it is not limited to 48 kHz.
    pub fn k_weighting(sample_rate: f64) -> Self {
        // Stage 1: high shelf modelling the head.
        let f0 = 1_681.974_450_955_533;
        let gain_db = 3.999_843_853_973_347;
        let q = 0.707_175_236_955_419_6;
        let k = (PI * f0 / sample_rate).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        let shelf = Biquad::new(
            [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        );
        // Stage 2: RLB high-pass.
        let f0 = 38.135_470_876_024_44;
        let q = 0.500_327_037_323_877_3;
        let k = (PI * f0 / sample_rate).tan();
        let a0 = 1.0 + k / q + k * k;
        let highpass = Biquad::new([1.0, -2.0, 1.0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
        Self(shelf, highpass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn k_weighting_matches_the_bs1770_table_at_48k() {
        let Cascade2(shelf, highpass) = Cascade2::k_weighting(48_000.0);
        let (b, a) = shelf.coefficients();
        let table_b = [1.535_124_859_586_97, -2.691_696_189_406_38, 1.198_392_810_852_85];
        let table_a = [-1.690_659_293_182_41, 0.732_480_774_215_85];
        for (x, y) in b.iter().zip(table_b).chain(a.iter().zip(table_a)) {
            assert!(close(*x, y, 1e-9), "{x} vs {y}");
        }
        let (b, a) = highpass.coefficients();
        assert_eq!(b, [1.0, -2.0, 1.0]);
        assert!(close(a[0], -1.990_047_454_833_98, 1e-9) && close(a[1], 0.990_072_250_366_21, 1e-9));
    }

    /// Steady-state gain of a filter for a sine at `freq`, in dB.
    fn gain_db(mut filter: impl FnMut(f64) -> f64, freq: f64, sample_rate: f64) -> f64 {
        let n = sample_rate as usize; // one second
        let (mut sum_in, mut sum_out) = (0.0, 0.0);
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f64 / sample_rate).sin();
            let y = filter(x);
            if i > n / 2 {
                sum_in += x * x;
                sum_out += y * y;
            }
        }
        10.0 * (sum_out / sum_in).log10()
    }

    #[test]
    fn crossovers_pass_and_reject() {
        let sr = 48_000.0;
        let mut lp = Cascade2::lr4_lowpass(500.0, sr);
        assert!(close(gain_db(|x| lp.process(x), 100.0, sr), 0.0, 0.1));
        let mut lp = Cascade2::lr4_lowpass(500.0, sr);
        assert!(close(gain_db(|x| lp.process(x), 500.0, sr), -6.02, 0.1), "LR4 is -6 dB at the cutoff");
        let mut lp = Cascade2::lr4_lowpass(500.0, sr);
        assert!(gain_db(|x| lp.process(x), 4000.0, sr) < -60.0);
        let mut hp = Cascade2::lr4_highpass(500.0, sr);
        assert!(gain_db(|x| hp.process(x), 60.0, sr) < -60.0);
        let mut through = Cascade2::lr4_lowpass(30_000.0, sr);
        assert!(close(gain_db(|x| through.process(x), 10_000.0, sr), 0.0, 1e-9));
    }

    #[test]
    fn k_weighting_shape() {
        let sr = 44_100.0;
        let mut k = Cascade2::k_weighting(sr);
        assert!(close(gain_db(|x| k.process(x), 997.0, sr), 0.69, 0.05), "about +0.7 dB at 1 kHz");
        let mut k = Cascade2::k_weighting(sr);
        assert!(close(gain_db(|x| k.process(x), 8000.0, sr), 4.0, 0.3), "high shelf about +4 dB");
        let mut k = Cascade2::k_weighting(sr);
        assert!(gain_db(|x| k.process(x), 20.0, sr) < -10.0, "RLB high-pass removes sub-bass");
    }
}
