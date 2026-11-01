"""Build the release sidecar and stage it under .cache/bin.

Staging a copy keeps a bench run pinned to one binary even if someone
rebuilds `target/release` meanwhile, and never touches
`apps/desktop/src-tauri/binaries/` (whose copy can be months stale).
"""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

SIDECAR = "yapstack-transcription-sidecar"


def default_features() -> str:
    # Parakeet with the same EPs the app ships. Whisper is left out: it
    # needs cmake, and the bench rows are Parakeet variants.
    if sys.platform == "darwin":
        return "parakeet,coreml,webgpu"
    if sys.platform == "win32":
        return "parakeet,webgpu"
    return "parakeet"


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        while chunk := fh.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def build(repo: Path, dest: Path, features: str) -> Path:
    subprocess.run(
        ["cargo", "build", "--release", "-p", SIDECAR, "--features", features],
        cwd=repo,
        check=True,
    )
    exe = SIDECAR + (".exe" if sys.platform == "win32" else "")
    release = repo / "target" / "release"
    dest.mkdir(parents=True, exist_ok=True)
    staged = dest / exe
    shutil.copy2(release / exe, staged)
    if sys.platform == "darwin" and "webgpu" in features:
        shutil.copy2((release / "libwebgpu_dawn.dylib").resolve(), dest / "libwebgpu_dawn.dylib")
        rpaths = subprocess.run(["otool", "-l", str(staged)], capture_output=True, text=True, check=True).stdout
        if " path @executable_path " not in rpaths:
            subprocess.run(["install_name_tool", "-add_rpath", "@executable_path", str(staged)], check=True)
        # install_name_tool invalidates the ad-hoc signature; re-sign so
        # Apple Silicon will run it.
        subprocess.run(["codesign", "--force", "--sign", "-", str(staged)], check=True, capture_output=True)
    elif sys.platform == "win32":
        for dll in release.glob("*.dll"):
            shutil.copy2(dll, dest / dll.name)

    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True).stdout.strip()
    dirty = bool(subprocess.run(["git", "status", "--porcelain", "--", "crates", "Cargo.lock"], cwd=repo, capture_output=True, text=True).stdout.strip())
    info = {
        "commit": commit,
        "dirty": dirty,
        "features": features,
        "built_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "sha256": sha256(staged),
    }
    (dest / "BUILD_INFO.json").write_text(json.dumps(info, indent=1))
    print(f"staged {staged}\n{json.dumps(info, indent=1)}")
    return staged
