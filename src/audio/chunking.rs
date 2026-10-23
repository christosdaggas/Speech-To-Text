// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Cut policy for while-recording chunk decodes.
//!
//! While a take is recording, its audio is decoded in bounded chunks at
//! natural pauses so the final decode at stop stays small. Two decisions live
//! here, both as pure functions over raw samples and segment timings:
//!
//! - **Where to cut.** A pause is found with a silence threshold that adapts
//!   to the audio instead of a fixed RMS constant. A fixed threshold misses
//!   every pause as soon as room noise sits above it, and every cut degenerates
//!   into the forced one. The adaptive threshold is a small multiple of a noise
//!   floor tracked from the signal, where the floor is capped at a fraction of
//!   the peak-held speech level — without the cap, continuous speech has no
//!   true silence to pull the floor down, it converges on the quiet frames
//!   *inside* speech, and the threshold starts "finding" pauses mid-word.
//! - **What is safe to commit at a cut.** A cut inside verified silence can be
//!   retired at exactly the cut: no word can straddle it. Any other cut (forced
//!   at the maximum, or a pause merely under the threshold) may have sliced a
//!   word in half, so the trailing segments that can straddle the boundary are
//!   held back for the next decode instead of being committed mangled. The next
//!   decode re-covers that audio whole; the commit watermark stops where the
//!   held-back text begins, so committed text stays append-only and never
//!   duplicates.

/// VAD analysis frame (~30 ms): the granularity the adaptive levels are
/// computed at and the cut quietness is verified at.
pub const FRAME_SECS: f32 = 0.03;

/// Per-frame EMA weight for RMS smoothing (fast attack, slow release).
const SMOOTH_ALPHA: f32 = 0.3;

/// The tracked noise floor may never exceed this fraction of the peak-held
/// speech level. Dictation audio sits well above its own noise floor, so a
/// floor climbing higher is drifting into speech, not measuring ambience.
/// Capping too hard only costs pause cuts (the forced cut still bounds the
/// chunk, and its hold-back makes it safe); capping too little deletes words.
const FLOOR_CAP_RATIO: f32 = 0.05;

/// The silence threshold is this multiple of the (capped) noise floor — a
/// pause reads a little above ambient, and the margin keeps the pause window
/// from being eaten by ambient itself.
const FLOOR_MULT: f32 = 3.0;

/// Clamps for the silence threshold: absolute floor keeps a near-silent mic
/// from zeroing the threshold (a pause then still reads as one), the ceiling
/// keeps a loud ambient from swallowing speech-pause scale.
const MIN_SILENCE_THRESHOLD: f32 = 0.0024;
const MAX_SILENCE_THRESHOLD: f32 = 0.048;

/// A pause cut is only *verified quiet* — safe to retire at without holding
/// anything back — when the loudest frame inside the pause window measures at
/// most this fraction of the peak-held speech level. A pause in usable
/// dictation audio reads far below the speech level; a "pause" that loud is
/// more likely quiet speech or a noise dip, and the cut must be treated as
/// unverified.
const QUIET_FRAME_RATIO: f32 = 0.25;

/// Segments ending within this of the cut have insufficient right context:
/// the decoder may have dropped or mangled their last word at the window
/// edge. At an unverified cut they are the ones held back.
pub const TAIL_GUARD_SECS: f32 = 0.5;

#[derive(Debug, Clone, Copy)]
pub struct Levels {
    /// Peak-held smoothed RMS — how loud the speech in this region gets.
    pub speech_level: f32,
    /// Silence threshold frames must fall under to read as a pause.
    pub silence_threshold: f32,
}

/// A cut the policy decided on, with raw indices relative to the scanned
/// region slice.
#[derive(Debug, Clone, Copy)]
pub struct CutChoice {
    /// The cut itself: mid-window, frame-aligned. When `verified_quiet` this
    /// is exactly where the chunk may retire.
    pub cut: usize,
    /// The cut sits in verified silence; no word can straddle it.
    pub verified_quiet: bool,
}

