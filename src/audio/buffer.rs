// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Audio buffer management and resampling to Whisper's expected format (16kHz mono f32).

const SILENCE_RMS_THRESHOLD: f32 = 0.004;
const SILENCE_WINDOW_SAMPLES: usize = 320;
const SILENCE_PADDING_SAMPLES: usize = 1600;
const MIN_SPEECH_SAMPLES: usize = 1600;
const NORMALIZE_TARGET_PEAK: f32 = 0.85;
const MAX_NORMALIZE_GAIN: f32 = 6.0;

/// Quiet-window length used when searching for a chunk cut point (250 ms of
/// source frames — long enough to be a real speech pause, not a plosive gap).
const CHUNK_QUIET_WINDOW_SECS: f32 = 0.25;

pub struct AudioBuffer {
    /// Raw samples as received from cpal (may be multi-channel, any sample rate).
    raw_samples: Vec<f32>,
    /// Source sample rate from the audio device.
    source_sample_rate: u32,
    source_channels: u32,
    /// Target sample rate for Whisper.
    target_sample_rate: u32,
    /// Raw samples already delivered to a while-recording chunk decode.
    /// Advanced only AFTER a chunk decodes successfully (see
    /// [`Self::commit_consumed`]), so a failed chunk loses nothing — its audio
    /// simply stays for the final decode.
    consumed_raw: usize,
}

/// Detached raw audio that can be conditioned away from the capture lock.
#[derive(Clone)]
pub struct RawAudioSnapshot {
    raw_samples: std::sync::Arc<Vec<f32>>,
    source_sample_rate: u32,
    source_channels: u32,
    target_sample_rate: u32,
}

impl RawAudioSnapshot {
    pub fn empty(target_sample_rate: u32) -> Self {
        Self {
            raw_samples: std::sync::Arc::new(Vec::new()),
            source_sample_rate: target_sample_rate,
            source_channels: 1,
            target_sample_rate,
        }
    }

    pub fn condition(self) -> Vec<f32> {
        self.condition_timed().0
    }

    /// Like [`Self::condition`], but also reports how much leading silence the
    /// conditioning trimmed away (ms), so segment timestamps — which are
    /// relative to the conditioned audio — can be mapped back onto the raw
    /// snapshot's timeline (the chunk hold-back rule lives there).
    pub fn condition_timed(self) -> (Vec<f32>, f64) {
        AudioBuffer {
            raw_samples: Vec::new(),
            source_sample_rate: self.source_sample_rate,
            source_channels: self.source_channels,
            target_sample_rate: self.target_sample_rate,
            consumed_raw: 0,
        }
        .prepare_raw(&self.raw_samples)
    }

    /// Number of raw source samples (pre-conditioning).
    pub fn raw_len(&self) -> usize {
        self.raw_samples.len()
    }

    /// Duration of the raw capture in seconds.
    pub fn duration_secs(&self) -> f32 {
        let per_second = (self.source_sample_rate * self.source_channels.max(1)) as f32;
        if per_second == 0.0 {
            return 0.0;
        }
        self.raw_samples.len() as f32 / per_second
    }
}

/// A chunk cut decision from [`AudioBuffer::peek_stable_chunk`].
pub struct ChunkCut {
    /// The region to decode: raw audio from the consumed mark up to the cut.
    pub snapshot: RawAudioSnapshot,
    /// Absolute raw index of the cut. Commit exactly here only when
    /// `verified_quiet`; otherwise commit at the watermark the decoded text
    /// dictates (see [`crate::audio::chunking::split_commit`]).
    pub cut: usize,
    /// Absolute raw index where the snapshot starts (the consumed mark).
    pub region_start: usize,
    /// The cut sits in verified silence: no word can straddle it.
    pub verified_quiet: bool,
    /// Source audio geometry, for mapping decoded times to raw indices.
    pub source_sample_rate: u32,
    pub source_channels: u32,
}

