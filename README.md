# TAK for fooyin

A fooyin plugin for Tom's lossless Audio Kompressor (`.tak`) playback. Audio
decoding, checksums, and seeking run in native Rust, with no Cargo dependencies.
A small C++/Qt adapter connects the decoder to fooyin.

Supports TAK codec types 0 (legacy), 2 and 4, 8/16/24-bit integer PCM, mono, stereo and
multichannel audio, sample-accurate seeking, and CRC-24 verification of stream
information, frame headers and audio. Speaker positions and valid-bit depth are
preserved when specified. Readable files and synchronous sequential inputs are
supported, including streams whose length becomes known at the final frame.

The metadata reader combines exact duration, sample count, encoder settings and
audio properties with tags from fooyin's enabled metadata readers. APEv2 tags,
ReplayGain, CUE sheets and artwork continue to use those readers. The plugin
preserves existing CUE segment timing and does not edit tags.

fooyin's integrity checks can verify the stored whole-PCM MD5. The Rust library,
C API and standalone decoder also offer explicit damaged-frame recovery with
silence or skipping; ordinary playback uses strict error reporting.

See [format limits and validation](docs/validation.md) for details.

## Build

Requires fooyin 0.13.1+ with development headers and its CMake package, Qt 6
development files, a C++23 compiler, CMake 3.25+, Ninja, and Rust 1.74+.
Qt 6.4+ is required on Linux/macOS; Windows requires Qt 6.8+ and MSVC.
Build against your installed fooyin version, Qt version, and architecture.
If building fooyin from source, enable its `INSTALL_HEADERS` CMake option.

```sh
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

Output: **`build/fyplugin_tak.so`** on Linux/macOS or
**`build/fyplugin_tak.dll`** on Windows. If CMake cannot find fooyin, add
`-DFooyin_DIR=/path/to/lib/cmake/fooyin` to the first command. Rust builds offline.
On Windows, use an MSVC developer shell with Qt on the CMake prefix path and
the matching Rust MSVC toolchain.

## Install

Download the archive for your platform from a successful
[Actions run](https://github.com/polyv0x/fooyin-tak/actions/workflows/build.yml)
and extract it, or build the plugin above. Version tags also publish tested
archives on [Releases](https://github.com/polyv0x/fooyin-tak/releases).

The build workflow targets fooyin **0.13.1**:

| Platform | Architecture | Build baseline |
| --- | --- | --- |
| Linux | x86-64, ARM64 | Debian 12, Qt 6.4 |
| Windows | x64 | MSVC, Qt 6.8.3 |
| macOS (experimental) | Apple Silicon | macOS 14+, Homebrew Qt/LLVM |

`BUILDINFO.txt` in each archive records its exact build versions. macOS builds
need a fooyin installation with matching Qt and C++ runtime dependencies.
Windows binaries are unsigned; macOS binaries use an ad-hoc signature without
notarization. Build from source if an archive does not match your installation.

1. In fooyin's plugin settings, choose **Install Plugin** and select the
   extracted `fyplugin_tak.so` or `fyplugin_tak.dll`.
2. Restart fooyin and enable **TAK** in the input decoder settings. Give it
   priority for `.tak` files if another decoder handles them.

For CUE duration and waveform support, enable **TAK** in the metadata reader
list too. Reload or rescan affected CUE files and regenerate cached waveforms
if their duration was previously missing.

For a system installation, run `sudo cmake --install build`. A custom plugin
destination can be configured with `-DTAK_PLUGIN_INSTALL_DIR=/path/to/plugins`.

## Development

Run the Rust and plugin tests after building:

```sh
ctest --test-dir build --output-on-failure
cargo clippy --locked --offline --all-targets -- -D warnings
cargo fmt --all --check
```

The [standalone decoder](examples/decode.rs) writes raw signed 32-bit
little-endian PCM and checks stored MD5 by default. An optional second argument
sets the seek position in milliseconds; `-` reads stdin and `--status` reports
verification and encoder information on stderr.

```sh
cargo run --release --offline --example decode -- --status album.tak > album.s32le
```

CI runs native Rust and Qt plugin tests on every platform and tests each staged
plugin before archiving it. Linux also checks the minimum Rust version.
[Validation notes](docs/validation.md) describe synthetic fixtures, differential
comparisons and optional encoder matrices. Wine and the official TAK encoder
are only fixture-generation tools, never playback dependencies.

## License

By **polyv0x**, under [GPL-3.0-or-later](LICENSE). The Rust reconstruction is
adapted from Paul B Mahol's [FFmpeg TAK implementation](https://github.com/FFmpeg/FFmpeg/tree/n8.0/libavcodec),
originally LGPL-2.1-or-later, and the GPL-3.0-or-later
[Librempeg legacy decoder](https://github.com/librempeg/librempeg/blob/d8566dbd63453903c80cfe28d72583d1fe2b386d/libavcodec/takdec.c).
See the [FFmpeg attribution](LICENSES/ffmpeg-tak.txt),
[Librempeg attribution](LICENSES/librempeg-tak.txt) and
[LGPL text](LICENSES/LGPL-2.1.txt). Include these license files when distributing
the plugin. No encoder binaries or third-party music are included.
