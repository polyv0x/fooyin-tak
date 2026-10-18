#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Stage, test and archive a native Release build, including license notices."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("build", type=Path)
    parser.add_argument("system", choices=["linux", "windows", "macos"])
    parser.add_argument("arch", choices=["x86_64", "aarch64"])
    parser.add_argument("fooyin_version")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    build = args.build.resolve()
    native_system = {"Linux": "linux", "Windows": "windows", "Darwin": "macos"}[platform.system()]
    native_arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(platform.machine().lower(), platform.machine().lower())
    if (args.system, args.arch) != (native_system, native_arch):
        raise ValueError("Archives must be built and tested on their native platform")
    # A relative destination keeps staging independent of the SDK installation.
    cache = (build / "CMakeCache.txt").read_text()
    if not any(line.startswith("TAK_PLUGIN_INSTALL_DIR:") and line.endswith("=plugins") for line in cache.splitlines()):
        raise ValueError("Configure with -DTAK_PLUGIN_INSTALL_DIR=plugins before packaging")
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    name = f"fooyin-tak-{version}-{args.system}-{args.arch}-fooyin-{args.fooyin_version}"
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="tak-package-", dir=build) as temporary:
        temporary = Path(temporary)
        stage = temporary / "stage"
        command = ["cmake", "--install", str(build), "--config", "Release", "--prefix", str(stage)]
        if args.system != "windows":
            command.append("--strip")
        subprocess.run(command, check=True)
        suffix = ".dll" if args.system == "windows" else ".so"
        plugin = stage / "plugins" / ("fyplugin_tak" + suffix)
        package = temporary / name
        package.mkdir()
        packaged_plugin = package / plugin.name
        shutil.copy2(plugin, packaged_plugin)
        if args.system == "macos":
            subprocess.run(["codesign", "--force", "--sign", "-", str(packaged_plugin)], check=True)
        executable = "tak_plugin_test" + (".exe" if args.system == "windows" else "")
        test = build / executable
        if not test.exists():
            test = build / "Release" / executable
        subprocess.run([str(test), str(packaged_plugin), str(root / "tests/fixtures")],
                       env=dict(os.environ, QT_QPA_PLATFORM="offscreen"), check=True)
        for source in ["LICENSE", "README.md"]:
            shutil.copy2(root / source, package / source)
        shutil.copytree(root / "LICENSES", package / "LICENSES")
        info = (build / "build-info-Release.txt").read_text()
        commit = os.environ.get("GITHUB_SHA") or subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
        info += f"Source commit: {commit}\nFooyin release: {args.fooyin_version}\n"
        info += subprocess.check_output(["rustc", "--version"], text=True)
        (package / "BUILDINFO.txt").write_text(info)
        kind = "zip" if args.system == "windows" else "gztar"
        archive = Path(shutil.make_archive(str(dist / name), kind, root_dir=temporary, base_dir=name))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n")
    print(f"Created and tested {archive}")


if __name__ == "__main__":
    main()
