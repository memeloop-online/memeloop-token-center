#!/usr/bin/env python3
"""Export a replay-safe cpa-session-archive delta through CPA/API2.

The upstream collector exposes only whole-session exports. This driver prefers a
snapshot-bound stable cursor and falls back to the bounded legacy projection only
when its overlap window is provably complete. It never logs the management
credential, snapshot, export ticket, session id, or archived payload.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import datetime as dt
import hashlib
import http.client
import json
import os
import sqlite3
import stat
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO, Iterable


SOURCE_FINGERPRINT_VERSION = 1
CHECKPOINT_VERSION = 2
MANIFEST_VERSION = 2
SESSIONS_PATH = "/v0/management/plugins/cpa-session-archive/sessions"
EXPORT_PATH = "/v0/management/plugins/cpa-session-archive/export"
STATS_PATH = "/v0/management/plugins/cpa-session-archive/stats"
STABLE_CURSOR_PROTOCOL = "session-snapshot-cursor-v1"
LEGACY_PROJECTION_PROTOCOL = "legacy-last-at-limit-v1"
MAX_SESSION_COUNT = 1_000_000


class DeltaError(RuntimeError):
    pass


class StableCursorUnsupported(DeltaError):
    """The source explicitly returned its legacy session-list representation."""


@dataclass(frozen=True)
class SessionProjection:
    sessions: list[dict[str, Any]]
    protocol: str
    request_count: int
    snapshot: str | None = None
    ingest_fence: str | None = None


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Tickets and management credentials must never cross a redirect boundary."""

    def redirect_request(
        self,
        request: urllib.request.Request,
        file_pointer: BinaryIO,
        code: int,
        message: str,
        headers: Any,
        new_url: str,
    ) -> None:
        return None


HTTP_OPENER = urllib.request.build_opener(NoRedirect)


def parse_time(value: str, label: str) -> dt.datetime:
    if not isinstance(value, str) or not value.strip():
        raise DeltaError(f"{label} is missing")
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = dt.datetime.fromisoformat(text)
    except ValueError as error:
        raise DeltaError(f"{label} is not RFC3339") from error
    if parsed.tzinfo is None:
        raise DeltaError(f"{label} must contain a timezone")
    return parsed.astimezone(dt.timezone.utc)


def format_time(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).isoformat(timespec="microseconds").replace(
        "+00:00", "Z"
    )


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def strict_json_loads(value: bytes | str) -> Any:
    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        output: dict[str, Any] = {}
        for key, item in pairs:
            if key in output:
                raise ValueError("duplicate JSON object key")
            output[key] = item
        return output

    def reject_constant(value: str) -> None:
        raise ValueError(f"non-finite JSON number: {value}")

    return json.loads(
        value, object_pairs_hook=unique_object, parse_constant=reject_constant
    )


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def is_sha256(value: Any) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def ensure_private_regular(path: Path, label: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except FileNotFoundError as error:
        raise DeltaError(f"{label} does not exist") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise DeltaError(f"{label} must be a regular non-symlink file")
    if metadata.st_mode & 0o077:
        raise DeltaError(f"{label} must not be accessible by group or other")
    return metadata


def load_token(path: Path) -> str:
    ensure_private_regular(path, "management token file")
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise DeltaError("management token file could not be opened safely") from error
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077:
            raise DeltaError(
                "management token file must be a private regular file"
            )
        raw = os.read(descriptor, 16_385)
    finally:
        os.close(descriptor)
    try:
        token = raw.decode("utf-8").strip()
    except UnicodeDecodeError as error:
        raise DeltaError("management token file is invalid") from error
    if (
        not token
        or len(raw) > 16_384
        or any(ord(char) < 0x21 or ord(char) > 0x7E for char in token)
    ):
        raise DeltaError("management token file is invalid")
    return token


def safe_origin(raw: str, allow_http: bool, label: str) -> tuple[str, str]:
    if not raw or any(ord(char) < 0x21 or ord(char) > 0x7E for char in raw):
        raise DeltaError(f"{label} is invalid")
    parsed = urllib.parse.urlsplit(raw)
    if parsed.scheme not in ({"https", "http"} if allow_http else {"https"}):
        raise DeltaError(f"{label} must use HTTPS")
    if (
        not parsed.netloc
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
    ):
        raise DeltaError(f"{label} is invalid")
    origin = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, "", "", ""))
    prefix = parsed.path.rstrip("/")
    return origin, origin + prefix


def unwrap_json(value: Any) -> Any:
    current = value
    for _ in range(6):
        if not isinstance(current, dict):
            return current
        if "url" in current or "records" in current or "session_id" in current:
            return current
        if "StatusCode" in current:
            status_code = current.get("StatusCode")
            if status_code != 200:
                raise DeltaError("source plugin response returned a non-success status")
            nested = current.get("Body")
            if not isinstance(nested, str):
                raise DeltaError("source plugin response body is invalid")
            try:
                current = strict_json_loads(nested)
            except (json.JSONDecodeError, ValueError):
                try:
                    decoded = base64.b64decode(nested, validate=True)
                    current = strict_json_loads(decoded)
                except (
                    binascii.Error,
                    json.JSONDecodeError,
                    UnicodeDecodeError,
                    ValueError,
                ) as error:
                    raise DeltaError("source plugin response body is invalid") from error
            continue
        moved = False
        for key in ("result", "Result", "data", "body"):
            if key not in current:
                continue
            nested = current[key]
            if isinstance(nested, str):
                try:
                    nested = strict_json_loads(nested)
                except (json.JSONDecodeError, ValueError):
                    continue
            current = nested
            moved = True
            break
        if not moved:
            return current
    return current