pub struct Scan<'a> {
    /// Raw interleaved samples of the uncommitted region.
    pub region: &'a [f32],
    /// Source channels (interleaved samples per frame).
    pub channels: usize,
    /// Analysis frame length, in interleaved samples.
    pub frame_len: usize,
    /// Pause detection window, in interleaved samples.
    pub quiet_window: usize,
    /// Scan range (interleaved samples into `region`).
    pub scan_from: usize,
    pub scan_to: usize,
    /// The region reached its maximum length: cut at the quietest window even
    /// if nothing qualifies as a pause.
    pub force: bool,
}

/// Measure the adaptive levels over `samples`: a per-frame RMS smoothed with
/// [`SMOOTH_ALPHA`], the speech level as its peak, and the noise floor as a
/// low percentile (robust to a stray dropout or a lone loud frame).
pub fn analyze_levels(samples: &[f32], frame_len: usize) -> Levels {
    let mut smoothed: Vec<f32> = Vec::new();
    let mut acc = 0.0f32;
    for frame in samples.chunks_exact(frame_len.max(1)) {
        let rms = rms(frame);
        acc = SMOOTH_ALPHA * rms + (1.0 - SMOOTH_ALPHA) * acc;
        smoothed.push(acc);
    }
    if smoothed.is_empty() {
        return Levels {
            speech_level: 0.0,
            silence_threshold: MIN_SILENCE_THRESHOLD,
        };
    }
    let speech_level = smoothed.iter().copied().fold(0.0f32, f32::max);
    let mut sorted = smoothed.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[(sorted.len() / 10).min(sorted.len() - 1)];
    let floor = floor.min(FLOOR_CAP_RATIO * speech_level);
    let silence_threshold =
        (floor * FLOOR_MULT).clamp(MIN_SILENCE_THRESHOLD, MAX_SILENCE_THRESHOLD);
    Levels {
        speech_level,
        silence_threshold,
    }
}

/// Walk the scan range in quiet-window steps and pick the cut: the first
/// window under the silence threshold wins (earliest cut, freshest chunks);
/// without one, the quietest window is used only when `force` is set.
/// `None` means keep buffering.
pub fn choose_cut(scan: Scan<'_>, levels: &Levels) -> Option<CutChoice> {
    let Scan {
        region,
        channels,
        frame_len,
        quiet_window,
        scan_from,
        scan_to,
        force,
    } = scan;
    if scan_from + quiet_window > scan_to {
        return None;
    }

    let mut best: Option<(usize, f32)> = None; // (window start, rms)
    let mut pos = scan_from;
    while pos + quiet_window <= scan_to {
        let window = &region[pos..pos + quiet_window];
        let rms = rms(window);
        if rms < levels.silence_threshold {
            // Verified quiet only when even the loudest frame of the window
            // reads far below speech — then no word can straddle the cut.
            let peak_frame = frame_rms(window, frame_len);
            let verified = peak_frame <= levels.speech_level * QUIET_FRAME_RATIO;
            return Some(CutChoice {
                cut: mid_cut(pos, quiet_window, channels),
                verified_quiet: verified,
            });
        }
        match best {
            Some((_, b)) if rms >= b => {}
            _ => best = Some((pos, rms)),
        }
        pos += quiet_window;
    }

    if !force {
        return None;
    }
    let (window_start, _) = best?;
    Some(CutChoice {
        cut: mid_cut(window_start, quiet_window, channels),
        // Forced cuts (and cuts at the quietest window of a maxed region) are
        // never verified: the window was merely the least loud in range.
        verified_quiet: false,
    })
}

