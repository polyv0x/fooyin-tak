#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compare the native decoder's PCM and seeks against FFmpeg or source PCM.

By default uses the checked-in fixtures. --encoder adds a generated matrix of
54 streams covering presets 0..4m, 1/2/6 channels, and 8/16/24-bit PCM. Requires
Wine only for --encoder; downloaded/third-party samples are not committed.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

from generate_fixtures import generate


def compare(binary, paths):
    for path in paths:
        probe = json.loads(subprocess.check_output([
            "ffprobe", "-v", "error", "-show_entries", "stream=sample_rate,channels",
            "-of", "json", str(path)]))["streams"][0]
        rate, channels = int(probe["sample_rate"]), probe["channels"]
        with path.open("rb") as stream:
            header = stream.read(9)
        if len(header) == 9 and header[8] & 63 == 0:
            reference = path.with_suffix(".s32le")
            if not reference.exists():
                raise ValueError(f"Legacy stream {path} needs source PCM in {reference}; FFmpeg does not support codec 0")
            expected = reference.read_bytes()
        else:
            expected = subprocess.check_output(["ffmpeg", "-v", "error", "-err_detect", "crccheck+explode",
                                                "-i", str(path), "-f", "s32le", "-"])
        for milliseconds in [0, 1, 53, 139]:
            result = subprocess.run([str(binary), str(path), str(milliseconds)], capture_output=True)
            if result.returncode:
                raise AssertionError(f"{path.name}: {result.stderr.decode().strip()}")
            offset = milliseconds * rate // 1000 * channels * 4
            wanted = expected[offset:]
            if result.stdout != wanted:
                mismatch = next((i for i, (a, b) in enumerate(zip(result.stdout, wanted)) if a != b),
                                min(len(result.stdout), len(wanted)))
                raise AssertionError(f"{path.name}: PCM mismatch at byte {mismatch}, seek {milliseconds}ms")
        print(f"PASS {path.name}: PCM and four seek positions", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--decoder", type=Path, default=Path("target/release/examples/decode"))
    parser.add_argument("--encoder", type=Path)
    parser.add_argument("files", type=Path, nargs="*")
    args = parser.parse_args()
    # Include nested fixtures with saved source PCM. The large silent legacy
    # seek-table fixture has no PCM blob and is covered by the Rust feature test.
    paths = args.files or sorted(path.with_suffix(".tak") for path in
                                 (Path(__file__).parent / "fixtures").rglob("*.s32le"))
    compare(args.decoder.resolve(), paths)
    if args.encoder:
        cases = []
        rates = [8000, 11025, 12000, 22050, 44100, 48000, 96000, 192000]
        for bits in [8, 16, 24]:
            for channels in [1, 2, 6]:
                for preset in ["0", "1", "2", "3", "4", "4m"]:
                    rate = rates[len(cases) % len(rates)]
                    name = f"matrix-{bits}-{channels}-{preset}-{rate}"
                    cases.append((name, rate, channels, bits, 17011, preset, "mixed"))
        with tempfile.TemporaryDirectory(prefix="fooyin-tak-matrix-") as tmp:
            generate(args.encoder.resolve(), Path(tmp), cases)
            compare(args.decoder.resolve(), sorted(Path(tmp).glob("*.tak")))
        print(f"All {len(paths) + len(cases)} files matched reference PCM; all {len(cases)} matrix files also matched FFmpeg.")


if __name__ == "__main__":
    main()
