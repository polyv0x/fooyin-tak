#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Generate codec type 0 fixtures with TAKC 1.0, then verify native PCM and seeks.

The official encoder's -v option independently verifies its encoded audio.
Usage: python3 tests/generate_legacy_fixtures.py /path/to/Takc.exe
Requires Wine; neither the encoder nor any music is distributed.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from generate_fixtures import pcm, wave_bytes


CASES = [
    ("legacy-mono8", 8000, 1, 8, 4109, "0", "mixed"),
    ("legacy-stereo8", 11025, 2, 8, 4517, "1", "mixed"),
    ("legacy-stereo16", 44100, 2, 16, 22067, "4m", "mixed"),
    ("legacy-stereo24", 96000, 2, 24, 24031, "3m", "mixed"),
    ("legacy-silence16", 22050, 2, 16, 10007, "2", "silence"),
    ("legacy-negative24", 32000, 1, 24, 8017, "4m", "negative"),
]


def generate(encoder, output, decoder, cases=CASES):
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, WINEDEBUG="-all")
    manifest = {}
    with tempfile.TemporaryDirectory(prefix="fooyin-tak-legacy-") as tmp:
        for name, rate, channels, bits, count, preset, signal in cases:
            raw, expected = pcm(rate, channels, bits, count, signal)
            wav = Path(tmp) / (name + ".wav")
            wav.write_bytes(wave_bytes(rate, channels, bits, raw))
            target = output.resolve() / (name + ".tak")
            winpath = lambda path: "Z:" + str(path.resolve()).replace("/", "\\")
            # A file avoids inheriting captured pipes in Wine's background services.
            with tempfile.TemporaryFile(mode="w+") as log:
                result = subprocess.run(["wine", str(encoder), "-e", "-p" + preset,
                                         "-v", "-overwrite", winpath(wav), winpath(target)],
                                        env=env, stdout=log, stderr=log, timeout=180)
                log.seek(0)
                if result.returncode:
                    raise RuntimeError(f"TAKC failed for {name}: {log.read()}")
            assert target.read_bytes()[8] & 63 == 0, f"Not a legacy stream: {name}"
            for milliseconds in [0, 1, 53, 139]:
                result = subprocess.run([str(decoder), str(target), str(milliseconds)], capture_output=True)
                assert result.returncode == 0, f"{name}: {result.stderr.decode()}"
                offset = milliseconds * rate // 1000 * channels * 4
                assert result.stdout == expected[offset:], f"PCM mismatch: {name}, seek {milliseconds}ms"
            output.joinpath(name + ".s32le").write_bytes(expected)
            manifest[name] = dict(rate=rate, channels=channels, bits=bits, samples=count,
                                  preset=preset, signal=signal, codec=0, encoder="TAKC 1.0")
            print(f"PASS {name}: source PCM and four seek positions", flush=True)
    output.joinpath("legacy-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("encoder", type=Path)
    parser.add_argument("--decoder", type=Path, default=Path("target/release/examples/decode"))
    parser.add_argument("--output", type=Path, default=Path(__file__).parent / "fixtures")
    parser.add_argument("--matrix", action="store_true", help="Also verify 36 temporary legacy streams")
    args = parser.parse_args()
    subprocess.run(["wineserver", "-p60"], check=True)
    generate(args.encoder.resolve(), args.output, args.decoder.resolve())
    if args.matrix:
        rates = [8000, 11025, 12000, 22050, 44100, 48000, 96000]
        cases = []
        for bits in [8, 16, 24]:
            for channels in [1, 2]:
                for preset in ["0", "1", "2", "3", "4", "4m"]:
                    rate = rates[len(cases) % len(rates)]
                    name = f"legacy-matrix-{bits}-{channels}-{preset}-{rate}"
                    cases.append((name, rate, channels, bits, 17011, preset, "mixed"))
        with tempfile.TemporaryDirectory(prefix="fooyin-tak-legacy-matrix-") as tmp:
            generate(args.encoder.resolve(), Path(tmp), args.decoder.resolve(), cases)


if __name__ == "__main__":
    main()
