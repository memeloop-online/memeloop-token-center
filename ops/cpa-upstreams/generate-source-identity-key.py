#!/usr/bin/env python3
"""Create a versioned source-identity key without exposing key material."""

from __future__ import annotations

import argparse
import os
import pathlib
import secrets
import stat
import sys
from typing import NoReturn


KEY_PREFIX = b"MTC-SOURCE-ID-KEY\0\x01"
KEY_PAYLOAD_BYTES = 32


class GenerationFailure(RuntimeError):
    """A deliberately path- and secret-free operator-facing failure."""


def open_safe_parent(target: pathlib.Path) -> int:
    if (
        not target.is_absolute()
        or target == pathlib.Path("/")
        or any(part in {"", ".", ".."} for part in target.parts[1:])
    ):
        raise GenerationFailure("target key file path must be absolute and normalized")
    flags = (
        os.O_RDONLY
        | os.O_DIRECTORY
        | os.O_NOFOLLOW
        | getattr(os, "O_CLOEXEC", 0)
    )
    try:
        descriptor = os.open("/", flags)
    except OSError as error:
        raise GenerationFailure("target parent directory is not safe") from error
    try:
        for component in target.parent.parts[1:]:
            try:
                child = os.open(component, flags, dir_fd=descriptor)
            except OSError as error:
                raise GenerationFailure("target parent directory is not safe") from error
            os.close(descriptor)
            descriptor = child
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) & 0o022
        ):
            raise GenerationFailure("target parent directory is not safe")
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def write_all(descriptor: int, document: bytearray) -> None:
    view = memoryview(document)
    try:
        offset = 0
        while offset < len(view):
            written = os.write(descriptor, view[offset:])
            if written <= 0:
                raise OSError("short write")
            offset += written
    finally:
        view.release()


def generate(path_value: str) -> None:
    target = pathlib.Path(path_value)
    parent_descriptor = open_safe_parent(target)
    descriptor: int | None = None
    created = False
    document = bytearray(KEY_PREFIX)
    document.extend(secrets.token_bytes(KEY_PAYLOAD_BYTES))
    try:
        flags = (
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | os.O_NOFOLLOW
            | getattr(os, "O_CLOEXEC", 0)
        )
        try:
            descriptor = os.open(
                target.name, flags, 0o600, dir_fd=parent_descriptor
            )
            created = True
            os.fchmod(descriptor, 0o600)
            metadata = os.fstat(descriptor)
            if (
                not stat.S_ISREG(metadata.st_mode)
                or stat.S_IMODE(metadata.st_mode) != 0o600
                or metadata.st_uid != os.geteuid()
                or metadata.st_nlink != 1
            ):
                raise GenerationFailure("target key file could not be created safely")
            write_all(descriptor, document)
            os.fsync(descriptor)
            os.close(descriptor)
            descriptor = None
            os.fsync(parent_descriptor)
        except GenerationFailure:
            raise
        except OSError as error:
            raise GenerationFailure("target key file could not be created safely") from error
    except BaseException:
        if descriptor is not None:
            os.close(descriptor)
        if created:
            try:
                os.unlink(target.name, dir_fd=parent_descriptor)
                os.fsync(parent_descriptor)
            except OSError:
                pass
        raise
    finally:
        document.clear()
        os.close(parent_descriptor)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate a protected CPA migration source-identity key."
    )
    parser.add_argument(
        "output",
        help="New absolute output path in a current-user-owned safe directory.",
    )
    return parser.parse_args(argv)


def fail(message: str) -> NoReturn:
    print(f"Source identity key generation stopped: {message}", file=sys.stderr)
    raise SystemExit(2)


if __name__ == "__main__":
    try:
        arguments = parse_args(sys.argv[1:])
        generate(arguments.output)
    except GenerationFailure as error:
        fail(str(error))