/// Split a decoded chunk's segments at a risky cut's hold-back boundary.
/// Returns the segments safe to commit plus the snapshot-relative watermark
/// (ms): everything from there was decoded but is NOT committed here, and the
/// next decode re-covers it whole.
///
/// `hold_back` is false for a verified-quiet cut (nothing can straddle it) and
/// for decoders whose segments carry no real timings (their whole output is
/// committed as-is, as before).
///
/// Hold the entire suffix starting with the first segment reaching the guard,
/// even if that segment is long. A zero watermark tells the caller to retain
/// the audio for final decoding rather than repeatedly decoding a stalled chunk.
pub fn split_commit(
    segments: &[(i64, i64, String)],
    cut_ms: f64,
    hold_back: bool,
) -> (Vec<(i64, i64, String)>, f64) {
    if !hold_back || segments.is_empty() {
        return (segments.to_vec(), cut_ms);
    }
    // A decoder without timings reports its segments spanning nothing; treat
    // its output as untimed and commit it whole (the caller gates on the
    // backend, this is belt and braces).
    if segments.iter().all(|(s, e, _)| *s == 0 && *e == 0) {
        return (segments.to_vec(), cut_ms);
    }

    let guard_start = cut_ms - f64::from(TAIL_GUARD_SECS) * 1000.0;
    let Some(held) = segments
        .iter()
        .position(|(_, e, _)| (*e as f64) > guard_start)
    else {
        return (segments.to_vec(), cut_ms);
    };

    // Overlapping timestamps cannot define a safe retirement boundary.
    let watermark = (segments[held].0 as f64).clamp(0.0, cut_ms);
    if watermark <= 0.0
        || segments[..held]
            .iter()
            .any(|(_, e, _)| *e as f64 > watermark)
    {
        // Ambiguous/overlapping segment timestamps cannot safely retire audio.
        return (Vec::new(), 0.0);
    }
    (segments[..held].to_vec(), watermark)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean_square =
        samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
    mean_square.sqrt()
}

/// Loudest analysis-frame RMS inside `window` (frames tile from its start).
fn frame_rms(window: &[f32], frame_len: usize) -> f32 {
    window
        .chunks(frame_len.max(1))
        .map(rms)
        .fold(0.0f32, f32::max)
}

