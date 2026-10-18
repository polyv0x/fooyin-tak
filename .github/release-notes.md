Native Rust TAK decoding for fooyin 0.13.1, including legacy codec type 0.

Download the archive matching your operating system and architecture, extract
it, and install `fyplugin_tak.so` (Linux/macOS) or `fyplugin_tak.dll` (Windows)
through fooyin's **Install Plugin** button. Restart fooyin after installation.
Keep the included license notices when redistributing the plugin.

Linux builds use Debian 12 and Qt 6.4. Windows x64 builds use MSVC and Qt 6.8.3.
The macOS Apple Silicon build targets macOS 14+ and is experimental; use a
fooyin build with matching Qt and C++ runtime dependencies. Windows binaries
are unsigned; macOS binaries use an ad-hoc signature, without notarization.

Every archive passes native decoder tests and the fooyin plugin integration
test, including loading the staged plugin that is packaged. `BUILDINFO.txt`
records the source revision and build versions. `SHA256SUMS` contains the
archive checksums.
