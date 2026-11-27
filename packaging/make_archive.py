#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""Create deterministic CLI archives plus per-artifact SHA-256 sidecars."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
from pathlib import Path
import re
import struct
import sys
import tarfile
import zipfile

sys.dont_write_bytecode = True
from native_binary import TARGETS, validate_binary

DOCS = ("README.md", "LICENSE", "spec/DFYP-v1.md", "deploy/README.md")


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def add_tar(bundle: tarfile.TarFile, source: Path, name: str, mode: int) -> None:
    data = source.read_bytes()
    info = tarfile.TarInfo(name)
    info.size, info.mode, info.mtime = len(data), mode, 0
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    bundle.addfile(info, io.BytesIO(data))


def write_tar(path: Path, files: list[tuple[Path, str, int]]) -> None:
    with path.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as bundle:
                for source, name, mode in files:
                    add_tar(bundle, source, name, mode)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary-dir", type=Path, required=True)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?", version):
        parser.error("invalid semantic version")
    windows = args.target.endswith("windows-msvc")
    names = ("dfy.exe", "dfy-relay.exe") if windows else ("dfy", "dfy-relay")
    for name in names:
        binary = args.binary_dir / name
        if not binary.is_file():
            parser.error(f"missing binary: {binary}")
        try:
            validate_binary(binary, args.target)
        except (OSError, ValueError, struct.error) as error:
            parser.error(str(error))
    root = Path(__file__).resolve().parent.parent
    for name in DOCS:
        if not (root / name).is_file():
            parser.error(f"missing document: {root / name}")
    args.output_dir.mkdir(parents=True, exist_ok=True)

    suffix = "zip" if windows else "tar.gz"
    archive = args.output_dir / f"dafeiyu-{version}-{args.target}.{suffix}"
    files: list[tuple[Path, str, int]] = []
    for name in names:
        files.append((args.binary_dir / name, name, 0o755))
    for source in DOCS:
        archive_name = "relay-deployment.md" if source == "deploy/README.md" else Path(source).name
        files.append((root / source, archive_name, 0o644))

    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as bundle:
            for source, name, mode in files:
                info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (0o100000 | mode) << 16
                bundle.writestr(info, source.read_bytes())
    else:
        write_tar(archive, files)

    artifacts = [archive]
    if args.target.startswith(("x86_64-unknown-linux", "aarch64-unknown-linux")):
        arch = args.target.split("-", 1)[0]
        relay_archive = args.output_dir / f"dfy-relay-linux-{arch}.tar.gz"
        write_tar(relay_archive, [(args.binary_dir / "dfy-relay", "dfy-relay", 0o755)])
        artifacts.append(relay_archive)
    for artifact in artifacts:
        sidecar = artifact.with_name(artifact.name + ".sha256")
        sidecar.write_text(f"{digest(artifact)}  {artifact.name}\n", encoding="ascii")
        print(artifact)


if __name__ == "__main__":
    main()
