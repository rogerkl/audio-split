use crate::audio::AudioData;

pub struct DetectParams {
    /// RMS level below which a window counts as silence, in dBFS.
    pub threshold_db: f32,
    /// Minimum length of a silent stretch to count as a gap, in seconds.
    pub min_gap_secs: f32,
    /// Minimum track length, in seconds (suppresses spurious splits).
    pub min_track_secs: f32,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            threshold_db: -40.0,
            min_gap_secs: 1.0,
            min_track_secs: 15.0,
        }
    }
}

/// Returns track start positions (sample frames), always including frame 0.
///
/// A track start is placed where the signal rises above the threshold again
/// after a sufficiently long silent stretch, backed off slightly into the
/// silence so the track attack is not clipped.
pub fn detect_track_starts(audio: &AudioData, params: &DetectParams) -> Vec<usize> {
    let sr = audio.sample_rate as usize;
    let win = (sr / 100).max(1); // 10 ms analysis windows
    let frames = audio.frames();
    let n_win = frames / win;
    let channels = audio.channels;

    let threshold = 10f32.powf(params.threshold_db / 20.0);
    let threshold_sq = threshold * threshold;

    let min_gap_windows = ((params.min_gap_secs * 100.0) as usize).max(1);
    let min_track_frames = (params.min_track_secs as usize) * sr;
    // Back off 50 ms into the silence so the attack of the track is kept.
    let backoff = sr / 20;

    let mut starts = vec![0usize];
    let mut quiet_run = 0usize;
    // Treat the very beginning as silence so a track starting after lead-in
    // silence still gets its marker moved off zero only if long enough.
    let mut last_start = 0usize;

    for w in 0..n_win {
        let begin = w * win;
        let end = begin + win;
        let mut sum_sq = 0f32;
        for frame in begin..end {
            for ch in 0..channels {
                let s = audio.samples[frame * channels + ch];
                sum_sq += s * s;
            }
        }
        let mean_sq = sum_sq / (win * channels) as f32;

        if mean_sq < threshold_sq {
            quiet_run += 1;
        } else {
            if quiet_run >= min_gap_windows {
                let candidate = (w * win).saturating_sub(backoff);
                if candidate > 0 && candidate.saturating_sub(last_start) >= min_track_frames {
                    starts.push(candidate);
                    last_start = candidate;
                }
            }
            quiet_run = 0;
        }
    }

    starts
}
