#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Keep Cargo, CMake, the lockfile and release tags in agreement."""
import os
from pathlib import Path
import re
import tomllib

root = Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
assert re.search(r"project\(fooyin_tak VERSION " + re.escape(version) + r"\s", (root / "CMakeLists.txt").read_text())
lock = tomllib.loads((root / "Cargo.lock").read_text())
assert next(p["version"] for p in lock["package"] if p["name"] == "fooyin-tak") == version
if os.environ.get("GITHUB_REF_TYPE") == "tag":
    assert os.environ["GITHUB_REF_NAME"] == "v" + version, "Tag must match the package version"
print(f"Version {version} is consistent")
