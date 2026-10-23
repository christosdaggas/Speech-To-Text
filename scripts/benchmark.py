#!/usr/bin/env python3
"""Generate a hash-pinned EN/EL synthetic corpus and independently score WER."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import unicodedata
import wave


def words(text):
    normalized = unicodedata.normalize("NFKD", text).casefold()
    normalized = "".join(char for char in normalized if not unicodedata.combining(char))
    return re.findall(r"\w+", normalized)


def distance(reference, hypothesis):
    row = list(range(len(hypothesis) + 1))
    for i, source in enumerate(reference, 1):
        next_row = [i]
        for j, target in enumerate(hypothesis, 1):
            next_row.append(min(next_row[-1] + 1, row[j] + 1, row[j - 1] + (source != target)))
        row = next_row
    return row[-1]


def generate(output):
    output.mkdir(parents=True, exist_ok=True)
    corpus = json.loads((Path(__file__).resolve().parents[1] / "benchmarks/corpus.json").read_text())
    version = subprocess.check_output(["espeak-ng", "--version"], text=True).strip()
    for case in corpus["cases"]:
        destination = output / (case["id"] + ".wav")
        if destination.exists():
            raise SystemExit(f"Refusing to overwrite {destination}; use a new corpus directory")
        with tempfile.TemporaryDirectory(dir=output) as temporary:
            source = Path(temporary) / "speech.wav"
            subprocess.run(["espeak-ng", "-v", case["language"], "-s", "145", "-w", str(source), "--", case["reference"]], check=True)
            with wave.open(str(source), "rb") as audio:
                params = audio.getparams()
                frames = audio.readframes(audio.getnframes())
            # A fixed silence prefix/tail probes source-timeline correctness.
            silence = bytes(params.framerate * params.nchannels * params.sampwidth)
            with wave.open(str(destination), "wb") as audio:
                audio.setparams(params)
                audio.writeframes(silence + frames + silence)
        case.update(path=destination.name, sha256=hashlib.sha256(destination.read_bytes()).hexdigest())
    corpus["generator"] = {"version": version, "speed": 145, "silence_seconds": 1}
    (output / "manifest.json").write_text(json.dumps(corpus, ensure_ascii=False, indent=2) + "\n")
    print(output / "manifest.json")


def score(path):
    data = json.loads(path.read_text())
    groups = {}
    for result in data["results"]:
        key = (result["language"], result["pass"])
        group = groups.setdefault(key, {"words": 0, "raw_errors": 0, "cleaned_errors": 0, "milliseconds": 0})
        reference = words(result["reference"])
        group["words"] += len(reference)
        group["raw_errors"] += distance(reference, words(result["raw_text"]))
        group["cleaned_errors"] += distance(reference, words(result["text"]))
        group["milliseconds"] += result["total_ms"]
    for key, group in groups.items():
        for kind in ["raw", "cleaned"]:
            group[kind + "_wer"] = group[kind + "_errors"] / max(1, group["words"])
        print(json.dumps(dict(language=key[0], pass_index=key[1], **group), ensure_ascii=False))


def self_test():
    assert distance(words("one two three"), words("one two three")) == 0
    assert distance(words("one two three"), words("one four")) == 2
    assert distance(words("Σας ευχαριστούμε πολύ"), words("ΣΑΣ ΕΥΧΑΡΙΣΤΟΥΜΕ ΠΟΛΥ!")) == 0
    assert distance(words("no no no no"), words("no")) == 3
    print("BENCHMARK_SCORER_TESTS_PASSED")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("generate").add_argument("output", type=Path)
    commands.add_parser("score").add_argument("results", type=Path)
    commands.add_parser("self-test")
    args = parser.parse_args()
    if args.command == "generate":
        generate(args.output)
    elif args.command == "score":
        score(args.results)
    else:
        self_test()