/// Map a snapshot-relative time (ms) back to an absolute raw index in the
/// capture buffer: clamped to the chunk's `[region_start, cut]` span and
/// frame-aligned.
pub fn raw_index_for_ms(
    region_start: usize,
    cut: usize,
    source_sample_rate: u32,
    source_channels: u32,
    ms: f64,
) -> usize {
    let channels = source_channels.max(1) as usize;
    let per_second = (source_sample_rate.max(1) as f64) * (channels as f64);
    let rel = ((ms / 1000.0) * per_second).round().max(0.0) as usize;
    let mut idx = region_start.saturating_add(rel).min(cut);
    idx -= idx % channels;
    idx.max(region_start)
}

impl AudioBuffer {
    pub fn new(target_sample_rate: u32) -> Self {
        Self {
            raw_samples: Vec::with_capacity(target_sample_rate as usize * 30), // 30s pre-alloc
            source_sample_rate: target_sample_rate,
            source_channels: 1,
            target_sample_rate,
            consumed_raw: 0,
        }
    }

    pub fn set_source_params(&mut self, sample_rate: u32, channels: u32) {
        self.source_sample_rate = sample_rate;
        self.source_channels = channels;
        let desired = (sample_rate as usize)
            .saturating_mul(channels.max(1) as usize)
            .saturating_mul(30)
            .min(crate::limits::MAX_BUFFER_SAMPLES);
        if self.raw_samples.capacity() < desired {
            self.raw_samples
                .reserve(desired.saturating_sub(self.raw_samples.len()));
        }
    }

    /// Push new samples into the buffer. Caps total length at
    /// [`crate::limits::MAX_BUFFER_SAMPLES`] so a forgotten/runaway recording
    /// can't grow memory without bound (extra samples are dropped).
    pub fn push_samples(&mut self, samples: &[f32]) -> bool {
        self.push_with_cap(samples, crate::limits::MAX_BUFFER_SAMPLES)
    }

    fn push_with_cap(&mut self, samples: &[f32], cap: usize) -> bool {
        if samples.len() > cap.saturating_sub(self.raw_samples.len()) {
            return false;
        }
        self.raw_samples.extend_from_slice(samples);
        true
    }

    /// Convert and append i16 PCM directly into the existing raw buffer.
    pub fn push_i16_samples(&mut self, samples: &[i16]) -> bool {
        let cap = crate::limits::MAX_BUFFER_SAMPLES;
        let room = cap.saturating_sub(self.raw_samples.len());
        if samples.len() > room {
            return false;
        }
        self.raw_samples.extend(
            samples
                .iter()
                .take(room)
                .map(|&sample| sample as f32 / i16::MAX as f32),
        );
        true
    }

    pub fn raw_len(&self) -> usize {
        self.consumed_raw + self.raw_samples.len()
    }

    pub fn clear(&mut self) {
        self.raw_samples.clear();
        self.consumed_raw = 0;
    }

    /// Detach the raw recording NOT yet consumed by while-recording chunk
    /// decodes (the whole take when chunking was never used), allowing
    /// conditioning on a worker without holding the real-time capture mutex.
    pub fn take_raw_snapshot(&mut self) -> RawAudioSnapshot {
        let raw = std::mem::take(&mut self.raw_samples);
        self.consumed_raw = 0;
        RawAudioSnapshot {
            raw_samples: std::sync::Arc::new(raw),
            source_sample_rate: self.source_sample_rate,
            source_channels: self.source_channels,
            target_sample_rate: self.target_sample_rate,
        }
    }

