//! Sample-rate conversion for the microphone: a windowed-sinc low-pass
//! evaluated at each output sample's position in the input, so any pair of
//! rates works (48 kHz → 16 kHz, 44.1 kHz → 16 kHz, 8 kHz → 16 kHz, …).
//!
//! The filter passes up to 81.25 % of the narrower rate's Nyquist frequency
//! (6.5 kHz for 16 kHz output) and stops from 93.75 % (7.5 kHz), at least
//! 70 dB down, so nothing above the output's Nyquist frequency folds back
//! into the speech band. A Kaiser window shapes it; the number of taps
//! follows from that transition width at the input rate.
//!
//! The ratio is reduced to `out/in = L/M`, so output sample `n` sits at
//! input position `n·M/L`: its whole part says which input samples it
//! needs, its fraction (one of `L` phases) which taps. With few phases (the
//! usual rates: 48 k → 1, 44.1 k → 160) every phase's taps are worked out
//! once; otherwise they are worked out per sample.

/// Stopband attenuation the Kaiser window is designed for, in dB.
const ATTENUATION_DB: f64 = 70.0;
/// Edge of the passband, as a fraction of the narrower rate.
const PASS: f64 = 0.40625;
/// Width of the transition band, as a fraction of the narrower rate.
const TRANSITION: f64 = 0.0625;
/// Above this many phases the taps are computed per output sample.
const MAX_TABLE_PHASES: u64 = 512;

pub(crate) struct Resampler {
    /// `None` when both rates are the same: samples pass through untouched.
    sinc: Option<Sinc>,
}

struct Sinc {
    /// Output samples per `m` input samples (reduced).
    l: u64,
    m: u64,
    /// Taps on each side of an output sample's position.
    half: usize,
    /// The low-pass cutoff, as a fraction of the input rate × 2.
    cutoff: f64,
    beta: f64,
    /// `l` rows of `2·half` taps, when `l` is small enough.
    table: Option<Vec<f32>>,
    /// Input samples still needed; `history[0]` is input sample `base`.
    history: Vec<f32>,
    base: i64,
    /// Output samples made, input samples received.
    produced: u64,
    received: u64,
    taps: Vec<f32>,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32) -> Self {
        let (from, to) = (from.max(1), to.max(1));
        if from == to {
            return Self { sinc: None };
        }
        let g = gcd(u64::from(from), u64::from(to));
        let (l, m) = (u64::from(to) / g, u64::from(from) / g);
        let narrow = f64::from(from.min(to));
        let fc = PASS * narrow + TRANSITION * narrow / 2.;
        let transition = TRANSITION * narrow / f64::from(from);
        // Kaiser's estimates for the window's shape and length.
        let beta = 0.1102 * (ATTENUATION_DB - 8.7);
        let taps = ((ATTENUATION_DB - 7.95) / (2.285 * std::f64::consts::TAU * transition)).ceil();
        let half = (taps as usize).div_ceil(2) + 1;
        let mut sinc = Sinc {
            l,
            m,
            half,
            cutoff: 2. * fc / f64::from(from),
            beta,
            table: None,
            // Before the first sample the signal is silence.
            history: vec![0.; half - 1],
            base: -(half as i64 - 1),
            produced: 0,
            received: 0,
            taps: vec![0.; 2 * half],
        };
        if l <= MAX_TABLE_PHASES {
            let mut table = vec![0.; l as usize * 2 * half];
            for (phase, row) in table.chunks_mut(2 * half).enumerate() {
                sinc.fill_taps(phase as u64, row);
            }
            sinc.table = Some(table);
        }
        Self { sinc: Some(sinc) }
    }

    /// Converts `input` (one channel) and appends what it can to `out`.
    /// Output lags input by about `half` input samples until [`Self::flush`].
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        match &mut self.sinc {
            None => out.extend_from_slice(input),
            Some(sinc) => {
                sinc.history.extend_from_slice(input);
                sinc.received += input.len() as u64;
                sinc.run(u64::MAX, out);
            }
        }
    }

    /// The end of the input: appends the last samples, so that `n` input
    /// samples made `ceil(n·to/from)` output samples in all.
    pub(crate) fn flush(&mut self, out: &mut Vec<f32>) {
        let Some(sinc) = &mut self.sinc else { return };
        let total = (sinc.received * sinc.l).div_ceil(sinc.m);
        sinc.history.extend(std::iter::repeat_n(0., sinc.half + 1));
        sinc.run(total, out);
    }
}

impl Sinc {
    fn run(&mut self, until: u64, out: &mut Vec<f32>) {
        let half = self.half as i64;
        while self.produced < until {
            let position = self.produced * self.m;
            let whole = (position / self.l) as i64;
            let phase = position % self.l;
            if whole + half >= self.base + self.history.len() as i64 {
                break;
            }
            let start = (whole - half + 1 - self.base) as usize;
            let input = &self.history[start..start + 2 * self.half];
            let taps: &[f32] = match &self.table {
                Some(table) => {
                    let row = phase as usize * 2 * self.half;
                    &table[row..row + 2 * self.half]
                }
                None => {
                    let mut taps = std::mem::take(&mut self.taps);
                    self.fill_taps(phase, &mut taps);
                    self.taps = taps;
                    &self.taps
                }
            };
            out.push(input.iter().zip(taps).map(|(x, h)| x * h).sum());
            self.produced += 1;
        }
        // Drop what no later output sample reads.
        let next = (self.produced * self.m / self.l) as i64;
        let unused = (next - half + 1 - self.base).clamp(0, self.history.len() as i64);
        if unused > 0 {
            self.history.drain(..unused as usize);
            self.base += unused;
        }
    }

