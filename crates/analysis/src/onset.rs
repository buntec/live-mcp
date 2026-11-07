//! Onsets: spectral flux of the mid signal at full rate, every ~5 ms.

use crate::spectrum::Stft;

/// Window length in seconds (~21 ms); the hop is a quarter of it.
const WINDOW_SECONDS: f64 = 0.0213;
/// Log-magnitude compression, as for chroma.
const COMPRESSION: f32 = 100.0;
/// Bins below this are ignored (rumble, DC).
const LOW_HZ: f64 = 30.0;

#[derive(Debug)]
pub struct OnsetMeter {
    stft: Stft,
    previous: Vec<f32>,
    first_bin: usize,
}

impl OnsetMeter {
    pub fn new(sample_rate: f64) -> Self {
        let size = ((sample_rate * WINDOW_SECONDS) as usize).next_power_of_two().max(256);
        let first_bin = ((LOW_HZ * size as f64 / sample_rate).ceil() as usize).max(1);
        Self { stft: Stft::new(size, size / 4), previous: vec![0.0; size / 2 + 1], first_bin }
    }

    /// Forgets the input: the next sound is an onset against silence.
    pub fn reset(&mut self) {
        self.stft.reset();
        self.previous.fill(0.0);
    }

    /// Roughly how many input samples an attack is old when its flux peaks: attacks register
    /// once they are inside the newest part of the window, well before they reach its centre.
    pub fn attack_delay(&self) -> usize {
        self.stft.size() / 4
    }

    /// Feeds one sample; every hop returns the flux: the mean rise in log magnitude per bin
    /// since the previous frame (0 for steady sound, a few units for a hard attack).
    #[inline]
    pub fn push(&mut self, x: f32) -> Option<f32> {
        if !self.stft.push(x) {
            return None;
        }
        let power = self.stft.power();
        let mut rise = 0.0f32;
        for (p, previous) in power.iter().zip(self.previous.iter_mut()).skip(self.first_bin) {
            let level = (1.0 + COMPRESSION * p.sqrt()).ln();
            rise += (level - *previous).max(0.0);
            *previous = level;
        }
        Some(rise / (power.len() - self.first_bin) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_stand_out_and_steady_tones_do_not() {
        let rate = 48_000.0;
        let mut meter = OnsetMeter::new(rate);
        let mut fluxes = Vec::new();
        for n in 0..rate as usize {
            let t = n as f64 / rate;
            let tone = 0.3 * (2.0 * std::f64::consts::PI * 220.0 * t).sin();
            // A decaying noise burst every 250 ms, from 0.5 s on.
            let since = t % 0.25;
            let burst = if t >= 0.5 && since < 0.02 { ((n * 7919) % 1000) as f64 / 1000.0 - 0.5 } else { 0.0 };
            if let Some(flux) = meter.push((tone + burst) as f32) {
                fluxes.push((t, flux));
            }
        }
        let steady = fluxes.iter().filter(|(t, _)| *t > 0.1 && *t < 0.5).map(|(_, f)| *f).fold(0.0, f32::max);
        let attacks = fluxes.iter().filter(|(t, _)| *t >= 0.5).map(|(_, f)| *f).fold(0.0, f32::max);
        assert!(attacks > 10.0 * steady.max(1e-3), "attacks {attacks}, steady {steady}");
    }
}