    /// Look for a stable chunk for while-recording decoding: audio from the
    /// consumed mark up to a cut at a quiet point (a real speech pause), so
    /// the decoder never gets a word sliced in half. Returns a COPY of the
    /// chunk plus the cut; the caller commits a watermark only after the chunk
    /// decoded successfully ([`Self::commit_consumed`]).
    ///
    /// `min_secs`/`max_secs` bound the chunk length: below the minimum this
    /// returns `None` (keep buffering); if no window under the adaptive
    /// silence threshold exists before the maximum, the QUIETEST window in
    /// range is used (marked unverified — see [`crate::audio::chunking`]) so a
    /// non-stop talker still gets bounded chunks.
    pub fn peek_stable_chunk(&self, min_secs: f32, max_secs: f32) -> Option<ChunkCut> {
        let channels = self.source_channels.max(1) as usize;
        let rate = self.source_sample_rate.max(1) as usize;
        let start = 0;
        let available = self.raw_samples.len() - start;

        let min_samples = ((min_secs * rate as f32) as usize).saturating_mul(channels);
        let max_samples = ((max_secs * rate as f32) as usize).saturating_mul(channels);
        if available < min_samples || min_samples == 0 {
            return None;
        }

        let quiet_window = (((CHUNK_QUIET_WINDOW_SECS * rate as f32) as usize).max(1)) * channels;
        let scan_from = min_samples;
        let scan_to = max_samples.min(available);
        if scan_from + quiet_window > scan_to {
            return None;
        }

        // Adaptive levels over the whole uncommitted region (not just the scan
        // range): the pause threshold must not depend on where the chunk
        // bounds happen to fall. Past-max audio is legitimate level evidence.
        let region = &self.raw_samples[start..];
        let frame_len =
            (((crate::audio::chunking::FRAME_SECS * rate as f32) as usize).max(1)) * channels;
        let levels = crate::audio::chunking::analyze_levels(region, frame_len);
        let choice = crate::audio::chunking::choose_cut(
            crate::audio::chunking::Scan {
                region,
                channels,
                frame_len,
                quiet_window,
                scan_from,
                scan_to,
                force: available >= max_samples,
            },
            &levels,
        )?;

        let cut = start + choice.cut;
        Some(ChunkCut {
            snapshot: RawAudioSnapshot {
                raw_samples: std::sync::Arc::new(self.raw_samples[start..cut].to_vec()),
                source_sample_rate: self.source_sample_rate,
                source_channels: self.source_channels,
                target_sample_rate: self.target_sample_rate,
            },
            cut: self.consumed_raw + cut,
            region_start: self.consumed_raw,
            verified_quiet: choice.verified_quiet,
            source_sample_rate: self.source_sample_rate,
            source_channels: self.source_channels,
        })
    }

    /// Advance the consumed mark after a chunk decoded successfully. Ignored
    /// if the buffer was since taken/cleared (`cut` no longer valid).
    pub fn commit_consumed(&mut self, cut: usize) {
        if cut > self.consumed_raw && cut <= self.raw_len() {
            self.raw_samples.drain(..cut - self.consumed_raw);
            self.consumed_raw = cut;
        }
    }

    /// Copy only enough source frames to produce the requested 16 kHz tail.
    /// One extra source second preserves silence-trimming context.
    pub fn tail_raw_snapshot(&self, max_target_samples: usize) -> RawAudioSnapshot {
        let channels = self.source_channels.max(1) as usize;
        let source_rate = self.source_sample_rate.max(1) as usize;
        let target_rate = self.target_sample_rate.max(1) as usize;
        let source_frames = max_target_samples
            .saturating_mul(source_rate)
            .div_ceil(target_rate)
            .saturating_add(source_rate);
        let wanted = source_frames.saturating_mul(channels);
        let mut start = self.raw_samples.len().saturating_sub(wanted);
        start -= start % channels;
        RawAudioSnapshot {
            raw_samples: std::sync::Arc::new(self.raw_samples[start..].to_vec()),
            source_sample_rate: self.source_sample_rate,
            source_channels: self.source_channels,
            target_sample_rate: self.target_sample_rate,
        }
    }

    pub fn duration_secs(&self) -> f32 {
        let total_frames = self.raw_len() / self.source_channels.max(1) as usize;
        total_frames as f32 / self.source_sample_rate.max(1) as f32
    }

    /// Convert the buffered audio to mono 16kHz f32 (Whisper's expected format).
    #[cfg(test)]
    pub fn get_mono_16khz(&self) -> Vec<f32> {
        self.get_mono_16khz_timed().0
    }

    /// Like [`Self::get_mono_16khz`], but also reports the trimmed leading
    /// silence in milliseconds.
    pub fn get_mono_16khz_timed(&self) -> (Vec<f32>, f64) {
        self.prepare_raw(&self.raw_samples)
    }

