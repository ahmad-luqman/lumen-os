//! Measurements shared by the tests.

use std::vec::Vec;

/// The frequency of a single periodic signal, from the times of its rising
/// zero crossings interpolated between samples: (crossings - 1) periods
/// between the first and last. Exact to a fraction of a sample at each end,
/// so over a second it is far inside 1%.
pub fn measured_hz(samples: &[i16], rate: u32) -> f64 {
    let times: Vec<f64> = samples
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0 && w[1] >= 0)
        .map(|(i, w)| {
            let (a, b) = (f64::from(w[0]), f64::from(w[1]));
            i as f64 + -a / (b - a)
        })
        .collect();
    assert!(times.len() >= 3, "too few crossings to measure: {}", times.len());
    (times.len() - 1) as f64 * f64::from(rate) / (times[times.len() - 1] - times[0])
}

/// Whether `got` is within 1% of `want`.
pub fn within_1_percent(got: f64, want: f64) -> bool {
    (got - want).abs() < want * 0.01
}

/// The largest magnitude in `samples`.
pub fn peak(samples: &[i16]) -> u16 {
    samples.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0)
}
