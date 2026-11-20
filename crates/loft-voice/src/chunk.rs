//! Recordings longer than a model takes in one go are cut into pieces at
//! the quietest moment near each piece's end, so a cut rarely falls inside
//! a word.

use std::ops::Range;

/// Loudness is compared over windows this long (samples at 16 kHz: 50 ms).
const WINDOW: usize = 800;

/// Pieces of `pcm` no longer than `max` samples, covering all of it in
/// order. Each cut is at the quietest window of the last third of a piece.
pub(crate) fn pieces(pcm: &[f32], max: usize) -> Vec<Range<usize>> {
    let max = max.max(WINDOW * 4);
    let mut out = Vec::new();
    let mut start = 0;
    while pcm.len() - start > max {
        let end = start + max;
        let from = end - max / 3;
        let mut best = (f32::MAX, end);
        let mut at = from;
        while at + WINDOW <= end {
            let energy: f32 = pcm[at..at + WINDOW].iter().map(|s| s * s).sum();
            if energy < best.0 {
                best = (energy, at + WINDOW / 2);
            }
            at += WINDOW / 2;
        }
        out.push(start..best.1);
        start = best.1;
    }
    out.push(start..pcm.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_audio_is_one_piece() {
        assert_eq!(pieces(&[0.1; 1000], 16_000), vec![0..1000]);
    }

    #[test]
    fn long_audio_is_cut_in_its_quiet_gaps() {
        // Ten seconds of "speech" with a silent gap around 8.5 s.
        let mut pcm: Vec<f32> = (0..160_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.5)
            .collect();
        pcm[134_000..138_000].fill(0.0);
        let cut = pieces(&pcm, 144_000);
        assert_eq!(cut.len(), 2);
        assert!((134_000..138_000).contains(&cut[0].end), "{cut:?}");
        assert_eq!(cut[0].end, cut[1].start);
        assert_eq!(cut[1].end, pcm.len());
        assert!(cut.iter().all(|r| r.len() <= 144_000));
    }
}
