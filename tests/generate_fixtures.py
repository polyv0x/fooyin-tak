#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate synthetic fixtures with TAKC 2.3.3 (only needed by maintainers).

Usage: python3 tests/generate_fixtures.py /path/to/Takc.exe [output_directory]
Requires Wine and ffmpeg. No encoder binary or third-party music is distributed.
WINEPREFIX can select an isolated, already initialized Wine prefix.
"""
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile


CASES = [
    ("mono8", 8000, 1, 8, 4109, "0", "mixed"),
    ("stereo8", 11025, 2, 8, 4517, "2", "mixed"),
    ("stereo16", 8000, 2, 16, 16752, "4m", "mixed"),
    ("surround24", 48000, 6, 24, 24321, "4m", "mixed"),
    ("silence16", 44100, 2, 16, 10007, "2", "silence"),
    ("stereo24", 96000, 2, 24, 20017, "4m", "mixed"),
    ("short24", 48000, 6, 24, 7, "2", "mixed"),
    ("negative16", 22050, 1, 16, 4017, "4m", "negative"),
]


def pcm(rate, channels, bits, count, signal):
    values = []
    state = 0x12345678
    scale = 1 << (bits - 1)
    for i in range(count):
        state = (1664525 * state + 1013904223) & 0xffffffff
        for ch in range(channels):
            if signal == "silence":
                value = 0
            elif signal == "negative":
                value = -scale
            else:
                value = int(scale * (0.43 * math.sin(i * 2 * math.pi * 123 / rate)
                                    + 0.22 * math.sin((i + ch * 3) * 2 * math.pi * 531 / rate)))
                if (i // 1024) % 4 == 2:
                    value = ((state >> ch) % (2 * scale)) - scale
                if (i // 1024) % 4 == 3:
                    value = (value >> 4) << 4
                if i % 997 == 0:
                    value = -scale if ch % 2 == 0 else scale - 1
            values.append(value)
    raw = b"".join((v + 128).to_bytes(1, "little") if bits == 8 else
                   v.to_bytes(bits // 8, "little", signed=True) for v in values)
    expected = b"".join(struct.pack("<i", v << (32 - bits)) for v in values)
    return raw, expected


def wave_bytes(rate, channels, bits, raw):
    stride = channels * (bits // 8)
    if channels > 2:
        # WAVEFORMATEXTENSIBLE with explicit speaker assignments.
        mask = (1 << channels) - 1
        fmt = struct.pack("<HHIIHHHHI", 0xfffe, channels, rate, rate * stride,
                          stride, bits, 22, bits, mask)
        fmt += bytes.fromhex("0100000000001000800000aa00389b71")
    else:
        fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * stride, stride, bits)
    payload = b"WAVEfmt " + struct.pack("<I", len(fmt)) + fmt
    payload += b"data" + struct.pack("<I", len(raw)) + raw
    if len(raw) & 1:
        payload += b"\0"
    return b"RIFF" + struct.pack("<I", len(payload)) + payload


def generate(encoder, output, cases=CASES):
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, WINEDEBUG="-all")
    # Keep the Wine server alive between encoder invocations.
    subprocess.run(["wineserver", "-p60"], env=env, check=True)
    manifest = {}
    with tempfile.TemporaryDirectory(prefix="fooyin-tak-fixtures-") as tmp:
        for name, rate, channels, bits, count, preset, signal in cases:
            raw, expected = pcm(rate, channels, bits, count, signal)
            wav = Path(tmp) / (name + ".wav")
            wav.write_bytes(wave_bytes(rate, channels, bits, raw))
            target = output.resolve() / (name + ".tak")
            winpath = lambda path: "Z:" + str(path.resolve()).replace("/", "\\")
            result = subprocess.run(["wine", str(encoder), "-e", "-p" + preset,
                                     "-md5", "-overwrite", "-silent", winpath(wav), winpath(target)],
                                    env=env, timeout=180)
            if result.returncode:
                raise RuntimeError(f"TAKC exited with status {result.returncode}: {name}")
            reference = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(target), "-f", "s32le", "-"])
            assert reference == expected, f"FFmpeg does not match source PCM: {name}"
            output.joinpath(name + ".s32le").write_bytes(expected)
            manifest[name] = dict(rate=rate, channels=channels, bits=bits, samples=count, preset=preset, signal=signal)
            print(f"Generated {name}: {channels}ch/{bits}bit/{rate}Hz, {count} samples", flush=True)
    output.joinpath("manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    generate(Path(sys.argv[1]).resolve(), Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).parent / "fixtures")
