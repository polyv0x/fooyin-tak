#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Generate synthetic WAV-format, speaker-layout and frame-boundary fixtures."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile

from generate_fixtures import pcm


CASES = [
    ("valid20-stereo", 48000, 2, 24, 20, 0x3, 1025, "4m"),
    ("quad-back", 48000, 4, 16, 16, 0x33, 1537, "2"),
    ("surround51-side", 48000, 6, 24, 24, 0x60f, 1025, "4m"),
    *[(f"boundary{count}", 8000, 1, 16, 16, 0x4, count, "0")
      for count in (1, 511, 512, 513)],
]


def generate(encoder, output):
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, WINEDEBUG="-all")
    subprocess.run(["wineserver", "-p60"], env=env, check=True)
    manifest = {}
    with tempfile.TemporaryDirectory(prefix="fooyin-tak-extended-") as tmp:
        for name, rate, channels, bits, valid_bits, mask, count, preset in CASES:
            raw, expected = pcm(rate, channels, bits, count, "mixed")
            if valid_bits < bits:
                values = [int.from_bytes(raw[i:i + bits // 8], "little", signed=True)
                          for i in range(0, len(raw), bits // 8)]
                values = [(value >> (bits - valid_bits)) << (bits - valid_bits) for value in values]
                raw = b"".join(value.to_bytes(bits // 8, "little", signed=True) for value in values)
                expected = b"".join(struct.pack("<i", value << (32 - bits)) for value in values)
            stride = channels * (bits // 8)
            fmt = struct.pack("<HHIIHHHHI", 0xfffe, channels, rate, rate * stride,
                              stride, bits, 22, valid_bits, mask)
            fmt += bytes.fromhex("0100000000001000800000aa00389b71")
            # Preserve both a nontrivial extensible header and a trailing RIFF chunk.
            tail = b"LIST" + struct.pack("<I", 24) + b"INFOISFT" + struct.pack("<I", 12) + b"TAK fixture\0"
            head = b"WAVEfmt " + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", len(raw))
            head = b"RIFF" + struct.pack("<I", len(head) + len(raw) + len(tail)) + head
            wav = Path(tmp) / (name + ".wav")
            wav.write_bytes(head + raw + tail)
            target = output.resolve() / (name + ".tak")
            winpath = lambda path: "Z:" + str(path.resolve()).replace("/", "\\")
            subprocess.run(["wine", str(encoder), "-e", "-p" + preset, "-fsl512", "-md5", "-v",
                            "-overwrite", "-silent", winpath(wav), winpath(target)],
                           env=env, timeout=180, check=True)
            reference = subprocess.check_output([
                "ffmpeg", "-v", "error", "-err_detect", "crccheck+explode",
                "-i", str(target), "-f", "s32le", "-"])
            if reference != expected:
                raise AssertionError(f"FFmpeg does not match source PCM: {name}")
            output.joinpath(name + ".s32le").write_bytes(expected)
            manifest[name] = dict(rate=rate, channels=channels, bits=bits, valid_bits=valid_bits,
                                  channel_mask=mask, samples=count, preset=preset, frame_limit=512,
                                  pcm_md5=hashlib.md5(raw).hexdigest(),
                                  wave_head=head.hex(), wave_tail=tail.hex())
            print(f"Generated {name}: {channels}ch, {valid_bits}/{bits}bit, {count} samples", flush=True)
    output.joinpath("manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("encoder", type=Path)
    parser.add_argument("--output", type=Path, default=Path(__file__).parent / "fixtures/extended")
    args = parser.parse_args()
    generate(args.encoder.resolve(), args.output)


if __name__ == "__main__":
    main()