    fn prepare_raw(&self, raw_samples: &[f32]) -> (Vec<f32>, f64) {
        if raw_samples.is_empty() {
            return (Vec::new(), 0.0);
        }

        let mono = self.to_mono(raw_samples);

        let prepared = if self.source_sample_rate == self.target_sample_rate {
            mono
        } else {
            self.resample(&mono, self.source_sample_rate, self.target_sample_rate)
        };

        self.condition_for_transcription(&prepared)
    }

    fn condition_for_transcription(&self, samples: &[f32]) -> (Vec<f32>, f64) {
        let (trimmed, lead_samples) = self.trim_silence(samples);
        if trimmed.is_empty() {
            return (Vec::new(), 0.0);
        }

        let lead_ms = (lead_samples as f64) * 1000.0 / self.target_sample_rate.max(1) as f64;
        (self.normalize_audio(&trimmed), lead_ms)
    }

    fn to_mono(&self, raw_samples: &[f32]) -> Vec<f32> {
        let channels = self.source_channels as usize;
        if channels <= 1 {
            return raw_samples.to_vec();
        }

        raw_samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    }

    /// Trim leading/trailing silence, returning the trimmed audio plus how
    /// many samples of leading silence were removed.
    fn trim_silence(&self, samples: &[f32]) -> (Vec<f32>, usize) {
        if samples.is_empty() {
            return (Vec::new(), 0);
        }

        let first_window = samples
            .chunks(SILENCE_WINDOW_SAMPLES)
            .position(|chunk| Self::window_rms(chunk) >= SILENCE_RMS_THRESHOLD);
        let last_window = samples
            .chunks(SILENCE_WINDOW_SAMPLES)
            .rposition(|chunk| Self::window_rms(chunk) >= SILENCE_RMS_THRESHOLD);

        let (first_window, last_window) = match (first_window, last_window) {
            (Some(first), Some(last)) => (first, last),
            _ => return (Vec::new(), 0),
        };

        let start = first_window
            .saturating_mul(SILENCE_WINDOW_SAMPLES)
            .saturating_sub(SILENCE_PADDING_SAMPLES);
        let end = ((last_window + 1) * SILENCE_WINDOW_SAMPLES + SILENCE_PADDING_SAMPLES)
            .min(samples.len());

        if end <= start || end - start < MIN_SPEECH_SAMPLES {
            return (Vec::new(), 0);
        }

        (samples[start..end].to_vec(), start)
    }

    fn normalize_audio(&self, samples: &[f32]) -> Vec<f32> {
        let peak = samples
            .iter()
            .fold(0.0f32, |acc, sample| acc.max(sample.abs()));

        if peak <= f32::EPSILON {
            return Vec::new();
        }

        if peak >= NORMALIZE_TARGET_PEAK {
            return samples.to_vec();
        }

        let gain = (NORMALIZE_TARGET_PEAK / peak).min(MAX_NORMALIZE_GAIN);
        samples
            .iter()
            .map(|sample| (sample * gain).clamp(-1.0, 1.0))
            .collect()
    }

