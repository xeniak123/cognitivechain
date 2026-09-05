#!/usr/bin/env python3
"""Package cog-miner into a single self-contained executable for end users.

Produces `dist/cog-miner.exe` on Windows and `dist/cog-miner` on Linux, so a
miner only has to download one file and run:

    cog-miner --wallet <ADDRESS> --pool <NODE_IP>

Run this on the target OS -- PyInstaller does not cross-compile.

    python -m pip install pyinstaller
    python build_release.py            # CPU build, ~40 MB
    python build_release.py --gpu      # bundles CUDA PyTorch, ~2.5 GB

The CPU build is the right default for distribution: it works everywhere, and
users with an NVIDIA card get the GPU path automatically once they install
PyTorch themselves (the miner detects it at runtime).
"""

from __future__ import annotations

import argparse
import platform
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def run(cmd: list[str]) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.check_call(cmd, cwd=ROOT)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--gpu",
        action="store_true",
        help="bundle PyTorch so the executable ships with CUDA support",
    )
    parser.add_argument(
        "--clean",
        action="store_true",
        help="remove build/ and dist/ before building",
    )
    args = parser.parse_args()

    try:
        import PyInstaller  # noqa: F401
    except ImportError:
        print(
            "error: PyInstaller is not installed.\n"
            "       python -m pip install pyinstaller",
            file=sys.stderr,
        )
        return 1

    if args.clean:
        for folder in ("build", "dist"):
            shutil.rmtree(ROOT / folder, ignore_errors=True)

    cmd = [
        sys.executable,
        "-m",
        "PyInstaller",
        "--onefile",
        "--name",
        "cog-miner",
        "--console",
        "--hidden-import",
        "blake3",
        "--collect-binaries",
        "blake3",
    ]
    if args.gpu:
        cmd += ["--collect-all", "torch"]
    else:
        # Keep the CPU build small: torch is optional and detected at runtime.
        cmd += ["--exclude-module", "torch"]
    cmd += [str(ROOT / "cog_miner" / "__main__.py")]

    run(cmd)

    suffix = ".exe" if platform.system() == "Windows" else ""
    artifact = ROOT / "dist" / f"cog-miner{suffix}"
    if not artifact.exists():
        print("error: PyInstaller did not produce the expected artifact", file=sys.stderr)
        return 1

    size_mb = artifact.stat().st_size / (1024 * 1024)
    print()
    print(f"built {artifact}  ({size_mb:.1f} MB, {platform.system()} {platform.machine()})")
    print("Ship this single file. Users run:")
    print(f"  {artifact.name} --wallet <COG_ADDRESS> --pool <NODE_IP>")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
