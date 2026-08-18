#!/usr/bin/env python3
"""Attach unchanged CPA client credentials to imported CPAMP identities.

Plaintext credentials are accepted only from stdin, a mounted file, or the
read-only CPA management endpoint. They remain in this process' memory and are
never placed in argv, environment variables, output, or temporary files.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import pathlib
import re
import resource
import ssl
import stat
import subprocess
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from typing import BinaryIO, NoReturn


MAX_INPUT_BYTES = 4 * 1024 * 1024
MAX_HTTP_RESPONSE_BYTES = 1024 * 1024
MAX_IDENTITIES = 100_000
LOCK_SQL = (
    "pg_try_advisory_lock(hashtextextended("
    "'memeloop-token-center:legacy-cpa-credentials', 734627102948314))"
)
UNLOCK_SQL = (
    "pg_advisory_unlock(hashtextextended("
    "'memeloop-token-center:legacy-cpa-credentials', 734627102948314))"
)
IDENTITIES_END = "__MTC_LEGACY_IDENTITIES_END__"
HEX_SHA256 = re.compile(r"^[0-9a-f]{64}$")
TENANT_ID = re.compile(r"^[A-Za-z0-9._:-]{1,200}$")

# A crash must not turn in-memory credential material into a core file.
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


class ImportFailure(RuntimeError):
    """A deliberately secret-free operator-facing failure."""


@dataclass(frozen=True)
class Identity:
    source_hash: str
    key_id: str


@dataclass(frozen=True)
class Plan:
    candidates: tuple[tuple[str, Identity], ...]
    identity_count: int
    existing_count: int
    already_attached: int


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        del req, fp, code, msg, headers, newurl
        return None


def bounded_read(stream: BinaryIO, limit: int, label: str) -> bytes:
    value = stream.read(limit + 1)
    if len(value) > limit:
        raise ImportFailure(f"{label} exceeds the allowed size")
    return value


def read_secret_file(path_value: str, label: str, limit: int = MAX_INPUT_BYTES) -> bytes:
    path = pathlib.Path(path_value)
    try:
        with path.open("rb", buffering=0) as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode):
                raise ImportFailure(f"{label} must be a regular file")
            process_groups = {os.getegid(), *os.getgroups()}
            unauthorized_mode = metadata.st_mode & 0o037
            group_read_is_unauthorized = bool(metadata.st_mode & 0o040) and (
                metadata.st_gid not in process_groups
            )
            if unauthorized_mode or group_read_is_unauthorized:
                raise ImportFailure(f"{label} has unsafe access permissions")
            return bounded_read(stream, limit, label)
    except ImportFailure:
        raise
    except OSError as error:
        raise ImportFailure(f"{label} is not readable") from error


def decode_utf8(value: bytes, label: str) -> str:
    try:
        return value.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ImportFailure(f"{label} is not valid UTF-8") from error


def validate_credential(value: object) -> str:
    if not isinstance(value, str):
        raise ImportFailure("credential input contains a non-string item")
    if value != value.strip() or "\x00" in value or "\r" in value or "\n" in value:
        raise ImportFailure("credential input contains an invalid item")
    length = len(value.encode("utf-8"))
    if length < 16 or length > 512:
        raise ImportFailure("credential input contains an invalid item")
    return value


def reject_duplicate_json_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for name, value in pairs:
        if name in result:
            raise ImportFailure("CPA JSON contains duplicate object keys")
        result[name] = value
    return result


def parse_candidates(raw: bytes, input_format: str) -> list[str]:
    text = decode_utf8(raw, "credential input")
    if input_format == "cpa-json":
        try:
            document = json.loads(text, object_pairs_hook=reject_duplicate_json_keys)
        except ImportFailure:
            raise
        except (json.JSONDecodeError, RecursionError) as error:
            raise ImportFailure("CPA JSON is invalid") from error
        if not isinstance(document, dict) or set(document) != {"api-keys"}:
            raise ImportFailure("CPA JSON must contain only the api-keys field")
        values = document["api-keys"]
        if not isinstance(values, list):
            raise ImportFailure("CPA JSON api-keys must be an array")
    elif input_format == "lines":
        values = text.splitlines()
        if not values or any(value == "" for value in values):
            raise ImportFailure("line input must contain non-empty credentials")
    else:  # argparse prevents this; keeping the parser independently fail-closed helps tests.
        raise ImportFailure("unsupported credential input format")
    candidates = [validate_credential(value) for value in values]
    if not candidates:
        raise ImportFailure("credential input is empty")
    return candidates


def normalized_base_url(value: str, label: str, allow_http: bool) -> str:
    try:
        parsed = urllib.parse.urlsplit(value)
    except ValueError as error:
        raise ImportFailure(f"{label} is invalid") from error
    allowed_schemes = {"https"}
    if allow_http:
        allowed_schemes.add("http")
    if (
        parsed.scheme not in allowed_schemes
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise ImportFailure(f"{label} is invalid")
    path = parsed.path.rstrip("/")
    return urllib.parse.urlunsplit(
        (parsed.scheme, parsed.netloc, path, "", "")
    )


def ssl_context(ca_file: str | None) -> ssl.SSLContext:
    try:
        return ssl.create_default_context(cafile=ca_file)
    except (OSError, ssl.SSLError) as error:
        raise ImportFailure("CA file is invalid or unreadable") from error


def opener_for(ca_file: str | None) -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(
        NoRedirectHandler(), urllib.request.HTTPSHandler(context=ssl_context(ca_file))
    )


def request_bytes(
    opener: urllib.request.OpenerDirector,
    request: urllib.request.Request,
    label: str,
    expected_status: int,
    limit: int,
) -> bytes:
    try:
        with opener.open(request, timeout=30) as response:
            status_code = response.getcode()
            if status_code != expected_status:
                raise ImportFailure(f"{label} returned an unexpected status")
            return bounded_read(response, limit, label)
    except ImportFailure:
        raise
    except urllib.error.HTTPError as error:
        # Never read or echo an error body: either peer could reflect a secret.
        error.close()
        raise ImportFailure(f"{label} returned an unexpected status") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise ImportFailure(f"{label} request failed") from error


def read_token(path: str, label: str) -> str:
    token = decode_utf8(read_secret_file(path, label, 16 * 1024), label)
    if token != token.strip() and token.rstrip("\r\n") != token.strip():
        raise ImportFailure(f"{label} contains invalid whitespace")
    token = token.strip()
    if not token or "\r" in token or "\n" in token or "\x00" in token:
        raise ImportFailure(f"{label} is invalid")
    return token


def fetch_cpa_candidates(
    base_url: str,
    management_token_file: str,
    ca_file: str | None,
    allow_http: bool,
) -> bytes:
    base_url = normalized_base_url(base_url, "CPA management URL", allow_http)
    token = read_token(management_token_file, "CPA management token file")
    request = urllib.request.Request(
        f"{base_url}/v0/management/api-keys",
        method="GET",
        headers={"Authorization": f"Bearer {token}", "Accept": "application/json"},
    )
    return request_bytes(
        opener_for(ca_file), request, "CPA management export", 200, MAX_INPUT_BYTES
    )


class PsqlIdentitySession:
    """Keep one PostgreSQL session advisory lock across preflight and apply."""

    def __init__(self, tenant_external_id: str, psql_binary: str) -> None:
        if not TENANT_ID.fullmatch(tenant_external_id):
            raise ImportFailure("tenant external id contains unsupported characters")
        self.tenant_external_id = tenant_external_id
        self._stderr: list[str] = []
        psql_environment = os.environ.copy()
        psql_environment.setdefault("PGCONNECT_TIMEOUT", "10")
        psql_environment.setdefault("PGAPPNAME", "mtc-legacy-credential-import")
        try:
            self.process = subprocess.Popen(
                [
                    psql_binary,
                    "-X",
                    "--no-psqlrc",
                    "-qAt",
                    "--no-password",
                    "--set=ON_ERROR_STOP=1",
                    f"--set=tenant_external_id={tenant_external_id}",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="strict",
                bufsize=1,
                env=psql_environment,
            )
        except OSError as error:
            raise ImportFailure("psql could not be started") from error
        if self.process.stdin is None or self.process.stdout is None or self.process.stderr is None:
            self.process.kill()
            raise ImportFailure("psql pipes could not be created")
        self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self._stderr_thread.start()
        try:
            self._write(f"SELECT CASE WHEN {LOCK_SQL} THEN '1' ELSE '0' END;\n")
            lock_result = self._readline()
        except ImportFailure:
            self.close()
            raise
        if lock_result != "1":
            self.close()
            raise ImportFailure("another legacy credential import holds the advisory lock")

    def _drain_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            # SQL is constant and contains no credential. Keep only a bounded diagnostic.
            if sum(len(item) for item in self._stderr) < 16 * 1024:
                self._stderr.append(line.rstrip())

    def _write(self, value: str) -> None:
        assert self.process.stdin is not None
        try:
            self.process.stdin.write(value)
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise ImportFailure("PostgreSQL identity session ended unexpectedly") from error

    def _readline(self) -> str:
        assert self.process.stdout is not None
        try:
            value = self.process.stdout.readline()
        except (OSError, UnicodeError) as error:
            raise ImportFailure("PostgreSQL identity output is invalid") from error
        if value == "":
            raise ImportFailure("PostgreSQL identity query failed")
        return value.rstrip("\r\n")

    def mappings(self) -> tuple[list[Identity], list[Identity], list[Identity]]:
        self._write(
            "COPY ("
            "SELECT 'identity', lower(i.api_key_hash), i.key_id "
            "FROM cpamp_import_identities i "
            "JOIN key_records k ON k.id = i.key_id AND k.status = 'active' "
            "JOIN tenants t ON t.id = k.tenant_id "
            "WHERE t.external_id = :'tenant_external_id' "
            "UNION ALL "
            "SELECT CASE WHEN c.revoked_at IS NULL THEN 'existing' ELSE 'revoked' END, "
            "lower(c.source_hash), c.key_id "
            "FROM legacy_key_credentials c "
            "WHERE EXISTS ("
            "SELECT 1 FROM cpamp_import_identities i "
            "JOIN key_records k ON k.id = i.key_id "
            "JOIN tenants t ON t.id = k.tenant_id "
            "WHERE t.external_id = :'tenant_external_id' "
            "AND (lower(c.source_hash) = lower(i.api_key_hash) OR c.key_id = i.key_id)"
            ") "
            "ORDER BY 1, 2, 3"
            ") TO STDOUT WITH (FORMAT csv);\n"
            f"\\echo {IDENTITIES_END}\n"
        )
        identities: list[Identity] = []
        existing: list[Identity] = []
        revoked: list[Identity] = []
        while True:
            line = self._readline()
            if line == IDENTITIES_END:
                break
            try:
                row = next(csv.reader([line], strict=True))
            except (csv.Error, StopIteration) as error:
                raise ImportFailure("PostgreSQL identity output is invalid") from error
            if len(row) != 3 or row[0] not in {"identity", "existing", "revoked"}:
                raise ImportFailure("PostgreSQL identity output is invalid")
            identity = checked_identity(row[1], row[2])
            if row[0] == "identity":
                identities.append(identity)
            elif row[0] == "existing":
                existing.append(identity)
            else:
                revoked.append(identity)
            if len(identities) + len(existing) + len(revoked) > MAX_IDENTITIES:
                raise ImportFailure("PostgreSQL identity result exceeds the allowed size")
        return identities, existing, revoked

    def close(self) -> None:
        if not hasattr(self, "process") or self.process.poll() is not None:
            return
        try:
            self._write(f"SELECT {UNLOCK_SQL};\n\\quit\n")
            self.process.wait(timeout=5)
        except (ImportFailure, subprocess.TimeoutExpired, ValueError):
            self.process.kill()
            self.process.wait()
        self._stderr_thread.join(timeout=1)

    def __enter__(self) -> "PsqlIdentitySession":
        return self

    def __exit__(self, _type, _value, _traceback) -> None:  # noqa: ANN001
        self.close()


def checked_identity(source_hash: str, key_id: str) -> Identity:
    source_hash = source_hash.lower()
    if not HEX_SHA256.fullmatch(source_hash):
        raise ImportFailure("PostgreSQL contains an invalid source hash")
    try:
        normalized_key_id = str(uuid.UUID(key_id))
    except (ValueError, AttributeError) as error:
        raise ImportFailure("PostgreSQL contains an invalid target key id") from error
    if normalized_key_id != key_id.lower():
        raise ImportFailure("PostgreSQL contains a non-canonical target key id")
    return Identity(source_hash, normalized_key_id)


def build_plan(
    credentials: list[str],
    identities: list[Identity],
    existing: list[Identity],
    revoked: list[Identity] | None = None,
) -> Plan:
    identity_by_hash: dict[str, Identity] = {}
    identity_hash_by_key: dict[str, str] = {}
    for identity in identities:
        if identity.source_hash in identity_by_hash:
            raise ImportFailure("CPAMP identities contain a duplicate source hash")
        previous_hash = identity_hash_by_key.get(identity.key_id)
        if previous_hash is not None and previous_hash != identity.source_hash:
            raise ImportFailure("CPAMP identities contain a duplicate target key")
        identity_by_hash[identity.source_hash] = identity
        identity_hash_by_key[identity.key_id] = identity.source_hash
    if not identity_by_hash:
        raise ImportFailure("CPAMP identity set is empty")

    candidate_by_hash: dict[str, str] = {}
    for credential in credentials:
        source_hash = hashlib.sha256(credential.encode("utf-8")).hexdigest()
        if source_hash in candidate_by_hash:
            raise ImportFailure("credential input contains a duplicate credential")
        candidate_by_hash[source_hash] = credential

    unmatched_candidates = set(candidate_by_hash) - set(identity_by_hash)
    unprovided_identities = set(identity_by_hash) - set(candidate_by_hash)
    if unmatched_candidates or unprovided_identities:
        raise ImportFailure("credential and CPAMP identity sets do not match exactly")

    if revoked:
        raise ImportFailure("a selected source or target has a revoked legacy mapping")

    existing_by_hash: dict[str, str] = {}
    existing_hash_by_key: dict[str, str] = {}
    for attached in existing:
        previous_key = existing_by_hash.get(attached.source_hash)
        if previous_key is not None and previous_key != attached.key_id:
            raise ImportFailure("existing legacy mappings contain a source conflict")
        previous_hash = existing_hash_by_key.get(attached.key_id)
        if previous_hash is not None and previous_hash != attached.source_hash:
            raise ImportFailure("existing legacy mappings contain a target conflict")
        existing_by_hash[attached.source_hash] = attached.key_id
        existing_hash_by_key[attached.key_id] = attached.source_hash

    already_attached = 0
    pairs: list[tuple[str, Identity]] = []
    for source_hash in sorted(candidate_by_hash):
        identity = identity_by_hash[source_hash]
        attached_key = existing_by_hash.get(source_hash)
        if attached_key is not None and attached_key != identity.key_id:
            raise ImportFailure("an existing legacy source maps to another target")
        attached_hash = existing_hash_by_key.get(identity.key_id)
        if attached_hash is not None and attached_hash != source_hash:
            raise ImportFailure("an existing target maps to another legacy source")
        if attached_key == identity.key_id:
            already_attached += 1
        pairs.append((candidate_by_hash[source_hash], identity))
    return Plan(tuple(pairs), len(identities), len(existing), already_attached)


def attach_one(
    opener: urllib.request.OpenerDirector,
    target_base_url: str,
    service_token: str,
    credential: str,
    identity: Identity,
) -> None:
    body = json.dumps(
        {"credential": credential, "source_hash": identity.source_hash},
        separators=(",", ":"),
    ).encode("utf-8")
    request = urllib.request.Request(
        f"{target_base_url}/internal/v1/keys/{identity.key_id}/legacy-credentials",
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {service_token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    raw_response = request_bytes(
        opener, request, "Token Center legacy credential API", 201, MAX_HTTP_RESPONSE_BYTES
    )
    try:
        response = json.loads(raw_response)
    except (json.JSONDecodeError, UnicodeDecodeError, RecursionError) as error:
        raise ImportFailure("Token Center legacy credential response is invalid") from error
    if (
        not isinstance(response, dict)
        or response.get("key_id") != identity.key_id
        or response.get("source_hash") != identity.source_hash
        or not isinstance(response.get("generation"), int)
        or not isinstance(response.get("fingerprint"), str)
        or not response["fingerprint"]
    ):
        raise ImportFailure("Token Center legacy credential response did not verify")


def summary(plan: Plan, mode: str, attached_verified: int) -> str:
    # Deliberately expose counts only: no credential, hash, fingerprint, key UUID, or URL.
    return json.dumps(
        {
            "mode": mode,
            "candidate_count": len(plan.candidates),
            "identity_count": plan.identity_count,
            "existing_mapping_count": plan.existing_count,
            "already_attached_count": plan.already_attached,
            "pending_count": len(plan.candidates) - plan.already_attached,
            "attached_verified_count": attached_verified,
        },
        separators=(",", ":"),
        sort_keys=True,
    )


def argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Match unchanged CPA credentials to CPAMP identities (dry-run by default).",
        allow_abbrev=False,
    )
    parser.add_argument("--tenant-external-id", required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument(
        "--input-file",
        help="Read candidate credentials from this mounted file; use '-' for stdin.",
    )
    source.add_argument(
        "--cpa-management-url",
        help="Read only GET /v0/management/api-keys from this CPA base URL.",
    )
    parser.add_argument(
        "--input-format", choices=("cpa-json", "lines"), default="cpa-json"
    )
    parser.add_argument("--cpa-management-token-file")
    parser.add_argument("--cpa-ca-file")
    parser.add_argument("--allow-http-cpa", action="store_true")
    parser.add_argument("--target-api-base-url")
    parser.add_argument("--service-token-file")
    parser.add_argument("--target-ca-file")
    parser.add_argument("--allow-http-target", action="store_true")
    parser.add_argument("--psql-binary", default="psql", help=argparse.SUPPRESS)
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Perform idempotent API attachments after the full preflight succeeds.",
    )
    return parser


def run(arguments: argparse.Namespace) -> str:
    if arguments.cpa_management_url:
        if not arguments.cpa_management_token_file:
            raise ImportFailure("CPA management token file is required")
        if arguments.input_format != "cpa-json":
            raise ImportFailure("CPA management export requires cpa-json input format")
        raw = fetch_cpa_candidates(
            arguments.cpa_management_url,
            arguments.cpa_management_token_file,
            arguments.cpa_ca_file,
            arguments.allow_http_cpa,
        )
    else:
        if arguments.cpa_management_token_file or arguments.cpa_ca_file:
            raise ImportFailure("CPA management options require CPA management URL")
        if arguments.input_file == "-":
            raw = bounded_read(sys.stdin.buffer, MAX_INPUT_BYTES, "credential input")
        else:
            raw = read_secret_file(arguments.input_file, "credential input")
    credentials = parse_candidates(raw, arguments.input_format)

    if arguments.apply:
        if not arguments.target_api_base_url or not arguments.service_token_file:
            raise ImportFailure("target API URL and service token file are required for apply")
        target_url = normalized_base_url(
            arguments.target_api_base_url,
            "Token Center API URL",
            arguments.allow_http_target,
        )
        service_token = read_token(arguments.service_token_file, "service token file")
        target_opener = opener_for(arguments.target_ca_file)
    else:
        if arguments.target_api_base_url or arguments.service_token_file or arguments.target_ca_file:
            raise ImportFailure("target API options are accepted only with --apply")
        target_url = ""
        service_token = ""
        target_opener = None

    with PsqlIdentitySession(arguments.tenant_external_id, arguments.psql_binary) as database:
        identities, existing, revoked = database.mappings()
        plan = build_plan(credentials, identities, existing, revoked)
        if not arguments.apply:
            return summary(plan, "dry-run", 0)
        assert target_opener is not None
        attached_verified = 0
        for credential, identity in plan.candidates:
            attach_one(
                target_opener, target_url, service_token, credential, identity
            )
            attached_verified += 1
        return summary(plan, "apply", attached_verified)


def fail(message: str) -> NoReturn:
    print(f"legacy credential import failed: {message}", file=sys.stderr)
    raise SystemExit(2)


def main() -> None:
    try:
        arguments = argument_parser().parse_args()
        print(run(arguments))
    except ImportFailure as error:
        fail(str(error))


if __name__ == "__main__":
    main()
