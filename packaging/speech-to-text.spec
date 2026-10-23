# Source-build spec; local binary packaging uses ../speech-to-text.spec.

%global appid com.chrisdaggas.speech-to-text

Name:           speech-to-text
Version:        1.0.0
Epoch:          1
Release:        1%{?dist}
Summary:        Local speech-to-text transcription using Whisper (GTK4/libadwaita)

License:        MIT AND Apache-2.0
URL:            https://chrisdaggas.com
Source0:        https://github.com/christosdaggas/Speech-To-Text/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.99
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  cmake
BuildRequires:  clang
BuildRequires:  gtk4-devel >= 4.14
BuildRequires:  libadwaita-devel >= 1.5
BuildRequires:  alsa-lib-devel
BuildRequires:  vulkan-loader-devel
BuildRequires:  glslc
BuildRequires:  glib2-devel
BuildRequires:  gettext
BuildRequires:  desktop-file-utils
BuildRequires:  appstream

Requires:       gtk4 >= 4.14
Requires:       libadwaita >= 1.5
Requires:       alsa-lib
Requires:       vulkan-loader
Recommends:     xdg-desktop-portal-gnome
Recommends:     gnome-keyring

%description
Speech to Text is made for Fedora with the GNOME desktop, tested on Fedora 44. It
transcribes speech locally using Whisper (with optional Parakeet and Qwen3-ASR
backends). Transcription runs entirely on your machine. Optional, opt-in
features may use the network: an "Improve with AI" LLM integration you
configure, and a startup update check — both can be disabled.

%prep
%autosetup -n Speech-To-Text-%{version}

%build
# whisper-rs ships pregenerated bindings; avoid the bindgen/libclang dependency.
export WHISPER_DONT_GENERATE_BINDINGS=1
. scripts/release-env.sh
cargo build --release --locked --features vulkan

%install
install -Dm0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}

install -Dm0644 data/%{appid}.desktop \
    %{buildroot}%{_datadir}/applications/%{appid}.desktop
install -Dm0644 data/icons/hicolor/scalable/apps/%{appid}.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg
install -Dm0644 data/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg \
    %{buildroot}%{_datadir}/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg
# Also under scalable/apps: many StatusNotifier hosts only probe the sized and
# scalable app directories, never symbolic/apps, so a tray icon installed only
# in the canonical place is simply never found.
install -Dm0644 data/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{appid}-symbolic.svg
install -Dm0644 data/icons/hicolor/scalable/apps/%{appid}-ai.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{appid}-ai.svg
install -Dm0644 data/%{appid}.metainfo.xml \
    %{buildroot}%{_metainfodir}/%{appid}.metainfo.xml

# Explicit packaging compilation; Cargo-generated catalogues stay in OUT_DIR.
for lang in de el es fr it pt ru zh; do
    mkdir -p %{buildroot}%{_datadir}/locale/${lang}/LC_MESSAGES
    msgfmt -c po/${lang}.po -o %{buildroot}%{_datadir}/locale/${lang}/LC_MESSAGES/%{name}.mo
done
install -Dm0644 data/vad/ggml-silero-v5.1.2.bin \
    %{buildroot}%{_datadir}/speech-to-text/ggml-silero-v5.1.2.bin

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/%{appid}.desktop
appstreamcli validate --no-net \
    %{buildroot}%{_metainfodir}/%{appid}.metainfo.xml

