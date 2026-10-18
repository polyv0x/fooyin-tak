# Decoder validation


```sh
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo fmt --all --check
cargo run --release --offline --example decode -- album.tak > album.s32le
```

The example writes interleaved, full-scale signed 32-bit little-endian PCM. Its
optional second argument seeks in milliseconds. The Rust library API returns
right-aligned signed samples; the C ABI and fooyin plugin return full-scale S32.
The C ABI ownership and callback contract are in [include/fooyin_tak.h](../include/fooyin_tak.h).

The checked-in synthetic fixtures need no encoder or FFmpeg to run. They cover
mono/stereo/5.1, three PCM precisions, silence, full-scale negative values, short
frames, and different compression settings. Tests also exercise fresh and cached
seeks, short reads, truncation, corrupt entropy data, callback failures, and the
actual Qt plugin through fooyin's loader, including CUE scans and cancellation.

For optional differential testing against FFmpeg (or the original source PCM
for legacy fixtures, since FFmpeg does not decode codec type 0):

```sh
cargo build --release --offline --example decode
python3 tests/differential.py
python3 tests/differential.py /path/to/more/*.tak
```

To regenerate fixtures or run the additional 54-file encoder matrix, obtain
[TAK 2.3.3](http://www.thbeck.de/Download/TAK_2.3.3.zip) and install Wine and
FFmpeg. Use `WINEPREFIX` to select an initialized Wine prefix if desired:

```sh
python3 tests/generate_fixtures.py /path/to/Applications/Takc.exe
python3 tests/differential.py --encoder /path/to/Applications/Takc.exe
```

The matrix tests presets 0, 1, 2, 3, 4 and 4m, 8/16/24-bit PCM, 1/2/6 channels,
and rates from 8 to 192 kHz. Each generated stream is checked against source PCM
and FFmpeg, including four seek positions. Wine and the proprietary encoder are
only fixture-generation tools, never playback dependencies.

Legacy fixtures use [TAKC 1.0](https://www.rarewares.org/files/lossless/Tak1.0.zip),
with its built-in encoding verification enabled. To regenerate them and test
an additional 36 combinations of presets, precisions, channel counts and rates:

```sh
python3 tests/generate_legacy_fixtures.py /path/to/TAK-1.0/Takc.exe --matrix
```

These cases compare full decoded output and seeks with the original synthetic
PCM. The ordinary Rust and Qt integration tests cover the saved legacy fixtures
alongside the modern ones.


## Format limits

Input must be readable and seekable. Codec type 1, non-PCM data, in-frame
metadata, and streams with unknown length and no last-frame metadata are
rejected explicitly. The optional whole-stream MD5 is not verified; CRC-24
checks cover stream information, frame headers and decoded frames.

Playback retains one decoded frame and builds a seek index as needed. The
first seek into unvisited audio scans frame headers.
