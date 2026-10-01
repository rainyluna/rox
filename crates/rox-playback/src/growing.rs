//! A waveform filled in as its audio arrives, over a length known ahead: a
//! download decoded while it comes in, or a stream too big to keep binned as
//! it plays. Every bin knows whether anything landed in it, so a strip can
//! draw what's known and a stand-in for the rest.
//!
//! Its bins aren't the cache's. A stored waveform is folded from a whole
//! decode ([`crate::engine::decode_peaks`]), and a stated length can be off
//! by a frame or a second, so this only ever draws.

use rox_library::peaks::{PeakBin, PeakLanes};

#[derive(Clone, Copy)]
struct Acc {
    lo: f32,
    hi: f32,
    square: f64,
    frames: u32,
}

impl Default for Acc {
    fn default() -> Self {
        Acc {
            lo: f32::MAX,
            hi: f32::MIN,
            square: 0.0,
            frames: 0,
        }
    }
}

pub struct Growing {
    total_frames: u64,
    /// The mono mix, then left and right, `bins` long each.
    lanes: [Vec<Acc>; 3],
    stereo: bool,
}

impl Growing {
    /// `total_frames` at the rate the frames will come in.
    pub fn new(bins: usize, total_frames: u64, stereo: bool) -> Growing {
        let bins = bins.max(1);

        Growing {
            total_frames: total_frames.max(1),
            lanes: std::array::from_fn(|_| vec![Acc::default(); bins]),
            stereo,
        }
    }

    /// Interleaved stereo frames, the first at `at` on the track's clock.
    /// Frames past the stated length land in the last bin.
    pub fn add(&mut self, at: u64, frames: &[f32]) {
        let bins = self.lanes[0].len() as u64;

        for (k, frame) in frames.as_chunks::<2>().0.iter().enumerate() {
            let bin = (((at + k as u64) * bins) / self.total_frames).min(bins - 1) as usize;
            let samples = [(frame[0] + frame[1]) * 0.5, frame[0], frame[1]];

            for (lane, sample) in self.lanes.iter_mut().zip(samples) {
                let acc = &mut lane[bin];
                acc.lo = acc.lo.min(sample);
                acc.hi = acc.hi.max(sample);
                acc.square += f64::from(sample) * f64::from(sample);
                acc.frames += 1;
            }
        }
    }

    /// Whether any bin has had audio.
    pub fn any(&self) -> bool {
        self.lanes[0].iter().any(|acc| acc.frames > 0)
    }

    /// The lanes as they stand, normalized over what's known the way a whole
    /// decode is, and which bins are known. An empty bin reads as silence.
    pub fn snapshot(&self) -> (PeakLanes, Vec<bool>) {
        let known = self.lanes[0].iter().map(|acc| acc.frames > 0).collect();

        let keep = if self.stereo { 3 } else { 1 };
        let mut lanes: PeakLanes = self.lanes[..keep]
            .iter()
            .map(|lane| {
                lane.iter()
                    .map(|acc| match acc.frames {
                        0 => PeakBin::default(),
                        n => PeakBin {
                            lo: acc.lo,
                            hi: acc.hi,
                            rms: (acc.square / f64::from(n)).sqrt() as f32,
                        },
                    })
                    .collect()
            })
            .collect();

        crate::engine::normalize_peaks(&mut lanes[..1]);
        crate::engine::normalize_peaks(&mut lanes[1..]);

        (lanes, known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_land_in_the_bins_their_position_names() {
        let mut growing = Growing::new(4, 400, true);
        growing.add(200, &[0.5, -0.5, 0.25, -0.25]);

        let (lanes, known) = growing.snapshot();
        assert_eq!(known, vec![false, false, true, false]);
        assert_eq!(lanes.len(), 3);
        assert_eq!(
            lanes[0][0],
            PeakBin::default(),
            "nothing heard reads silent"
        );
        assert!(lanes[1][2].hi > 0.0 && lanes[2][2].lo < 0.0);
    }

    #[test]
    fn frames_past_the_stated_length_keep_to_the_last_bin() {
        let mut growing = Growing::new(4, 100, false);
        growing.add(150, &[0.5, 0.5]);

        let (lanes, known) = growing.snapshot();
        assert_eq!(known, vec![false, false, false, true]);
        assert_eq!(lanes.len(), 1, "mono has no channel lanes");
    }
}
