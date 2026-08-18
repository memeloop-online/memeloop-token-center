#!/usr/bin/env python3
"""Fail if CPA upstream fixtures appear to contain non-synthetic material."""

from __future__ import annotations

import json
import pathlib
import re
import sys
from typing import Any

import yaml


ROOT = pathlib.Path(__file__).resolve().parents[1] / "fixtures" / "cpa-upstreams"
SENSITIVE_KEYS = {
    "api-key",
    "api_key",
    "access_token",
    "refresh_token",
    "id_token",
    "credential",
    "handle",
}
EMAIL = re.compile(r"^[^@\s]+@example\.test$")
FORBIDDEN_PATTERNS = (
    re.compile(r"\bgh[opusr]_[A-Za-z0-9]{16,}\b"),
    re.compile(r"\bsk-[A-Za-z0-9_-]{16,}\b"),
    re.compile(r"\beyJ[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}"),
)


def inspect(value: Any, key: str | None = None) -> None:
    if isinstance(value, dict):
        for child_key, child_value in value.items():
            if not isinstance(child_key, str):
                raise ValueError("fixture contains a non-string object key")
            inspect(child_value, child_key)
        return
    if isinstance(value, list):
        for child in value:
            inspect(child, key)
        return
    if not isinstance(value, str):
        return
    if key in SENSITIVE_KEYS and not (
        value.startswith("fixture-only-") or value.startswith("Fixture")
    ):
        raise ValueError("fixture contains a non-synthetic sensitive value")
    if key in {"email", "login"} and not EMAIL.fullmatch(value):
        raise ValueError("fixture contains a non-example email address")
    if key in {"base-url", "base_url"}:
        if ".example.test" not in value:
            raise ValueError("fixture contains a non-example upstream URL")


def main() -> int:
    files = sorted(path for path in ROOT.rglob("*") if path.is_file())
    if not files:
        raise ValueError("CPA upstream fixture set is empty")
    for path in files:
        raw = path.read_text(encoding="utf-8")
        for pattern in FORBIDDEN_PATTERNS:
            if pattern.search(raw):
                raise ValueError("fixture contains credential-like material")
        if path.suffix == ".json":
            value = json.loads(raw)
        elif path.suffix in {".yaml", ".yml"}:
            value = yaml.safe_load(raw)
        else:
            raise ValueError("fixture set contains an unsupported file")
        inspect(value)
    print(f"CPA upstream fixture sanitizer: PASS files={len(files)}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError, yaml.YAMLError) as error:
        print(f"CPA upstream fixture sanitizer: FAIL {error}", file=sys.stderr)
        raise SystemExit(1)
