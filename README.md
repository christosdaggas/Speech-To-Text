# Speech to Text

<img width="100%" alt="Speech to Text" src="docs/app.png" />

Turn your voice and audio recordings into text, right on your computer.

**Made for Fedora with the GNOME desktop (tested on Fedora 44).**

## What you can do

- Dictate using a keyboard shortcut and a small floating panel.
- Transcribe audio files, detect the language, or translate to English with Whisper.
- Search your transcription history and export your text.
- Optionally polish text with an AI service you choose.
- Choose your interface language and a light, dark or system theme.

Whisper is the default engine; Parakeet and Qwen3-ASR are optional local engines.
Download a model once, then transcribe offline. Vulkan GPU acceleration is included,
with CPU fallback.
For repeated dictation, Parakeet keeps its model in memory. Switching engines,
cancelling an active transcription or quitting releases it; Whisper also reuses its voice detector.

## Install

Download the Fedora x86_64 RPM from [GitHub Releases](https://github.com/christosdaggas/speech-to-text/releases/latest), then run:

```sh
sudo dnf install ./speech-to-text-1.0.0-*.x86_64.rpm
```

Open **Speech to Text** from GNOME's app launcher. Choose a model in
**Recognition → Model**, then record or open an audio file. Configure dictation
shortcuts and paste permissions in **Dictation & Audio → Dictation**.
The tray icon is optional; GNOME needs an AppIndicator extension to display it.

## Privacy

Audio is processed locally. Settings and history stay on your computer, and API
keys are stored in the system keyring. Optional cloud AI sends transcript text to
your chosen provider. The startup update check contacts GitHub and can be disabled
in Settings. See [PRIVACY.md](PRIVACY.md).

## Build and help

For a local Fedora RPM, run `bash scripts/build-rpm.sh`; the result is always saved
in `dist/rpm/`. Build requirements and developer checks are in
[CONTRIBUTING.md](CONTRIBUTING.md).

[What's new](CHANGELOG.md) · [Security](SECURITY.md) ·
[Website](https://chrisdaggas.com) · [Contact](mailto:info@chrisdaggas.com)

See [LICENSE](LICENSE) and [third-party notices](native/NOTICE).