class SourceClient:
    def __init__(
        self,
        base_url: str,
        download_base_url: str,
        token: str,
        timeout: float,
        allow_http: bool,
    ) -> None:
        self.origin, self.base = safe_origin(
            base_url, allow_http, "CPA management base URL"
        )
        self.download_origin, self.download_base = safe_origin(
            download_base_url, allow_http, "archive download base URL"
        )
        self.token = token
        self.timeout = timeout

    def _management_json(self, path: str, query: dict[str, str]) -> Any:
        url = self.base + path
        if query:
            url += "?" + urllib.parse.urlencode(query)
        request = urllib.request.Request(
            url,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/json",
                "User-Agent": "memeloop-token-center-delta-export/1",
            },
        )
        try:
            with HTTP_OPENER.open(request, timeout=self.timeout) as response:
                if response.status != 200:
                    raise DeltaError(
                        f"source management request returned HTTP {response.status}"
                    )
                payload = response.read(8 * 1024 * 1024 + 1)
        except urllib.error.HTTPError as error:
            raise DeltaError(
                f"source management request returned HTTP {error.code}"
            ) from None
        except (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            OSError,
        ):
            raise DeltaError("source management request failed") from None
        if len(payload) > 8 * 1024 * 1024:
            raise DeltaError("source management response is too large")
        try:
            return unwrap_json(strict_json_loads(payload))
        except (json.JSONDecodeError, UnicodeDecodeError, ValueError) as error:
            raise DeltaError("source management response is not valid JSON") from error

    @staticmethod
    def _session_items(payload: Any, strict_tie_order: bool) -> list[dict[str, Any]]:
        if not isinstance(payload, list):
            raise DeltaError("source sessions response is not an array")
        output: list[dict[str, Any]] = []
        seen: set[str] = set()
        previous: tuple[dt.datetime, str] | None = None
        for item in payload:
            if not isinstance(item, dict):
                raise DeltaError("source session summary is invalid")
            session_id = item.get("session_id")
            if not isinstance(session_id, str) or not session_id or session_id in seen:
                raise DeltaError("source session identity is invalid or duplicated")
            if strict_tie_order and (
                len(session_id) > 512
                or any(
                    ord(character) < 0x21 or ord(character) > 0x7E
                    for character in session_id
                )
            ):
                raise DeltaError("source stable session identity is not printable ASCII")
            last_at = parse_time(item.get("last_at"), "source session last_at")
            first_at = parse_time(item.get("first_at"), "source session first_at")
            if first_at > last_at:
                raise DeltaError("source session time range is invalid")
            if strict_tie_order and (
                item.get("first_at") != format_time(first_at)
                or item.get("last_at") != format_time(last_at)
                or not is_sha256(item.get("records_sha256"))
            ):
                raise DeltaError(
                    "source stable session timestamps or record digest are invalid"
                )
            current = (last_at, session_id)
            if previous is not None and (
                last_at > previous[0]
                or (
                    strict_tie_order
                    and last_at == previous[0]
                    and session_id <= previous[1]
                )
            ):
                raise DeltaError(
                    "source sessions are not in stable last_at/session_id order"
                )
            requests = item.get("requests")
            if not isinstance(requests, int) or isinstance(requests, bool) or requests < 0:
                raise DeltaError("source session request count is invalid")
            previous = current
            seen.add(session_id)
            output.append(item)
        return output

    def sessions(self, limit: int) -> list[dict[str, Any]]:
        payload = self._management_json(SESSIONS_PATH, {"limit": str(limit)})
        if isinstance(payload, dict):
            payload = payload.get("sessions", payload.get("items"))
        return self._session_items(payload, strict_tie_order=False)

    @staticmethod
    def _opaque_cursor(value: Any, label: str) -> str:
        if (
            not isinstance(value, str)
            or not value
            or len(value) > 4096
            or any(ord(character) < 0x21 or ord(character) > 0x7E for character in value)
        ):
            raise DeltaError(f"source {label} is invalid")
        return value

    @staticmethod
    def _ingest_fence(value: Any, label: str) -> str:
        if (
            not isinstance(value, str)
            or not value.isascii()
            or not value.isdecimal()
            or (len(value) > 1 and value.startswith("0"))
            or len(value) > 20
            or int(value) > 2**63 - 1
        ):
            raise DeltaError(f"source {label} is invalid")
        return value

    def stable_sessions(
        self,
        limit: int,
        lower_bound: dt.datetime,
        snapshot: str | None = None,
        after_ingest_fence: str | None = None,
    ) -> SessionProjection:
        if after_ingest_fence is not None:
            after_ingest_fence = self._ingest_fence(
                after_ingest_fence, "prior ingest fence"
            )
        sessions: list[dict[str, Any]] = []
        seen_sessions: set[str] = set()
        seen_cursors: set[str] = set()
        cursor: str | None = None
        expected: tuple[str, str, int, int, str] | None = None
        expected_snapshot = snapshot
        previous: tuple[dt.datetime, str] | None = None

        while True:
            query = {
                "limit": str(limit),
                "cursor_protocol": STABLE_CURSOR_PROTOCOL,
                "lower_bound_completed_at": format_time(lower_bound),
            }
            if expected_snapshot is not None:
                query["snapshot"] = expected_snapshot
            if after_ingest_fence is not None:
                query["after_ingest_fence"] = after_ingest_fence
            if cursor is not None:
                query["cursor"] = cursor
            payload = self._management_json(SESSIONS_PATH, query)
            if (
                not isinstance(payload, dict)
                or payload.get("cursor_protocol") != STABLE_CURSOR_PROTOCOL
            ):
                if isinstance(payload, list) or (
                    isinstance(payload, dict)
                    and "cursor_protocol" not in payload
                    and ("sessions" in payload or "items" in payload)
                ):
                    raise StableCursorUnsupported(
                        f"source does not implement {STABLE_CURSOR_PROTOCOL}"
                    )
                raise DeltaError(
                    "source returned an invalid stable session projection response"
                )
            page_snapshot = self._opaque_cursor(payload.get("snapshot"), "snapshot")
            ingest_fence = self._ingest_fence(
                payload.get("ingest_fence"), "ingest fence"
            )
            if (
                after_ingest_fence is not None
                and int(ingest_fence) < int(after_ingest_fence)
            ):
                raise DeltaError("source ingest fence moved backwards")
            session_count = payload.get("session_count")
            request_count = payload.get("request_count")
            set_digest = payload.get("session_set_sha256")
            complete = payload.get("complete")
            next_cursor = payload.get("next_cursor")
            if (
                not isinstance(session_count, int)
                or isinstance(session_count, bool)
                or session_count < 0
                or session_count > MAX_SESSION_COUNT
                or not isinstance(request_count, int)
                or isinstance(request_count, bool)
                or request_count < 0
                or not is_sha256(set_digest)
                or not isinstance(complete, bool)
            ):
                raise DeltaError("source stable session projection metadata is invalid")
            metadata = (
                page_snapshot,
                ingest_fence,
                session_count,
                request_count,
                set_digest,
            )
            if expected is None:
                expected = metadata
                expected_snapshot = page_snapshot
            elif metadata != expected:
                raise DeltaError("source stable session projection metadata changed between pages")
            if snapshot is not None and page_snapshot != snapshot:
                raise DeltaError("source stable session snapshot could not be replayed")

            raw_items = payload.get("sessions", payload.get("items"))
            page = self._session_items(raw_items, strict_tie_order=True)
            if len(page) > limit or (not page and not complete):
                raise DeltaError("source stable session projection page is invalid")
            for item in page:
                session_id = item["session_id"]
                last_at = parse_time(item["last_at"], "source session last_at")
                current = (last_at, session_id)
                if previous is not None and (
                    last_at > previous[0]
                    or (last_at == previous[0] and session_id <= previous[1])
                ):
                    raise DeltaError(
                        "source stable session pages overlap or are not in cursor order"
                    )
                if session_id in seen_sessions:
                    raise DeltaError("source stable session pages contain a duplicate session")
                seen_sessions.add(session_id)
                sessions.append(item)
                previous = current
                if len(sessions) > session_count:
                    raise DeltaError("source stable session projection exceeds its declared count")

            if complete:
                if next_cursor is not None:
                    raise DeltaError("source stable session projection completion is invalid")
                break
            next_value = self._opaque_cursor(next_cursor, "session cursor")
            if next_value in seen_cursors:
                raise DeltaError("source stable session projection cursor loop detected")
            seen_cursors.add(next_value)
            cursor = next_value

        if expected is None:
            raise DeltaError("source stable session projection is empty without metadata")
        if len(sessions) != expected[2]:
            raise DeltaError("source stable session projection has a gap")
        if sum(item["requests"] for item in sessions) != expected[3]:
            raise DeltaError("source stable session projection request count disagrees")
        if selection_digest(sessions) != expected[4]:
            raise DeltaError("source stable session projection digest disagrees")
        return SessionProjection(
            sessions=sessions,
            protocol=STABLE_CURSOR_PROTOCOL,
            request_count=expected[3],
            snapshot=expected[0],
            ingest_fence=expected[1],
        )

    def stats_records(self) -> int:
        payload = self._management_json(STATS_PATH, {})
        if not isinstance(payload, dict):
            raise DeltaError("source stats response is invalid")
        records = payload.get("records")
        if not isinstance(records, int) or isinstance(records, bool) or records < 0:
            raise DeltaError("source stats record count is invalid")
        return records

    def ticket_url(
        self,
        session_id: str,
        snapshot: str | None = None,
        records_sha256: str | None = None,
    ) -> str:
        query = {"id": session_id, "scope": "session", "format": "archive"}
        if snapshot is not None:
            query["snapshot"] = snapshot
        payload = self._management_json(
            EXPORT_PATH,
            query,
        )
        if not isinstance(payload, dict) or not isinstance(payload.get("url"), str):
            raise DeltaError("source export ticket response is invalid")
        if snapshot is not None and (
            payload.get("cursor_protocol") != STABLE_CURSOR_PROTOCOL
            or payload.get("snapshot") != snapshot
            or payload.get("records_sha256") != records_sha256
        ):
            raise DeltaError("source export ticket is not bound to the stable snapshot")
        if any(
            ord(character) < 0x21 or ord(character) > 0x7E
            for character in payload["url"]
        ):
            raise DeltaError("source export ticket response is invalid")
        ticket = urllib.parse.urljoin(self.download_base + "/", payload["url"])
        parsed = urllib.parse.urlsplit(ticket)
        ticket_origin = urllib.parse.urlunsplit(
            (parsed.scheme, parsed.netloc, "", "", "")
        )
        if (
            ticket_origin != self.download_origin
            or parsed.username
            or parsed.password
            or parsed.fragment
            or not parsed.path.startswith("/archive-api/v1/exports/")
        ):
            raise DeltaError("source export ticket escaped the configured download origin")
        return ticket

    def export_lines(
        self,
        session_id: str,
        maximum: int,
        snapshot: str | None = None,
        records_sha256: str | None = None,
    ) -> Iterable[bytes]:
        ticket = self.ticket_url(session_id, snapshot, records_sha256)
        request = urllib.request.Request(
            ticket,
            headers={
                "Accept": "application/x-ndjson",
                "User-Agent": "memeloop-token-center-delta-export/1",
            },
        )
        try:
            response: BinaryIO = HTTP_OPENER.open(request, timeout=self.timeout)
        except urllib.error.HTTPError as error:
            raise DeltaError(f"source archive export returned HTTP {error.code}") from None
        except (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            OSError,
        ):
            raise DeltaError("source archive export failed") from None
        try:
            while True:
                line = response.readline(maximum + 1)
                if not line:
                    break
                if len(line) > maximum:
                    raise DeltaError("source archive record exceeds the configured line limit")
                if line.strip():
                    yield line
        except (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            OSError,
        ):
            raise DeltaError("source archive export stream failed") from None
        finally:
            response.close()


