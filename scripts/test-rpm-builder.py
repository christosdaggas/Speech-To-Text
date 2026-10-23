#!/usr/bin/env python3
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def executable(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    path.chmod(0o755)


def check_builder(report_artifact):
    with tempfile.TemporaryDirectory(prefix="stt-rpm-builder-") as directory:
        root = Path(directory)
        scripts = root / "scripts"
        scripts.mkdir()
        shutil.copyfile(Path(__file__).with_name("build-rpm.sh"), scripts / "build-rpm.sh")
        executable(scripts / "version.sh", '#!/bin/sh\n[ "$1" = --check ] || echo 1.0.0\n')
        (scripts / "release-env.sh").write_text("")
        for name in [
            "data/com.chrisdaggas.speech-to-text.desktop",
            "data/com.chrisdaggas.speech-to-text.metainfo.xml",
            "data/resources/style.css",
            "data/icons/hicolor/scalable/apps/com.chrisdaggas.speech-to-text.svg",
            "data/icons/hicolor/scalable/apps/com.chrisdaggas.speech-to-text-ai.svg",
            "data/icons/hicolor/symbolic/apps/com.chrisdaggas.speech-to-text-symbolic.svg",
            "data/vad/ggml-silero-v5.1.2.bin", "LICENSE", "native/LICENSE-sherpa-onnx",
            "native/NOTICE", "speech-to-text.spec", "po/en.po",
            "vendor/whisper-rs-sys/LICENSE", "vendor/whisper-rs-sys/whisper.cpp/LICENSE",
        ]:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture\n")
        (root / "po/LINGUAS").write_text("en\n")
        binary = root / "custom cargo target/release/speech-to-text"
        executable(binary, '#!/bin/sh\n: ggml_vulkan\nif [ "$1" = --engine-version ]; then echo "whisper.cpp 1.9.5"; else echo "Speech to Text 1.0.0"; fi\n')
        executable(root / "target/release/speech-to-text", "#!/bin/sh\nexit 42\n")
        tools = root / "tools"
        artifact = {"reason": "compiler-artifact", "target": {"name": "speech-to-text", "kind": ["bin"]}, "executable": str(binary)}
        executable(tools / "cargo", "#!/bin/sh\ncat <<'JSON'\n" + json.dumps(artifact if report_artifact else {"reason": "build-finished", "success": True}) + "\nJSON\n")
        for tool in ["desktop-file-validate", "appstreamcli"]:
            executable(tools / tool, "#!/bin/sh\nexit 0\n")
        executable(tools / "msgfmt", '#!/bin/sh\nwhile [ "$1" != -o ]; do shift; done\n: > "$2"\n')
        name = "speech-to-text-1.0.0-1.fc44.x86_64.rpm"
        executable(tools / "rpmspec", f'#!/bin/sh\necho "{name}"\n')
        executable(tools / "rpm", '#!/bin/sh\ncase "$*" in *--requires*) echo libvulkan;; *) echo 1.0.0;; esac\n')
        executable(tools / "rpmbuild", f'''#!/bin/sh
while [ "$#" -gt 0 ]; do
    case "$1" in "_rpmdir "*) output=${{1#_rpmdir }};; esac
    shift
done
[ -n "$output" ] || exit 1
echo fixture > "$output/{name}"
''')
        environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"])
        result = subprocess.run(["bash", str(scripts / "build-rpm.sh")], cwd=root, env=environment, capture_output=True, text=True)
        package = root / "dist/rpm" / name
        if report_artifact:
            assert result.returncode == 0, result.stdout + result.stderr
            assert package.is_file()
            assert package.with_suffix(".rpm.sha256").is_file()
            assert (root / "dist/rpmbuild/SOURCES/speech-to-text").read_bytes() == binary.read_bytes()
        else:
            assert result.returncode != 0
            assert "Cargo did not report" in result.stderr
            assert not package.exists()


check_builder(True)
check_builder(False)
assert "%autosetup -n Speech-To-Text-%{version}" in Path(__file__).resolve().parents[1].joinpath("packaging/speech-to-text.spec").read_text()
print("RPM_BUILDER_REGRESSIONS_PASSED")