%files
%license LICENSE native/LICENSE-sherpa-onnx native/NOTICE
%license vendor/whisper-rs-sys/LICENSE vendor/whisper-rs-sys/whisper.cpp/LICENSE
%doc README.md
%{_bindir}/%{name}
%{_datadir}/applications/%{appid}.desktop
%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg
%{_datadir}/icons/hicolor/scalable/apps/%{appid}-ai.svg
%{_datadir}/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg
%{_datadir}/icons/hicolor/scalable/apps/%{appid}-symbolic.svg
%{_metainfodir}/%{appid}.metainfo.xml
%{_datadir}/locale/*/LC_MESSAGES/%{name}.mo
%dir %{_datadir}/speech-to-text
%{_datadir}/speech-to-text/ggml-silero-v5.1.2.bin

%changelog
* Fri Oct 23 2026 Christos A. Daggas <info@chrisdaggas.com> - 1:1.0.0-1
- Align application and package versions, with Epoch for existing installations
- Use current Fedora metadata validation and GNOME portal recommendations

* Thu Aug 20 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.7.0-1
- Update the source-build spec to the 1.7.0 release: ~4x faster Vulkan decoding
  (whisper.cpp 1.8.3), Silero VAD gating, complete de/el/es/it translations,
  tray icon and RemoteDesktop session fixes. See CHANGELOG.md for details

* Fri Aug 14 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.6.0-2
- Rebuilt the public source RPM with the Vulkan backend enabled for AMD and
  other Vulkan-capable GPUs; CPU fallback remains available

* Sat Aug 01 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.6.0-1
- Fixed: auto-paste now works with the main window open in the background;
  honest delivery reporting and a persistent portal session (~1s faster pastes)
- Added: while-recording chunked decoding for long dictations
- Added: warm sidecar servers for Qwen3-ASR and Cohere (weights load once)
- Changed: AI auto-improve no longer delays delivery; leaner decode defaults;
  history/settings writes off the UI thread; instant recording start
- Fixed: finished transcriptions always reach History; microphone loss and
  portal restarts recover automatically
- Security: capped metadata downloads, no-plaintext redirects, stronger
  redaction, minimal health endpoint, strict parameter validation

* Wed Jul 22 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.5.0-5
- Fixed: dictation from the mini panel / global shortcut said "No model loaded"
  when the app was autostarted hidden — the selected model is now preloaded at
  startup, and the transcription worker lazy-loads it as a safety net
- Fixed: mini-panel dictation could translate speech to English even though the
  Translate toggles showed off — the saved toggle state was never restored into
  the UI, so a stale saved "on" kept translating silently
- Changed: refreshed the application logo (yellow microphone)
- Changed: the tray menu no longer shows an icon next to Quit

* Wed Jul 22 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.5.0-4
- Rebuilt against patched dependencies: rustls-webpki 0.103.13 fixes a reachable
  panic in CRL parsing plus three name-constraint advisories on the HTTPS path
  used for model downloads (RUSTSEC-2026-0104, -0099, -0098, -0049)

* Wed Jul 22 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.5.0-3
- Fixed the tray icon showing as an empty slot: the icon is now sent as raw
  pixels (StatusNotifier IconPixmap) instead of relying on a themed icon name

* Wed Jul 22 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.5.0-2
- Restored the symbolic tray icon to a readable size

* Wed Jul 22 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.5.0-1
- Verified and resumable model/runtime downloads
- Faster bounded inference and more reliable recording workflows
- Expanded History, safer AI integration, and refined application UI

* Mon Jun 08 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.4.0-1
- Fixed: mini panel intermittent "Generic whisper error, code -6" on Vulkan GPUs with larger models / wider beam search. Mini panel now uses a clean batch decode.
- Fixed: borderline audio no longer breaks a transcription — whisper's built-in temperature retry re-enabled (temperature_inc = 0.2).
- Changed: "Show text live while transcribing" applies only to the main window now; the mini panel is always a clean batch decode.
- Changed: beam_size honoured everywhere — the main window's live preview no longer hard-codes greedy decoding (self-protection still applies).
- Changed: mini panel transform actions collapse into a single "Actions" dropdown next to Voice edit.
- Changed: Settings pages now fill the full content width.

* Sat Jun 06 2026 Christos A. Daggas <info@chrisdaggas.com> - 1.3.0-1
- Security & distribution hardening release (see CHANGELOG / SECURITY.md).
- Source-built RPM (was prebuilt-binary only).
