// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Decode audio files (WAV, MP3, FLAC, OGG, etc.) to mono 16kHz f32 PCM
//! using Symphonia.

use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::buffer::AudioBuffer;
use super::capture::WHISPER_SAMPLE_RATE;
use crate::error::{AppError, AppResult};
use rubato::Resampler;

/// Decode an audio file to mono 16kHz f32 PCM suitable for Whisper.
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub source_duration_secs: f32,
    pub leading_offset_ms: f64,
}

pub fn decode_audio_file(path: &Path) -> AppResult<DecodedAudio> {
    decode_audio_file_cancellable(path, None)
}

pub fn decode_audio_file_cancellable(
    path: &Path,
    cancel: Option<&crate::cancel::CancelToken>,
) -> AppResult<DecodedAudio> {
    // Reject oversized files before decoding (DoS guard).
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > crate::limits::MAX_DROPPED_FILE_BYTES {
            return Err(AppError::Audio(format!(
                "Audio file is too large ({:.1} GB); the limit is {:.0} GB.",
                meta.len() as f64 / 1e9,
                crate::limits::MAX_DROPPED_FILE_BYTES as f64 / 1e9
            )));
        }
    }

    let file = std::fs::File::open(path)
        .map_err(|e| AppError::Audio(format!("Cannot open file: {}", e)))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| AppError::Audio(format!("Unsupported audio format: {}", e)))?;

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| AppError::Audio("No audio track found".into()))?;

    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u32)
        .unwrap_or(1);
    if !(1_000..=384_000).contains(&sample_rate) || !(1..=32).contains(&channels) {
        return Err(AppError::Audio(format!(
            "Unsupported audio parameters: {sample_rate}Hz, {channels} channels"
        )));
    }
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AppError::Audio(format!("Unsupported codec: {}", e)))?;

    let mut pcm_stream = PcmStream::new(sample_rate)?;

    let mut decoded_samples: usize = 0;
    loop {
        if cancel.is_some_and(|token| token.is_aborted()) {
            return Err(AppError::Cancelled);
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(e) => return Err(AppError::Audio(format!("Audio packet read failed: {e}"))),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => return Err(AppError::Audio(format!("Audio decode failed: {e}"))),
        };

        let spec = *decoded.spec();
        if spec.rate != sample_rate || spec.channels.count() != channels as usize {
            return Err(AppError::Audio(
                "Audio format changed during decoding".into(),
            ));
        }
        let n_frames = decoded.capacity();

        let mut sample_buf = SampleBuffer::<f32>::new(n_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);

        let samples = sample_buf.samples();
        decoded_samples = decoded_samples.saturating_add(samples.len());
        let source_frames = decoded_samples / channels as usize;
        let projected_output = source_frames
            .saturating_mul(WHISPER_SAMPLE_RATE as usize)
            .div_ceil(sample_rate as usize);
        if projected_output > crate::limits::MAX_DECODED_SAMPLES {
            return Err(AppError::Audio(
                "Audio exceeds the decoded duration limit; split the file before retrying".into(),
            ));
        }
        for frame in samples.chunks_exact(channels as usize) {
            pcm_stream.push(frame.iter().sum::<f32>() / channels as f32)?;
        }
    }

    let source_duration_secs = decoded_samples as f32 / channels as f32 / sample_rate as f32;
    if cancel.is_some_and(|token| token.is_aborted()) {
        return Err(AppError::Cancelled);
    }
    let audio_buffer = pcm_stream.finish()?;
    let (pcm, leading_offset_ms) = audio_buffer.get_mono_16khz_timed();
    if pcm.is_empty() {
        return Err(AppError::Audio("No audio data decoded from file".into()));
    }

    tracing::info!(
        "Decoded audio file: {:.1}s at {}Hz {}ch → {} samples at 16kHz mono",
        pcm.len() as f32 / WHISPER_SAMPLE_RATE as f32,
        sample_rate,
        channels,
        pcm.len()
    );

    Ok(DecodedAudio {
        samples: pcm,
        source_duration_secs,
        leading_offset_ms,
    })
}

/// Downmix and resample incrementally; only target-rate PCM grows with the file.
struct PcmStream {
    resampler: Option<rubato::SincFixedIn<f32>>,
    pending: Vec<f32>,
    output: Vec<f32>,
    source_frames: usize,
    rate: u32,
}

