#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""Validate native executable headers before they enter release artifacts."""
from __future__ import annotations

import argparse
from pathlib import Path
import struct

TARGETS = {
    "x86_64-unknown-linux-gnu": ("elf", 62),
    "aarch64-unknown-linux-gnu": ("elf", 183),
    "x86_64-pc-windows-msvc": ("pe", 0x8664),
    "x86_64-apple-darwin": ("mach", 0x01000007),
    "aarch64-apple-darwin": ("mach", 0x0100000C),
}


def validate_binary(path: Path, target: str) -> None:
    kind, expected = TARGETS[target]
    with path.open("rb") as stream:
        header = stream.read(64)
        if kind == "elf":
            if len(header) < 64 or header[:4] != b"\x7fELF":
                raise ValueError(f"not an ELF executable: {path}")
            if header[4] != 2 or header[5] not in (1, 2) or header[6] != 1:
                raise ValueError(f"not a supported 64-bit ELF executable: {path}")
            endian = "<" if header[5] == 1 else ">"
            elf_type, machine, version = struct.unpack_from(endian + "HHI", header, 16)
            if machine != expected:
                raise ValueError(f"ELF architecture mismatch for {path}: expected {target}, got e_machine={machine}")
            if elf_type not in (2, 3) or version != 1:
                raise ValueError(f"ELF header is not an executable/shared object: {path}")
            return

        if kind == "pe":
            if len(header) < 64 or header[:2] != b"MZ":
                raise ValueError(f"not a PE executable: {path}")
            offset = struct.unpack_from("<I", header, 0x3C)[0]
            if offset > path.stat().st_size - 26 or offset < 64:
                raise ValueError(f"truncated or invalid PE header: {path}")
            stream.seek(offset)
            pe_header = stream.read(26)
            signature, machine, sections = struct.unpack_from("<4sHH", pe_header)
            optional_size, characteristics = struct.unpack_from("<HH", pe_header, 20)
            optional_magic = struct.unpack("<H", pe_header[24:26])[0]
            if signature != b"PE\0\0" or machine != expected:
                raise ValueError(f"PE architecture mismatch for {path}: expected {target}")
            if optional_magic != 0x20B or optional_size < 112 or sections == 0:
                raise ValueError(f"not a 64-bit PE32+ executable: {path}")
            if offset + 24 + optional_size > path.stat().st_size:
                raise ValueError(f"truncated PE optional header: {path}")
            if not characteristics & 0x0002:
                raise ValueError(f"PE image is not executable: {path}")
            return

        if len(header) < 32:
            raise ValueError(f"truncated Mach-O header: {path}")
        if header[:4] == b"\xcf\xfa\xed\xfe":
            endian = "<"
        elif header[:4] == b"\xfe\xed\xfa\xcf":
            endian = ">"
        else:
            raise ValueError(f"not a thin 64-bit Mach-O executable: {path}")
        _, cpu_type, _cpu_subtype, file_type = struct.unpack_from(endian + "IIII", header, 0)
        if cpu_type != expected:
            raise ValueError(f"Mach-O architecture mismatch for {path}: expected {target}, got cputype={cpu_type:#x}")
        if file_type != 2:
            raise ValueError(f"Mach-O image is not an executable: {path}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("target", choices=sorted(TARGETS))
    parser.add_argument("binaries", type=Path, nargs="+")
    args = parser.parse_args()
    for binary in args.binaries:
        try:
            validate_binary(binary, args.target)
        except (OSError, ValueError, struct.error) as error:
            parser.exit(1, f"native_binary: ERROR: {error}\n")


if __name__ == "__main__":
    main()
