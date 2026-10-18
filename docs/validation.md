# Decoder validation

```sh
cargo test --locked --offline
cargo +1.74.0 test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo fmt --all --check
cargo run --release --offline --example decode -- album.tak > album.s32le
```

The example writes interleaved, full-scale signed 32-bit little-endian PCM. Its
optional second argument seeks in milliseconds. `-` reads a blocking stdin
stream; `--status` writes metadata and integrity status to stderr. The Rust
library returns right-aligned signed samples; the C ABI and fooyin plugin return
full-scale S32.
The C ABI ownership and callback contract are in [include/fooyin_tak.h](../include/fooyin_tak.h).

The checked-in synthetic fixtures need no encoder or FFmpeg to run. They cover
mono/stereo/5.1, three PCM precisions, silence, full-scale negative values, short
frames, and different compression settings. Tests also exercise fresh and cached
seeks, short reads, truncation, corrupt entropy data, callback failures, and the
actual Qt plugin through fooyin's loader, including CUE scans and cancellation.

Additional cases cover unknown-length pipes, failed seek callbacks, MD5 states,
damaged-frame recovery, metadata limits, false sync candidates, unsupported
format changes, and missing or damaged final frames.

The extensible WAV fixtures cover 20 valid bits in a 24-bit container,
four-channel rear speakers, six-channel side speakers, preserved trailing RIFF
chunks, and lengths of 1, 511, 512 and 513 samples with a 512-sample frame limit.
Their source PCM and MD5 values are recorded in the fixture manifest. Regenerate
them with the official encoder:

```sh
python3 tests/generate_extended_fixtures.py /path/to/Applications/Takc.exe
```

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

The synthetic TAK 1.0 seek-table fixture contains 800,000 silent mono samples.
Tests authenticate all 106 table targets against frame-header CRCs, compare its
entire PCM output, and check that a distant seek reads fewer than 4,096 input
bytes. Fixture provenance and regeneration are in
[tests/fixtures/README.md](../tests/fixtures/README.md).

Version 0.2.0 validation also compared 54 modern, 36 legacy and seven extended
encoder cases against source PCM, with four seek positions each; modern cases
also matched FFmpeg. A separate official TAKC stdin capture retained the initial
unknown-length metadata and subsequent completed audio. This checks the actual
streaming layout, where padding reserves space for final-frame and MD5 metadata.
The completed-file and sequential outputs matched all 384,000 original stereo
sample frames.

## Metadata in fooyin

fooyin accepts the first successful metadata reader, rather than merging all
readers. The TAK reader asks the shared AudioLoader for enabled peer readers in
their configured order, reads tags into a temporary Track, and then applies
native TAK audio properties. Failed or recursive readers cannot replace the
caller's track. Existing bounded CUE/chapter timing is preserved.

Archive members use the supplied device and member filename; the archive wrapper
is not reopened. Borrowed devices stay open and their position is restored after
tag probes. Artwork still falls through to fooyin's existing cover readers.
Encoder version/preset/evaluation, codec number, stored MD5 availability and
differing valid-bit depth are exposed as technical properties.

Qt integration tests exercise enabled-reader ordering, failure rollback, direct
and AudioLoader-level recursion, archives, CUE timing, cancellation and device
ownership. An external harness against the installed FFmpeg reader also checked
APEv2 title, multivalue artist/custom fields, ReplayGain, embedded CUESHEET and
artwork on synthetic codec 0 and codec 2 files. That harness uses no private
fooyin headers in the plugin itself.

## Verification and recovery

Frame/header CRC checks always run. Rust `DecoderOptions` and C `TakOptions`
default to strict decoding with stored whole-PCM MD5 verification enabled.
fooyin enables MD5 when the host requests `VerifyIntegrity`; regular playback
uses CRC checks. `--no-verify-md5` disables only MD5 in the command-line decoder.

Verification reports one of `NotPresent`, `Disabled`, `Pending`, `Verified`,
`Mismatch` or `Incomplete`. A complete uninterrupted traversal is required for
`Verified`. A nontrivial seek or recovered damage invalidates an enabled MD5
traversal; seeking to sample zero resets the digest and damage ranges. A no-op
seek preserves verification. MD5 mismatch returns an error, including when the
final output buffer has already been filled: callers must discard output from
failed reads. The C API latches errors until a successful seek.

Recovery is opt-in through Rust/C options or `--recovery silence|skip`:

```sh
target/release/examples/decode --recovery silence --status damaged.tak > recovered.s32le
cat album.tak | target/release/examples/decode --status - > album.s32le
```

Silence preserves the original timeline; skipping omits damaged sample ranges.
Both report those ranges in original sample-frame coordinates and never report
an interrupted MD5 traversal as verified. The CLI reports recovered damage even
without `--status`. The fooyin adapter uses strict recovery because its public
decoder options provide no recovery setting.

Recovery requires CRC-authenticated subsequent frame headers. Ordinary I/O
errors, cancellation, resource limits and authenticated unsupported format
changes remain errors. A damaged ending can be concealed only when its duration
is known. Unknown-length input ending without a valid final frame is an error.

## Resource limits

Sequential decoding keeps one decoded frame and bounded buffering; it does not
collect the entire input. Metadata scanning is limited to 64 MiB and 1,024
blocks, with at most 32 MiB retained. Encoded frames and each recovery scan are
limited to 16 MiB. The lazy frame index and legacy seek hints each retain at
most 8,192 entries; damage reporting retains at most 4,096 merged ranges.

Seekable inputs use authenticated legacy seek-table entries when available and
scan frame headers for unindexed positions. Unknown lengths are resolved from
valid final-frame information or by scanning seekable input; sequential inputs
learn their final sample count during playback. Synchronous reads must return
zero only at EOF. Arbitrary nonblocking/live network input is not advertised by
the fooyin adapter. Metadata probes require a seekable device so they can
restore its position without consuming audio intended for playback.

## Format limits

Released integer PCM codec types 0, 2 and 4 are supported. Reference encoder
coverage uses TAK 1.0 and 2.3.3, with 8/16/24-bit PCM and up to six channels.
The 2.3.3 encoder itself limits multichannel audio to six channels. Preserved
WAV header/trailer bytes are accessible through Rust metadata and the C API;
they are not decoded as audio or interpreted as tags.

The official [TAK 2.3.3 documentation](http://www.thbeck.de/Download/TAK_2.3.3.zip)
(`Applications/Readme.html`, codec values) identifies codec 1 as experimental
and codec 3 as the TAK 2.1 beta lossWAV variant. These were investigated but no
validated implementation or reference fixtures were available; both remain
explicitly unsupported. Non-PCM data and in-frame metadata are also rejected.
The decoder does not encode audio or edit tags.