def source_fingerprint(client: SourceClient) -> str:
    material = json.dumps(
        {
            "origin": client.origin,
            "base": client.base,
            "download_origin": client.download_origin,
            "download_base": client.download_base,
            "sessions_path": SESSIONS_PATH,
            "export_path": EXPORT_PATH,
            "stats_path": STATS_PATH,
            "version": SOURCE_FINGERPRINT_VERSION,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return sha256_bytes(material)


def load_checkpoint(path: Path, fingerprint: str) -> dict[str, Any] | None:
    if not path.exists():
        return None
    ensure_private_regular(path, "checkpoint")
    try:
        value = strict_json_loads(path.read_bytes())
    except (json.JSONDecodeError, UnicodeDecodeError, ValueError) as error:
        raise DeltaError("checkpoint is not valid JSON") from error
    if (
        not isinstance(value, dict)
        or value.get("version") not in (1, CHECKPOINT_VERSION)
        or value.get("source_fingerprint") != fingerprint
        or not isinstance(value.get("sequence"), int)
        or isinstance(value.get("sequence"), bool)
        or value["sequence"] < 0
        or not is_sha256(value.get("last_output_sha256"))
        or not isinstance(value.get("last_output_records"), int)
        or value["last_output_records"] < 0
        or not isinstance(value.get("last_source_records"), int)
        or value["last_source_records"] < 0
    ):
        raise DeltaError("checkpoint does not match this source or version")
    parse_time(value.get("watermark_completed_at"), "checkpoint watermark")
    if value["version"] == CHECKPOINT_VERSION:
        protocol = value.get("session_projection_protocol")
        fence = value.get("source_ingest_fence")
        if protocol not in (
            LEGACY_PROJECTION_PROTOCOL,
            STABLE_CURSOR_PROTOCOL,
        ):
            raise DeltaError("checkpoint session projection protocol is invalid")
        if protocol == STABLE_CURSOR_PROTOCOL:
            SourceClient._ingest_fence(fence, "checkpoint ingest fence")
        elif fence is not None:
            raise DeltaError("legacy checkpoint contains an ingest fence")
    return value


def write_atomic_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.exists() and path.is_symlink():
        raise DeltaError("refusing to replace a symlink")
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(descriptor, 0o600)
        data = canonical_bytes(value) + b"\n"
        with os.fdopen(descriptor, "wb", closefd=True) as stream:
            descriptor = -1
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def file_digest(path: Path) -> tuple[int, str]:
    size = 0
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            size += len(chunk)
            digest.update(chunk)
    return size, digest.hexdigest()


def fsync_directory(path: Path) -> None:
    directory = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def validate_manifest(manifest: Any, fingerprint: str, output: Path) -> dict[str, Any]:
    if not isinstance(manifest, dict):
        raise DeltaError("delta manifest is invalid")
    required_integers = (
        "sequence",
        "overlap_seconds",
        "max_future_skew_seconds",
        "session_limit",
        "session_count",
        "record_count",
        "source_records_before",
        "source_records_after",
        "output_size_bytes",
    )
    if (
        manifest.get("version") not in (1, MANIFEST_VERSION)
        or manifest.get("source_fingerprint") != fingerprint
        or manifest.get("output_file") != output.name
        or any(
            not isinstance(manifest.get(key), int)
            or isinstance(manifest.get(key), bool)
            or manifest[key] < 0
            for key in required_integers
        )
        or manifest["sequence"] < 1
        or manifest["overlap_seconds"] < 1
        or not is_sha256(manifest.get("output_sha256"))
        or not is_sha256(manifest.get("session_set_sha256"))
        or not isinstance(manifest.get("stable_source_required"), bool)
    ):
        raise DeltaError("delta manifest is invalid")
    prior_output_sha256 = manifest.get("prior_output_sha256")
    if prior_output_sha256 is not None and (
        not is_sha256(prior_output_sha256)
    ):
        raise DeltaError("delta manifest prior output digest is invalid")
    projection_protocol = manifest.get(
        "session_projection_protocol", LEGACY_PROJECTION_PROTOCOL
    )
    projection_requests = manifest.get("source_projection_requests")
    snapshot_digest = manifest.get("source_snapshot_sha256")
    prior_ingest_fence = manifest.get("prior_source_ingest_fence")
    ingest_fence = manifest.get("source_ingest_fence")
    if manifest["version"] == MANIFEST_VERSION:
        version_two_keys = (
            "session_projection_protocol",
            "source_projection_requests",
            "source_snapshot_sha256",
            "prior_source_ingest_fence",
            "source_ingest_fence",
        )
        if any(key not in manifest for key in version_two_keys):
            raise DeltaError("delta manifest version-two metadata is incomplete")
    elif any(
        key in manifest
        for key in (
            "session_projection_protocol",
            "source_projection_requests",
            "source_snapshot_sha256",
            "prior_source_ingest_fence",
            "source_ingest_fence",
        )
    ):
        raise DeltaError("version-one delta manifest has version-two metadata")
    if projection_protocol not in (
        LEGACY_PROJECTION_PROTOCOL,
        STABLE_CURSOR_PROTOCOL,
    ):
        raise DeltaError("delta manifest session projection protocol is invalid")
    if projection_requests is not None and (
        not isinstance(projection_requests, int)
        or isinstance(projection_requests, bool)
        or projection_requests < manifest["record_count"]
    ):
        raise DeltaError("delta manifest projection request count is invalid")
    if projection_protocol == STABLE_CURSOR_PROTOCOL:
        if (
            projection_requests is None
            or not is_sha256(snapshot_digest)
            or not isinstance(ingest_fence, str)
        ):
            raise DeltaError("delta manifest stable snapshot metadata is invalid")
        SourceClient._ingest_fence(ingest_fence, "manifest ingest fence")
        if prior_ingest_fence is not None:
            SourceClient._ingest_fence(
                prior_ingest_fence, "manifest prior ingest fence"
            )
            if int(ingest_fence) < int(prior_ingest_fence):
                raise DeltaError("delta manifest ingest fence moved backwards")
    elif snapshot_digest is not None:
        raise DeltaError("delta manifest legacy projection has snapshot metadata")
    elif ingest_fence is not None or prior_ingest_fence is not None:
        raise DeltaError("delta manifest legacy projection has ingest fence metadata")
    if (
        manifest["overlap_seconds"] > 31 * 86_400
        or manifest["max_future_skew_seconds"] > 86_400
        or not 1 <= manifest["session_limit"] <= 1000
        or manifest["session_count"] > MAX_SESSION_COUNT
        or (
            projection_protocol == LEGACY_PROJECTION_PROTOCOL
            and manifest["session_count"] > manifest["session_limit"]
        )
        or manifest["source_records_after"] < manifest["source_records_before"]
        or manifest["record_count"] > manifest["source_records_after"]
        or (manifest["record_count"] > 0) != (manifest.get("max_started_at") is not None)
    ):
        raise DeltaError("delta manifest counts are inconsistent")
    prior = parse_time(
        manifest.get("prior_watermark_completed_at"),
        "delta manifest prior watermark",
    )
    watermark = parse_time(
        manifest.get("watermark_completed_at"), "delta manifest watermark"
    )
    lower = parse_time(
        manifest.get("lower_bound_completed_at"), "delta manifest lower bound"
    )
    observed = parse_time(manifest.get("observed_at"), "delta manifest observation time")
    max_started = manifest.get("max_started_at")
    if max_started is not None:
        parse_time(max_started, "delta manifest maximum start time")
    if (
        lower != prior - dt.timedelta(seconds=manifest["overlap_seconds"])
        or prior > watermark
        or watermark
        > observed
        + dt.timedelta(seconds=manifest["max_future_skew_seconds"])
    ):
        raise DeltaError("delta manifest watermarks are inconsistent")
    return manifest


def commit_checkpoint(
    path: Path, fingerprint: str, manifest: dict[str, Any]
) -> dict[str, Any]:
    checkpoint = {
        "version": CHECKPOINT_VERSION,
        "source_fingerprint": fingerprint,
        "sequence": manifest["sequence"],
        "watermark_completed_at": manifest["watermark_completed_at"],
        "last_output_sha256": manifest["output_sha256"],
        "last_output_records": manifest["record_count"],
        "last_source_records": manifest["source_records_after"],
        "session_projection_protocol": manifest.get(
            "session_projection_protocol", LEGACY_PROJECTION_PROTOCOL
        ),
        "source_ingest_fence": manifest.get("source_ingest_fence"),
    }
    write_atomic_json(path, checkpoint)
    return checkpoint


def resume_output(
    output: Path,
    pending_output: Path,
    manifest_path: Path,
    checkpoint_path: Path,
    checkpoint: dict[str, Any] | None,
    fingerprint: str,
) -> dict[str, Any]:
    ensure_private_regular(manifest_path, "delta manifest")
    try:
        manifest = validate_manifest(
            strict_json_loads(manifest_path.read_bytes()), fingerprint, output
        )
    except (json.JSONDecodeError, UnicodeDecodeError, ValueError) as error:
        raise DeltaError("delta manifest is not valid JSON") from error
    prior_sequence = 0 if checkpoint is None else checkpoint["sequence"]
    is_next = manifest["sequence"] == prior_sequence + 1
    if checkpoint is not None and is_next:
        is_next = (
            manifest["prior_watermark_completed_at"]
            == checkpoint["watermark_completed_at"]
            and manifest.get("prior_output_sha256")
            == checkpoint["last_output_sha256"]
            and manifest.get("prior_source_ingest_fence")
            == checkpoint.get("source_ingest_fence")
        )
    elif checkpoint is None and is_next:
        is_next = (
            manifest.get("prior_output_sha256") is None
            and manifest.get("prior_source_ingest_fence") is None
        )
    is_committed = (
        checkpoint is not None
        and manifest["sequence"] == checkpoint["sequence"]
        and manifest["output_sha256"] == checkpoint["last_output_sha256"]
        and manifest["record_count"] == checkpoint["last_output_records"]
        and manifest.get("source_ingest_fence")
        == checkpoint.get("source_ingest_fence")
        and manifest.get(
            "session_projection_protocol", LEGACY_PROJECTION_PROTOCOL
        )
        == checkpoint.get(
            "session_projection_protocol", LEGACY_PROJECTION_PROTOCOL
        )
    )
    if not is_next and not is_committed:
        raise DeltaError("delta manifest is not the next checkpoint transition")

    if output.exists() and pending_output.exists():
        raise DeltaError("both final and pending delta outputs exist")
    selected_output = output if output.exists() else pending_output
    ensure_private_regular(selected_output, "delta output")
    size, digest = file_digest(selected_output)
    if size != manifest.get("output_size_bytes") or digest != manifest.get(
        "output_sha256"
    ):
        raise DeltaError("delta output does not match its manifest")
    if selected_output == pending_output:
        os.replace(pending_output, output)
        fsync_directory(output.parent)
    if is_next:
        commit_checkpoint(checkpoint_path, fingerprint, manifest)
    return manifest


def select_sessions(
    sessions: list[dict[str, Any]], lower_bound: dt.datetime
) -> list[dict[str, Any]]:
    return [
        item
        for item in sessions
        if parse_time(item["last_at"], "source session last_at") >= lower_bound
    ]


def load_session_projection(
    client: SourceClient,
    lower_bound: dt.datetime,
    limit: int,
    source_records: int,
    snapshot: str | None = None,
    after_ingest_fence: str | None = None,
) -> SessionProjection:
    if snapshot is not None:
        return client.stable_sessions(
            limit, lower_bound, snapshot, after_ingest_fence
        )
    if after_ingest_fence is not None:
        return client.stable_sessions(
            limit, lower_bound, after_ingest_fence=after_ingest_fence
        )
    try:
        return client.stable_sessions(limit, lower_bound)
    except StableCursorUnsupported:
        pass
    legacy = client.sessions(limit)
    verify_complete_projection(legacy, limit, source_records)
    selected = select_sessions(legacy, lower_bound)
    if len(legacy) == limit:
        oldest = parse_time(legacy[-1]["last_at"], "source session last_at")
        if oldest >= lower_bound:
            raise DeltaError(
                "source session projection is saturated and does not implement "
                f"{STABLE_CURSOR_PROTOCOL}"
            )
    return SessionProjection(
        sessions=selected,
        protocol=LEGACY_PROJECTION_PROTOCOL,
        request_count=sum(item["requests"] for item in selected),
    )


def selection_digest(sessions: list[dict[str, Any]]) -> str:
    stable: list[dict[str, Any]] = []
    for item in sessions:
        summary = {
            "session_id": item["session_id"],
            "requests": item["requests"],
            "first_at": format_time(
                parse_time(item["first_at"], "source session first_at")
            ),
            "last_at": format_time(
                parse_time(item["last_at"], "source session last_at")
            ),
        }
        if "records_sha256" in item:
            summary["records_sha256"] = item["records_sha256"]
        stable.append(summary)
    stable.sort(key=lambda item: item["session_id"])
    return sha256_bytes(canonical_bytes(stable))


def verify_complete_projection(
    sessions: list[dict[str, Any]], limit: int, source_records: int
) -> None:
    if len(sessions) < limit:
        projected = sum(item["requests"] for item in sessions)
        if projected != source_records:
            raise DeltaError(
                "source record count disagrees with the complete session projection"
            )


def verify_source_clock(
    sessions: list[dict[str, Any]], maximum_allowed: dt.datetime
) -> None:
    for session in sessions:
        if (
            parse_time(session["first_at"], "source session first_at")
            > maximum_allowed
            or parse_time(session["last_at"], "source session last_at")
            > maximum_allowed
        ):
            raise DeltaError("source session timestamp exceeds the future-skew limit")


def export_delta(args: argparse.Namespace) -> dict[str, Any]:
    token = load_token(args.token_file)
    client = SourceClient(
        args.base_url,
        args.download_base_url or args.base_url,
        token,
        args.timeout_seconds,
        args.allow_http,
    )
    fingerprint = source_fingerprint(client)
    checkpoint = load_checkpoint(args.checkpoint, fingerprint)
    manifest_path = Path(str(args.output) + ".manifest.json")
    pending_output = Path(str(args.output) + ".pending")
    if args.resume:
        if (
            pending_output.exists()
            and not manifest_path.exists()
            and not args.output.exists()
        ):
            ensure_private_regular(pending_output, "orphaned pending delta output")
            pending_output.unlink()
            fsync_directory(pending_output.parent)
        else:
            return resume_output(
                args.output,
                pending_output,
                manifest_path,
                args.checkpoint,
                checkpoint,
                fingerprint,
            )
    if args.output.exists() or pending_output.exists() or manifest_path.exists():
        raise DeltaError(
            "delta output, pending file, or manifest already exists; use --resume or a new path"
        )
    if checkpoint is None:
        if args.since is None:
            raise DeltaError("--since is required before the first checkpoint")
        prior_watermark = parse_time(args.since, "initial since watermark")
        prior_ingest_fence = None
        sequence = 1
    else:
        if args.since is not None:
            raise DeltaError("--since cannot replace an existing checkpoint")
        prior_watermark = parse_time(
            checkpoint["watermark_completed_at"], "checkpoint watermark"
        )
        prior_ingest_fence = checkpoint.get("source_ingest_fence")
        sequence = checkpoint["sequence"] + 1
    try:
        lower_bound = prior_watermark - dt.timedelta(seconds=args.overlap_seconds)
    except OverflowError as error:
        raise DeltaError("source checkpoint is outside the supported time range") from error
    observed_at = dt.datetime.now(dt.timezone.utc)
    maximum_allowed = observed_at + dt.timedelta(
        seconds=args.max_future_skew_seconds
    )
    if prior_watermark > maximum_allowed:
        raise DeltaError("source checkpoint timestamp exceeds the future-skew limit")

    source_records_before = client.stats_records()
    first_projection = load_session_projection(
        client,
        lower_bound,
        args.session_limit,
        source_records_before,
        after_ingest_fence=prior_ingest_fence,
    )
    selected = first_projection.sessions
    verify_source_clock(selected, maximum_allowed)
    first_selection_digest = selection_digest(selected)

    args.output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    spool_descriptor, spool_name = tempfile.mkstemp(
        prefix=".mtc-archive-delta-spool.", suffix=".sqlite", dir=args.output.parent
    )
    os.close(spool_descriptor)
    os.chmod(spool_name, 0o600)
    output_temporary: str | None = None
    maximum_completed = prior_watermark
    maximum_started: dt.datetime | None = None
    try:
        spool = sqlite3.connect(spool_name)
        spool.execute("PRAGMA journal_mode=DELETE")
        spool.execute("PRAGMA synchronous=FULL")
        spool.execute(
            "CREATE TABLE records(request_id TEXT PRIMARY KEY, started_at TEXT NOT NULL, "
            "completed_at TEXT NOT NULL, digest TEXT NOT NULL, canonical BLOB NOT NULL)"
        )
        spool.execute(
            "CREATE TABLE seen_records(request_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, "
            "digest TEXT NOT NULL, canonical BLOB NOT NULL)"
        )
        downloaded_bytes = 0
        selected_bytes = 0
        for session in sorted(selected, key=lambda item: item["session_id"]):
            session_id = session["session_id"]
            exported_records = 0
            for raw_line in client.export_lines(
                session_id,
                args.max_line_bytes,
                first_projection.snapshot,
                session.get("records_sha256"),
            ):
                downloaded_bytes += len(raw_line)
                if downloaded_bytes > args.max_download_bytes:
                    raise DeltaError("source archive downloads exceed the configured limit")
                try:
                    item = strict_json_loads(raw_line)
                except (json.JSONDecodeError, UnicodeDecodeError, ValueError) as error:
                    raise DeltaError("source archive stream contains invalid JSON") from error
                if not isinstance(item, dict) or item.get("schema_version") not in (1, 2):
                    raise DeltaError("source archive record schema is unsupported")
                request_id = item.get("request_id")
                if not isinstance(request_id, str) or not request_id:
                    raise DeltaError("source archive request identity is invalid")
                if item.get("session_id") != session_id:
                    raise DeltaError("source session export returned a foreign session record")
                started_at = parse_time(item.get("started_at"), "archive started_at")
                completed_at = parse_time(item.get("completed_at"), "archive completed_at")
                if completed_at < started_at:
                    raise DeltaError("source archive record time range is invalid")
                if started_at > maximum_allowed or completed_at > maximum_allowed:
                    raise DeltaError(
                        "source archive timestamp exceeds the future-skew limit"
                    )
                encoded = canonical_bytes(item) + b"\n"
                digest = sha256_bytes(encoded)
                existing = spool.execute(
                    "SELECT session_id, digest FROM seen_records WHERE request_id=?",
                    (request_id,),
                ).fetchone()
                if existing is not None:
                    if existing[0] != session_id or existing[1] != digest:
                        raise DeltaError(
                            "one source request id has conflicting archive records"
                        )
                    continue
                spool.execute(
                    "INSERT INTO seen_records VALUES(?,?,?,?)",
                    (request_id, session_id, digest, encoded),
                )
                exported_records += 1
                if (
                    first_projection.protocol == LEGACY_PROJECTION_PROTOCOL
                    and started_at < lower_bound
                    and completed_at < lower_bound
                ):
                    continue
                selected_bytes += len(encoded)
                if selected_bytes > args.max_output_bytes:
                    raise DeltaError("delta output exceeds the configured size limit")
                spool.execute(
                    "INSERT INTO records VALUES(?,?,?,?,?)",
                    (
                        request_id,
                        format_time(started_at),
                        format_time(completed_at),
                        digest,
                        encoded,
                    ),
                )
                maximum_completed = max(maximum_completed, completed_at)
                maximum_started = (
                    started_at
                    if maximum_started is None
                    else max(maximum_started, started_at)
                )
            if exported_records != session["requests"]:
                raise DeltaError(
                    "source session export count disagrees with its session summary"
                )
            if first_projection.protocol == STABLE_CURSOR_PROTOCOL:
                session_digest = hashlib.sha256()
                session_rows = spool.execute(
                    "SELECT canonical FROM seen_records WHERE session_id=? "
                    "ORDER BY request_id",
                    (session_id,),
                )
                for (canonical,) in session_rows:
                    session_digest.update(canonical)
                if session_digest.hexdigest() != session["records_sha256"]:
                    raise DeltaError(
                        "source session export digest disagrees with its stable summary"
                    )
        spool.commit()

        source_records_after = client.stats_records()
        if args.require_stable_source and source_records_after != source_records_before:
            raise DeltaError("source record count changed despite the requested write barrier")
        second_projection = load_session_projection(
            client,
            lower_bound,
            args.session_limit,
            source_records_after,
            first_projection.snapshot,
            prior_ingest_fence,
        )
        verify_source_clock(second_projection.sessions, maximum_allowed)
        if (
            second_projection.protocol != first_projection.protocol
            or second_projection.request_count != first_projection.request_count
            or selection_digest(second_projection.sessions) != first_selection_digest
        ):
            raise DeltaError("source session projection changed during delta export; retry")
        final_source_records = client.stats_records()
        if args.require_stable_source and final_source_records != source_records_before:
            raise DeltaError("source record count changed despite the requested write barrier")
        source_records_after = final_source_records
        if source_records_after < source_records_before:
            raise DeltaError("source record count decreased during delta export")

        descriptor, output_temporary = tempfile.mkstemp(
            prefix=f".{args.output.name}.", dir=args.output.parent
        )
        os.fchmod(descriptor, 0o600)
        output_digest = hashlib.sha256()
        output_size = 0
        record_count = 0
        with os.fdopen(descriptor, "wb", closefd=True) as destination:
            rows = spool.execute(
                "SELECT canonical FROM records ORDER BY started_at, request_id"
            )
            for (encoded,) in rows:
                destination.write(encoded)
                output_digest.update(encoded)
                output_size += len(encoded)
                record_count += 1
            destination.flush()
            os.fsync(destination.fileno())
        os.replace(output_temporary, pending_output)
        output_temporary = None
        fsync_directory(args.output.parent)

        manifest = {
            "version": MANIFEST_VERSION,
            "source_fingerprint": fingerprint,
            "observed_at": format_time(observed_at),
            "max_future_skew_seconds": args.max_future_skew_seconds,
            "sequence": sequence,
            "prior_watermark_completed_at": format_time(prior_watermark),
            "prior_output_sha256": (
                None if checkpoint is None else checkpoint["last_output_sha256"]
            ),
            "lower_bound_completed_at": format_time(lower_bound),
            "overlap_seconds": args.overlap_seconds,
            "watermark_completed_at": format_time(maximum_completed),
            "max_started_at": (
                None if maximum_started is None else format_time(maximum_started)
            ),
            "session_limit": args.session_limit,
            "session_count": len(selected),
            "session_projection_protocol": first_projection.protocol,
            "source_projection_requests": first_projection.request_count,
            "source_snapshot_sha256": (
                None
                if first_projection.snapshot is None
                else sha256_bytes(first_projection.snapshot.encode("utf-8"))
            ),
            "prior_source_ingest_fence": prior_ingest_fence,
            "source_ingest_fence": first_projection.ingest_fence,
            "session_set_sha256": first_selection_digest,
            "record_count": record_count,
            "source_records_before": source_records_before,
            "source_records_after": source_records_after,
            "stable_source_required": bool(args.require_stable_source),
            "output_file": args.output.name,
            "output_size_bytes": output_size,
            "output_sha256": output_digest.hexdigest(),
        }
        write_atomic_json(manifest_path, manifest)
        os.replace(pending_output, args.output)
        fsync_directory(args.output.parent)
        commit_checkpoint(args.checkpoint, fingerprint, manifest)
        return manifest
    finally:
        try:
            spool.close()
        except UnboundLocalError:
            pass
        try:
            os.unlink(spool_name)
        except FileNotFoundError:
            pass
        if output_temporary is not None:
            try:
                os.unlink(output_temporary)
            except FileNotFoundError:
                pass


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Export a checkpointed cpa-session-archive delta through CPA/API2"
    )
    result.add_argument("--base-url", required=True)
    result.add_argument("--download-base-url")
    result.add_argument("--token-file", required=True, type=Path)
    result.add_argument("--checkpoint", required=True, type=Path)
    result.add_argument("--output", required=True, type=Path)
    result.add_argument("--since")
    result.add_argument("--overlap-seconds", type=int, default=86_400)
    result.add_argument("--session-limit", type=int, default=1000)
    result.add_argument("--max-line-bytes", type=int, default=16 * 1024 * 1024)
    result.add_argument("--max-download-bytes", type=int, default=64 * 1024**3)
    result.add_argument("--max-output-bytes", type=int, default=64 * 1024**3)
    result.add_argument("--timeout-seconds", type=float, default=60.0)
    result.add_argument("--max-future-skew-seconds", type=int, default=3600)
    result.add_argument("--require-stable-source", action="store_true")
    result.add_argument("--allow-http", action="store_true")
    result.add_argument("--resume", action="store_true")
    return result


def main() -> int:
    args = parser().parse_args()
    if args.overlap_seconds < 1 or args.overlap_seconds > 31 * 86_400:
        raise DeltaError("overlap seconds must be between one second and 31 days")
    if args.session_limit < 1 or args.session_limit > 1000:
        raise DeltaError("session limit must be between 1 and 1000")
    if args.max_line_bytes < 1024 or args.max_line_bytes > 16 * 1024 * 1024:
        raise DeltaError("max line bytes must be between 1 KiB and 16 MiB")
    if args.max_download_bytes < args.max_line_bytes or args.max_download_bytes > 1024**4:
        raise DeltaError("max download bytes must cover one line and be at most 1 TiB")
    if args.max_output_bytes < args.max_line_bytes or args.max_output_bytes > 1024**4:
        raise DeltaError("max output bytes must cover one line and be at most 1 TiB")
    if args.timeout_seconds <= 0 or args.timeout_seconds > 3600:
        raise DeltaError("timeout seconds must be between 0 and 3600")
    if args.max_future_skew_seconds < 0 or args.max_future_skew_seconds > 86_400:
        raise DeltaError("max future skew seconds must be between 0 and 86400")
    manifest = export_delta(args)
    print(
        json.dumps(
            {
                "sequence": manifest["sequence"],
                "sessions": manifest["session_count"],
                "records": manifest["record_count"],
                "watermark_completed_at": manifest["watermark_completed_at"],
                "source_records": manifest["source_records_after"],
                "output_sha256": manifest["output_sha256"],
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except DeltaError as error:
        print(f"delta export refused: {error}", file=sys.stderr)
        raise SystemExit(2) from None
    except (OSError, sqlite3.Error):
        print("delta export failed because of a local I/O error", file=sys.stderr)
        raise SystemExit(2) from None