    fn window_rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }

        let mean_square =
            samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
        mean_square.sqrt()
    }

    /// High-quality sinc resampling using rubato.
    fn resample(&self, samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
        if samples.is_empty() || from_rate == 0 || to_rate == 0 {
            return Vec::new();
        }

        use rubato::{
            Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
            WindowFunction,
        };

        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let ratio = to_rate as f64 / from_rate as f64;
        let chunk_size = 1024;
        let mut resampler = match SincFixedIn::<f32>::new(
            ratio, 2.0, params, chunk_size, 1, // mono
        ) {
            Ok(r) => r,
            Err(_) => return self.resample_linear(samples, from_rate, to_rate),
        };

        let expected = (samples.len() as f64 * ratio).round() as usize;
        let mut output = Vec::with_capacity(expected + chunk_size);
        let mut pos = 0;

        while pos < samples.len() || output.len() < expected {
            let end = (pos + chunk_size).min(samples.len());
            let mut chunk = samples[pos.min(samples.len())..end].to_vec();
            if chunk.len() < chunk_size {
                chunk.resize(chunk_size, 0.0);
            }
            let input = vec![chunk];
            match resampler.process(&input, None) {
                Ok(result) => {
                    if let Some(ch) = result.first() {
                        output.extend_from_slice(ch);
                    }
                }
                Err(_) => return self.resample_linear(samples, from_rate, to_rate),
            }
            pos += chunk_size;
        }

        // Flush the filter and discard padding. SincFixedIn's initial index
        // centers its kernel on the input timeline; removing output_delay()
        // here shifts speech earlier (covered by the impulse regression test).
        output.truncate(expected);
        output
    }

    /// Fallback linear interpolation resampling.
    fn resample_linear(&self, samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
        let ratio = from_rate as f64 / to_rate as f64;
        let output_len = ((samples.len() as f64) / ratio) as usize;
        let mut output = Vec::with_capacity(output_len);

        for i in 0..output_len {
            let src_pos = i as f64 * ratio;
            let src_idx = src_pos as usize;
            let frac = src_pos - src_idx as f64;

            let sample = if src_idx + 1 < samples.len() {
                samples[src_idx] as f64 * (1.0 - frac) + samples[src_idx + 1] as f64 * frac
            } else if src_idx < samples.len() {
                samples[src_idx] as f64
            } else {
                0.0
            };

            output.push(sample as f32);
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_limit_is_explicit_and_committed_audio_is_released() {
        let mut buffer = AudioBuffer::new(16_000);
        assert!(buffer.push_with_cap(&[0.1; 8], 10));
        assert!(!buffer.push_with_cap(&[0.2; 3], 10));
        assert_eq!(buffer.raw_len(), 8);
        buffer.commit_consumed(6);
        assert_eq!(buffer.raw_samples.len(), 2);
        assert_eq!(buffer.raw_len(), 8);
        assert!(buffer.push_with_cap(&[0.2; 8], 10));
        assert_eq!(buffer.raw_len(), 16);
        buffer.commit_consumed(12);
        assert_eq!(buffer.raw_samples.len(), 4);
        assert_eq!(buffer.take_raw_snapshot().raw_len(), 4);
    }
    use crate::audio::capture::WHISPER_SAMPLE_RATE;

    #[test]
    fn test_mono_conversion() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 2);

        // Stereo: L=1.0, R=0.0, L=0.5, R=0.5
        buf.push_samples(&[1.0, 0.0, 0.5, 0.5]);

        let mono = buf.to_mono(&buf.raw_samples);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.5).abs() < 1e-6);
        assert!((mono[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_duration() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        buf.push_samples(&vec![0.0; 16000]);

        let duration = buf.duration_secs();
        assert!((duration - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_resample_downsample() {
        let buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);

        // 48kHz -> 16kHz: 3:1 ratio
        let source: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();
        let resampled = buf.resample(&source, 48000, 16000);

        // Rubato sinc resampler may produce slightly different length due to
        // filter latency and chunk processing; allow wider tolerance.
        let expected = 1600.0f32;
        let tolerance = expected * 0.1; // 10% tolerance
        assert!(
            (resampled.len() as f32 - expected).abs() < tolerance,
            "Expected ~{} samples, got {}",
            expected,
            resampled.len()
        );
    }

    #[test]
    fn resampling_preserves_timeline_and_flushes_the_tail() {
        let buf = AudioBuffer::new(16000);
        for len in [1024, 1050, 48000] {
            let mut source = vec![0.0; len];
            let impulse = len - 60;
            source[impulse] = 1.0;
            let output = buf.resample(&source, 48000, 16000);
            assert_eq!(output.len(), (len as f64 / 3.0).round() as usize);
            let peak = output
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap()
                .0;
            assert!(peak.abs_diff(impulse / 3) <= 1, "{len}: peak at {peak}");
            assert!(output[peak].abs() > 0.1);
        }
    }

    #[test]
    fn test_empty_buffer() {
        let buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        assert!(buf.get_mono_16khz().is_empty());
        assert!((buf.duration_secs() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_trim_silence_keeps_voice_region() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);

        let mut samples = vec![0.0; 3200];
        samples.extend(vec![0.1; 3200]);
        samples.extend(vec![0.0; 3200]);
        buf.push_samples(&samples);

        let conditioned = buf.get_mono_16khz();
        assert!(!conditioned.is_empty());
        assert!(conditioned.len() < samples.len());
    }

    #[test]
    fn test_silence_only_is_discarded() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        buf.push_samples(&vec![0.0; 16000]);

        assert!(buf.get_mono_16khz().is_empty());
    }

    #[test]
    fn chunk_peek_waits_for_minimum_audio() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        buf.push_samples(&vec![0.1; 16000 * 5]); // 5s of "speech"
        assert!(buf.peek_stable_chunk(20.0, 40.0).is_none());
    }

    #[test]
    fn chunk_cuts_at_a_pause_and_commit_advances() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        // 22s speech, 1s pause, 5s speech.
        let mut s = vec![0.1; 16000 * 22];
        s.extend(vec![0.0; 16000]);
        s.extend(vec![0.1; 16000 * 5]);
        buf.push_samples(&s);

        let chunk = buf
            .peek_stable_chunk(20.0, 40.0)
            .expect("pause exists past the minimum");
        let cut = chunk.cut;
        assert!((16000 * 22..=16000 * 23).contains(&cut), "cut at {cut}");
        assert_eq!(chunk.snapshot.raw_len(), cut);
        // A real pause retires exactly at the cut — no hold-back needed.
        assert!(chunk.verified_quiet);

        buf.commit_consumed(cut);
        let tail = buf.take_raw_snapshot();
        assert_eq!(tail.raw_len(), s.len() - cut);
    }

    #[test]
    fn chunk_without_pause_waits_until_max_then_forces_cut() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        // 25s of continuous speech: past the minimum but no pause and below
        // the maximum — keep buffering.
        buf.push_samples(&vec![0.2; 16000 * 25]);
        assert!(buf.peek_stable_chunk(20.0, 40.0).is_none());
        // Past the maximum: a cut is forced at the quietest window in range.
        buf.push_samples(&vec![0.2; 16000 * 16]); // 41s total
        let chunk = buf
            .peek_stable_chunk(20.0, 40.0)
            .expect("forced cut once the region hits its max");
        let cut = chunk.cut;
        assert!(cut >= 16000 * 20, "cut at {cut}");
        assert_eq!(chunk.snapshot.raw_len(), cut);
        // Forced cuts may slice a word: the decoded text must hold back.
        assert!(!chunk.verified_quiet);
    }

    #[test]
    fn chunk_commit_is_ignored_after_take_or_clear() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 1);
        buf.push_samples(&vec![0.1; 16000]);
        let _ = buf.take_raw_snapshot();
        // A late commit for the already-taken recording must not corrupt the
        // consumed mark of the (empty or future) buffer.
        buf.commit_consumed(500);
        buf.push_samples(&vec![0.1; 800]);
        assert_eq!(buf.take_raw_snapshot().raw_len(), 800);
    }

    #[test]
    fn chunk_cut_is_frame_aligned_for_stereo() {
        let mut buf = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        buf.set_source_params(16000, 2);
        // Stereo: 21s speech, 1s pause, 2s speech (samples = 2 per frame).
        let mut s = vec![0.1; 16000 * 2 * 21];
        s.extend(vec![0.0; 16000 * 2]);
        s.extend(vec![0.1; 16000 * 2 * 2]);
        buf.push_samples(&s);
        let chunk = buf
            .peek_stable_chunk(20.0, 40.0)
            .expect("pause exists past the minimum");
        assert_eq!(chunk.cut % 2, 0, "cut must land on a frame boundary");
    }

    #[test]
    fn raw_index_for_ms_maps_onto_the_chunk_span() {
        // 2s at 16 kHz mono maps to 32000 raw samples past the region start.
        assert_eq!(raw_index_for_ms(1000, 50_000, 16_000, 1, 2_000.0), 33_000);
        // Clamped to the cut and frame-aligned (stereo).
        assert_eq!(raw_index_for_ms(0, 100, 16_000, 2, 999_999.0), 100);
        assert_eq!(raw_index_for_ms(0, 100, 16_000, 2, 1.0), 32);
        let idx = raw_index_for_ms(0, 100, 16_000, 2, 25.0);
        assert_eq!(idx % 2, 0);
    }
}
