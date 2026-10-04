#!/usr/bin/env python3
"""Reject incomplete or corrupt Windows + macOS releases before publication."""

import argparse
import hashlib
from pathlib import Path


PACKAGES = (
    "zapret-ui.exe",
    "zapret-ui-macos-arm64.zip",
    "zapret-ui-macos-arm64.dmg",
)
ASSETS = tuple(name for package in PACKAGES for name in (package, package + ".sha256"))


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def verify(directory, reference=None):
    directory = Path(directory)
    actual = {path.name for path in directory.iterdir()}
    expected = set(ASSETS)
    if actual != expected:
        raise ValueError(
            f"Expected exactly the six release assets; "
            f"missing={sorted(expected - actual)}, unexpected={sorted(actual - expected)}"
        )
    for name in ASSETS:
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"Asset must be a nonempty regular file: {name}")
    for name in PACKAGES:
        expected_line = f"{digest(directory / name)}  {name}"
        checksum = (directory / (name + ".sha256")).read_text(encoding="ascii").strip()
        if checksum != expected_line:
            raise ValueError(f"Invalid SHA-256 checksum or filename: {name}")
    if reference is not None:
        reference = Path(reference)
        verify(reference)
        for name in ASSETS:
            if digest(directory / name) != digest(reference / name):
                raise ValueError(f"Uploaded asset differs from the checked build: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--reference", type=Path, help="Compare uploaded bytes with checked CI assets")
    args = parser.parse_args()
    try:
        verify(args.directory, args.reference)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Release validation failed: {error}\n")
    print("Verified all six Windows + macOS release assets and SHA-256 checksums.")


if __name__ == "__main__":
    main()