    /// The taps for an output sample `phase/l` of the way past an input
    /// sample, normalized so that they add up to 1 (silence and steady
    /// levels come out unchanged).
    fn fill_taps(&self, phase: u64, taps: &mut [f32]) {
        let fraction = phase as f64 / self.l as f64;
        let half = self.half as f64;
        let values: Vec<f64> = (0..taps.len())
            .map(|j| {
                let t = (j as f64 - (half - 1.)) - fraction;
                sinc(self.cutoff * t) * kaiser(t / half, self.beta)
            })
            .collect();
        let sum: f64 = values.iter().sum();
        for (tap, value) in taps.iter_mut().zip(values) {
            *tap = (value / sum) as f32;
        }
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// The Kaiser window at `x` in -1..=1.
fn kaiser(x: f64, beta: f64) -> f64 {
    if x.abs() > 1. {
        return 0.;
    }
    bessel_i0(beta * (1. - x * x).sqrt()) / bessel_i0(beta)
}

/// The modified Bessel function of the first kind, order 0, by its series.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1., 1.);
    let half = x / 2.;
    for k in 1..64 {
        let f = half / k as f64;
        term *= f * f;
        sum += term;
        if term < sum * 1e-14 {
            break;
        }
    }
    sum
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, seconds: f64, amplitude: f64) -> Vec<f32> {
        let n = (f64::from(rate) * seconds) as usize;
        (0..n)
            .map(|i| {
                (amplitude * (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin()) as f32
            })
            .collect()
    }

    fn convert(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        r.process(input, &mut out);
        r.flush(&mut out);
        out
    }

    /// RMS of the middle half (away from the filter's start and end).
    fn rms(samples: &[f32]) -> f64 {
        let middle = &samples[samples.len() / 4..samples.len() * 3 / 4];
        (middle.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / middle.len() as f64).sqrt()
    }

    /// How much of `hz` is in `samples`, as an amplitude (Goertzel-style
    /// correlation over the middle half).
    fn amplitude_at(samples: &[f32], rate: u32, hz: f64) -> f64 {
        let start = samples.len() / 4;
        let middle = &samples[start..samples.len() * 3 / 4];
        let (mut re, mut im) = (0., 0.);
        for (i, &x) in middle.iter().enumerate() {
            let phase = std::f64::consts::TAU * hz * (start + i) as f64 / f64::from(rate);
            re += f64::from(x) * phase.cos();
            im += f64::from(x) * phase.sin();
        }
        2. * (re * re + im * im).sqrt() / middle.len() as f64
    }

    #[test]
    fn speech_band_tones_survive() {
        for from in [48_000, 44_100, 96_000, 22_050, 8_000, 11_025, 32_000] {
            for hz in [300., 1_000., 3_000.] {
                let out = convert(from, 16_000, &tone(from, hz, 0.5, 0.5));
                let a = amplitude_at(&out, 16_000, hz);
                assert!((a - 0.5).abs() < 0.005, "{from} Hz in, {hz} Hz tone: {a}");
            }
        }
    }

    #[test]
    fn tones_above_the_band_are_removed() {
        for from in [48_000, 44_100, 96_000, 22_050] {
            for hz in [7_500., 8_000., 9_000., 12_000., 15_000.] {
                if hz >= f64::from(from) / 2. {
                    continue;
                }
                let out = convert(from, 16_000, &tone(from, hz, 0.5, 0.5));
                // 70 dB down from 0.5 is 0.00016; allow for float rounding.
                assert!(
                    rms(&out) < 0.001,
                    "{from} Hz in, {hz} Hz tone: {}",
                    rms(&out)
                );
            }
        }
    }

    #[test]
    fn lengths_follow_the_ratio() {
        for (from, n) in [
            (48_000, 48_000),
            (48_000, 1),
            (48_000, 7),
            (44_100, 4_410),
            (44_100, 1_001),
            (8_000, 333),
            (16_000, 999),
        ] {
            let out = convert(from, 16_000, &vec![0.25; n]);
            let expected = (n as u64 * 16_000).div_ceil(from as u64) as usize;
            assert_eq!(out.len(), expected, "{n} samples at {from} Hz");
        }
        assert!(convert(48_000, 16_000, &[]).is_empty());
    }

    #[test]
    fn steady_levels_come_out_unchanged() {
        let out = convert(44_100, 16_000, &vec![0.25; 44_100]);
        for &x in &out[100..out.len() - 100] {
            assert!((x - 0.25).abs() < 1e-4, "{x}");
        }
    }

    #[test]
    fn chunks_give_the_same_result_as_one_piece() {
        let input = tone(44_100, 440., 0.3, 0.8);
        let whole = convert(44_100, 16_000, &input);
        let mut r = Resampler::new(44_100, 16_000);
        let mut out = Vec::new();
        let mut rest = &input[..];
        let mut size = 1;
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(size.min(rest.len()));
            r.process(chunk, &mut out);
            rest = tail;
            size = size * 7 % 1_013 + 1;
        }
        r.flush(&mut out);
        assert_eq!(out.len(), whole.len());
        for (a, b) in out.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn rates_with_many_phases_work_without_a_table() {
        // 16000/44_053 doesn't reduce: 16 000 phases, taps per sample.
        let r = Resampler::new(44_053, 16_000);
        assert!(r.sinc.as_ref().unwrap().table.is_none());
        let out = convert(44_053, 16_000, &tone(44_053, 1_000., 0.3, 0.5));
        assert!((amplitude_at(&out, 16_000, 1_000.) - 0.5).abs() < 0.005);
        assert_eq!(out.len(), (13_215u64 * 16_000).div_ceil(44_053) as usize);
    }

    #[test]
    fn same_rate_passes_through() {
        let input = tone(16_000, 1_000., 0.1, 0.5);
        assert_eq!(convert(16_000, 16_000, &input), input);
    }
}