impl PcmStream {
    fn new(rate: u32) -> AppResult<Self> {
        let resampler = if rate == WHISPER_SAMPLE_RATE {
            None
        } else {
            Some(
                rubato::SincFixedIn::new(
                    WHISPER_SAMPLE_RATE as f64 / rate as f64,
                    2.0,
                    rubato::SincInterpolationParameters {
                        sinc_len: 256,
                        f_cutoff: 0.95,
                        interpolation: rubato::SincInterpolationType::Linear,
                        oversampling_factor: 256,
                        window: rubato::WindowFunction::BlackmanHarris2,
                    },
                    1024,
                    1,
                )
                .map_err(|e| AppError::Audio(format!("Cannot resample audio: {e}")))?,
            )
        };
        Ok(Self {
            resampler,
            pending: Vec::with_capacity(1024),
            output: Vec::new(),
            source_frames: 0,
            rate,
        })
    }

    fn push(&mut self, sample: f32) -> AppResult<()> {
        self.source_frames += 1;
        if self.resampler.is_none() {
            self.output.push(sample);
        } else {
            self.pending.push(sample);
            if self.pending.len() == 1024 {
                self.process()?;
            }
        }
        Ok(())
    }

    fn process(&mut self) -> AppResult<()> {
        self.pending.resize(1024, 0.0);
        if let Some(resampler) = self.resampler.as_mut() {
            let result = resampler
                .process(&[&self.pending], None)
                .map_err(|e| AppError::Audio(format!("Audio resampling failed: {e}")))?;
            self.output.extend_from_slice(&result[0]);
        }
        self.pending.clear();
        Ok(())
    }

    fn finish(mut self) -> AppResult<AudioBuffer> {
        let expected = (self.source_frames as f64 * WHISPER_SAMPLE_RATE as f64 / self.rate as f64)
            .round() as usize;
        if self.resampler.is_some() {
            while !self.pending.is_empty() || self.output.len() < expected {
                self.process()?;
            }
        }
        self.output.truncate(expected);
        let mut buffer = AudioBuffer::new(WHISPER_SAMPLE_RATE);
        if !buffer.push_samples(&self.output) {
            return Err(AppError::Audio("Decoded audio exceeds memory limit".into()));
        }
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_mono_and_stereo_resampling_preserves_source_timeline() {
        let dir = tempfile::tempdir().unwrap();
        for rate in [8_000u32, 44_100, 48_000] {
            for channels in [1u16, 2] {
                let samples: Vec<f32> = (0..rate * 3)
                    .flat_map(|frame| {
                        let value = if (rate..rate * 2).contains(&frame) {
                            (frame as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.3
                        } else {
                            0.0
                        };
                        std::iter::repeat_n(value, channels as usize)
                    })
                    .collect();
                let mut wav = crate::transcription::encode_wav_16bit(&samples, rate);
                wav[22..24].copy_from_slice(&channels.to_le_bytes());
                wav[28..32].copy_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
                wav[32..34].copy_from_slice(&(channels * 2).to_le_bytes());
                let path = dir.path().join(format!("{rate}-{channels}.wav"));
                std::fs::write(&path, wav).unwrap();
                let decoded = super::decode_audio_file(&path).unwrap();
                assert!((decoded.source_duration_secs - 3.0).abs() < 0.01);
                assert!((850.0..=1000.0).contains(&decoded.leading_offset_ms));
                assert!((17_000..=21_000).contains(&decoded.samples.len()));
            }
        }
    }
    #[test]
    fn file_conditioning_preserves_source_duration_and_leading_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silence-speech.wav");
        let mut samples = vec![0.0; 16_000];
        samples.extend((0..16_000).map(|i| (i as f32 * 0.08).sin() * 0.3));
        samples.extend(vec![0.0; 16_000]);
        std::fs::write(
            &path,
            crate::transcription::encode_wav_16bit(&samples, 16_000),
        )
        .unwrap();
        let decoded = super::decode_audio_file(&path).unwrap();
        assert!((decoded.source_duration_secs - 3.0).abs() < 0.01);
        assert!((850.0..=1000.0).contains(&decoded.leading_offset_ms));
        assert!(decoded.samples.len() < samples.len());
    }
}