/// Cut mid-window, aligned to a whole source frame.
fn mid_cut(window_start: usize, quiet_window: usize, channels: usize) -> usize {
    let mut cut = window_start + quiet_window / 2;
    cut -= cut % channels.max(1);
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entire_region_and_overlapping_segments_are_retained() {
        for segments in [
            vec![(0, 20000, "long segment".into())],
            vec![(0, 19000, "first".into()), (18000, 20000, "overlap".into())],
        ] {
            let (kept, watermark) = split_commit(&segments, 20000.0, true);
            assert!(kept.is_empty());
            assert_eq!(watermark, 0.0);
        }
    }

    const RATE: usize = 16_000;

    fn levels_of(samples: &[f32]) -> Levels {
        analyze_levels(samples, (FRAME_SECS * RATE as f32) as usize)
    }

    #[test]
    fn threshold_adapts_to_noisy_ambient() {
        // A pause at the ambient level (0.003) must still read as a pause:
        // the old fixed 0.004 threshold would have missed it entirely.
        let mut s = vec![0.02; RATE * 2];
        s.extend(vec![0.003; RATE]);
        s.extend(vec![0.02; RATE * 2]);
        let levels = levels_of(&s);
        assert!(
            levels.silence_threshold > 0.003,
            "threshold {} must clear the noisy pause",
            levels.silence_threshold
        );
    }

    #[test]
    fn floor_is_capped_at_a_fraction_of_speech() {
        // Continuous speech has no true silence: the floor must not converge
        // on the quiet frames inside speech.
        let samples = vec![0.2f32; RATE * 5];
        let levels = levels_of(&samples);
        assert!(levels.speech_level > 0.19);
        assert!(
            levels.silence_threshold <= levels.speech_level * FLOOR_CAP_RATIO * FLOOR_MULT + 1e-6,
            "threshold {} rides above the speech-capped floor",
            levels.silence_threshold
        );
        assert!(levels.silence_threshold <= MAX_SILENCE_THRESHOLD);
    }

    #[test]
    fn cut_in_real_pause_is_verified_quiet() {
        // 22s speech, 1s silence, 5s speech; scan from 20s like the real peek.
        let mut s = vec![0.1f32; RATE * 22];
        s.extend(vec![0.0; RATE]);
        s.extend(vec![0.1; RATE * 5]);
        let quiet_window = RATE / 4;
        let levels = levels_of(&s);
        let choice = choose_cut(
            Scan {
                region: &s,
                channels: 1,
                frame_len: (FRAME_SECS * RATE as f32) as usize,
                quiet_window,
                scan_from: RATE * 20,
                scan_to: s.len(),
                force: true,
            },
            &levels,
        )
        .expect("the pause exists past the minimum");
        assert!(
            (RATE * 22..=RATE * 23).contains(&choice.cut),
            "cut at {}",
            choice.cut
        );
        assert!(
            choice.verified_quiet,
            "a real pause retires without hold-back"
        );
    }

    #[test]
    fn pause_that_is_loud_against_speech_is_unverified() {
        // A pause that dips under the threshold but carries a blip at a third
        // of speech level: a cut there may slice a word, so it must not be
        // marked verified.
        let mut s = vec![0.2f32; RATE * 22];
        let mut pause = vec![0.005f32; RATE];
        for sample in pause.iter_mut().take((FRAME_SECS * RATE as f32) as usize) {
            *sample = 0.06;
        }
        s.extend(pause);
        s.extend(vec![0.2; RATE * 5]);
        let quiet_window = RATE / 4;
        let levels = levels_of(&s);
        let choice = choose_cut(
            Scan {
                region: &s,
                channels: 1,
                frame_len: (FRAME_SECS * RATE as f32) as usize,
                quiet_window,
                scan_from: RATE * 20,
                scan_to: s.len(),
                force: true,
            },
            &levels,
        )
        .expect("the dip still reads as a pause");
        assert!(
            !choice.verified_quiet,
            "the blip must cost the verified flag"
        );
    }

    #[test]
    fn forced_cut_is_never_verified() {
        let s = vec![0.2f32; RATE * 30];
        let quiet_window = RATE / 4;
        let levels = levels_of(&s);
        let choice = choose_cut(
            Scan {
                region: &s,
                channels: 1,
                frame_len: (FRAME_SECS * RATE as f32) as usize,
                quiet_window,
                scan_from: RATE * 20,
                scan_to: s.len(),
                force: true,
            },
            &levels,
        )
        .expect("a maxed region forces a cut");
        assert!(!choice.verified_quiet);
    }

    #[test]
    fn no_pause_below_the_maximum_keeps_buffering() {
        let s = vec![0.2f32; RATE * 25];
        let quiet_window = RATE / 4;
        let levels = levels_of(&s);
        assert!(choose_cut(
            Scan {
                region: &s,
                channels: 1,
                frame_len: (FRAME_SECS * RATE as f32) as usize,
                quiet_window,
                scan_from: RATE * 20,
                scan_to: s.len(),
                force: false,
            },
            &levels,
        )
        .is_none());
    }

    fn seg(start: i64, end: i64) -> (i64, i64, String) {
        (start, end, "w".to_string())
    }

    #[test]
    fn verified_cut_commits_everything() {
        let segments = vec![seg(0, 4_000), seg(4_000, 9_500)];
        let (kept, watermark) = split_commit(&segments, 10_000.0, false);
        assert_eq!(kept, segments);
        assert_eq!(watermark, 10_000.0);
    }

    #[test]
    fn held_back_suffix_starts_the_next_decode() {
        // 20s region, cut at 20s: the last segment starts inside the span and
        // ends inside the tail guard — sliced-risk, hold it back.
        let segments = vec![seg(0, 18_000), seg(18_500, 19_400), seg(19_600, 19_950)];
        let (kept, watermark) = split_commit(&segments, 20_000.0, true);
        assert_eq!(kept, vec![seg(0, 18_000), seg(18_500, 19_400)]);
        assert_eq!(watermark, 19_600.0);
    }

    #[test]
    fn long_segment_crossing_the_guard_is_retained_whole() {
        let segments = vec![seg(0, 2_000), seg(18_000, 19_700)];
        let (kept, watermark) = split_commit(&segments, 20_000.0, true);
        assert_eq!(kept, vec![seg(0, 2_000)]);
        assert_eq!(watermark, 18_000.0);
    }

    #[test]
    fn no_timing_commits_whole() {
        let segments = vec![(0, 0, "α".to_string()), (0, 0, "β".to_string())];
        let (kept, watermark) = split_commit(&segments, 20_000.0, true);
        assert_eq!(kept, segments);
        assert_eq!(watermark, 20_000.0);
    }

    #[test]
    fn held_back_segments_never_stall_the_watermark() {
        let segments = vec![seg(19_200, 19_900)];
        let (kept, watermark) = split_commit(&segments, 20_000.0, true);
        assert!(kept.is_empty());
        assert!(watermark > 0.0);
    }
}
