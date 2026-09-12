#!/usr/bin/env python3
"""Safely extract a core release archive for runnable-tree manifest creation."""

from __future__ import annotations

import shutil
import stat
import sys
import tarfile
import zipfile
from pathlib import Path, PurePosixPath

MAX_UNCOMPRESSED_BYTES = 2 * 1024 * 1024 * 1024


def safe_name(raw: str, seen: set[str]) -> PurePosixPath:
    if not raw or "\0" in raw or "\\" in raw:
        raise ValueError(f"unsafe archive path: {raw!r}")
    path = PurePosixPath(raw)
    parts = tuple(part for part in path.parts if part != ".")
    if path.is_absolute() or not parts or any(part in ("", "..") for part in parts):
        raise ValueError(f"unsafe archive path: {raw!r}")
    normalized = PurePosixPath(*parts)
    key = normalized.as_posix().casefold()
    if key in seen:
        raise ValueError(f"duplicate archive path: {normalized}")
    seen.add(key)
    return normalized


def output_path(root: Path, relative: PurePosixPath) -> Path:
    target = root.joinpath(*relative.parts)
    target.parent.mkdir(parents=True, exist_ok=True)
    if root.resolve() not in target.resolve().parents:
        raise ValueError(f"archive path escapes destination: {relative}")
    return target


def copy_bounded(source, target: Path, size: int, total: list[int]) -> None:
    total[0] += size
    if size <= 0 or total[0] > MAX_UNCOMPRESSED_BYTES:
        raise ValueError("core archive has an invalid or excessive uncompressed size")
    with target.open("wb") as destination:
        shutil.copyfileobj(source, destination, length=1024 * 1024)


def extract_zip(archive: Path, root: Path) -> None:
    seen: set[str] = set()
    total = [0]
    with zipfile.ZipFile(archive) as source:
        for info in source.infolist():
            raw_name = info.filename.rstrip("/")
            if info.is_dir() and raw_name in ("", "."):
                continue
            relative = safe_name(raw_name, seen)
            mode = info.external_attr >> 16
            if stat.S_ISLNK(mode):
                raise ValueError(f"core archive contains a symbolic link: {relative}")
            target = output_path(root, relative)
            if info.is_dir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            file_type = stat.S_IFMT(mode)
            if file_type and file_type != stat.S_IFREG:
                raise ValueError(f"core archive contains a non-regular file: {relative}")
            with source.open(info) as entry:
                copy_bounded(entry, target, info.file_size, total)
            if mode & 0o111:
                target.chmod(target.stat().st_mode | 0o755)


def extract_tar(archive: Path, root: Path) -> None:
    seen: set[str] = set()
    total = [0]
    with tarfile.open(archive, mode="r:gz") as source:
        for member in source:
            raw_name = member.name.rstrip("/")
            if member.isdir() and raw_name in ("", "."):
                continue
            relative = safe_name(raw_name, seen)
            target = output_path(root, relative)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(f"core archive contains a link or special file: {relative}")
            entry = source.extractfile(member)
            if entry is None:
                raise ValueError(f"cannot read core archive entry: {relative}")
            with entry:
                copy_bounded(entry, target, member.size, total)
            if member.mode & 0o111:
                target.chmod(target.stat().st_mode | 0o755)


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: extract-core-tree.py ARCHIVE DESTINATION", file=sys.stderr)
        return 2
    archive = Path(sys.argv[1]).resolve()
    root = Path(sys.argv[2]).resolve()
    root.mkdir(parents=True, exist_ok=True)
    lower = archive.name.lower()
    if lower.endswith(".zip"):
        extract_zip(archive, root)
    elif lower.endswith((".tar.gz", ".tgz")):
        extract_tar(archive, root)
    else:
        raise ValueError(f"unsupported core archive: {archive.name}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
