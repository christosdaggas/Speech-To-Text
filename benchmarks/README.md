# Offline regression benchmark

The six original English/Greek sentences in `corpus.json` probe normal speech,
polite endings, repetitions, leading/trailing silence and source timestamps.
They are synthetic regression fixtures, **not** a representative accuracy study.

```sh
python3 scripts/benchmark.py self-test
python3 scripts/benchmark.py generate target/benchmark-corpus
cargo build --offline --locked --release --features vulkan
target/release/speech-to-text --benchmark target/benchmark-corpus/manifest.json whisper tiny gpu > target/whisper-benchmark.json
python3 scripts/benchmark.py score target/whisper-benchmark.json
```

Generation needs an already installed `espeak-ng`; it records its version,
parameters and SHA-256 of every WAV. Keep the same generated directory for a
before/after comparison. No download is performed. Use `cpu` for CPU builds;
the selected Whisper model or sidecar runtime/model must already be installed.
Parakeet: replace `whisper tiny gpu` with `parakeet parakeet-tdt-0.6b-v3 cpu`.
For Qwen, select an installed size explicitly: `qwen 0.6B cpu` or `qwen 1.7B cpu`.

The command bypasses GTK and does not save transcripts to history. It reports
raw and cleaned text, per-stage milliseconds, real-time factor and process peak
memory. Each clip runs twice in the same process; only the first clip starts
with an empty Whisper slot. Parakeet remains a per-request process. Preserve
build version, model hashes, compiler, CPU/GPU and driver details alongside
results. Compare raw and cleaned WER separately. The scorer is case-, punctuation- and
diacritic-insensitive (Unicode NFKD, including Greek tonos).
Do not infer production
accuracy or warm-runtime/VAD-reuse benefits from these six synthetic clips.
No warm Parakeet/VAD optimization is enabled without additional measurements.

## Native cancellation regression

The benchmark uses an active cancellation token, just like GUI/API jobs. This
also exercises the native callback; testing without a token missed a closure
layout defect in whisper-rs 0.16.0 that caused false encoder errors and crashes.

To compare a control decode without a callback with repeated cancellable decodes
and verify an already-cancelled request, use the opt-in model-backed test:

```sh
STT_TEST_MODEL=/path/to/ggml-large-v3-turbo-q5_0.bin \
STT_TEST_WAV="$PWD/target/benchmark-corpus/el-basic.wav" \
STT_TEST_GPU=1 \
cargo test --offline --locked --features vulkan \
  transcription::engine::tests::real_audio_with_cancellation_token -- --ignored --exact
```

Set `STT_TEST_GPU=0` to check CPU execution. The fixture and model must already
exist; this test does not download anything or read user recordings.
