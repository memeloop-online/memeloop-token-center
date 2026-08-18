#!/usr/bin/env python3
"""Inventory and import CPA upstream accounts through the control API.

The source config, auth files, target service token, and optional subscription
bridge secret are accepted only from owner-only regular files.  Secret values
are kept outside the printable plan and are never included in errors or output.
"""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
import pathlib
import re
import resource
import ssl
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, BinaryIO, NoReturn

import yaml


MAX_CONFIG_BYTES = 4 * 1024 * 1024
MAX_AUTH_BYTES = 1024 * 1024
MAX_SECRET_BYTES = 64 * 1024
MAX_RESPONSE_BYTES = 4 * 1024 * 1024
MAX_ACCOUNTS = 10_000
SOURCE_VERSION = "cpa-upstream-import-v1"
TENANT_PATTERN = re.compile(r"^[A-Za-z0-9._:-]{1,200}$")
HANDLE_PATTERN = re.compile(r"^[A-Za-z0-9]{1,80}$")
SAFE_NAME_PATTERN = re.compile(r"[^a-z0-9]+")
HEADER_NAME_PATTERN = re.compile(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]{1,200}$")
MANAGED_SOURCE_TYPE_PATTERN = re.compile(r"^[a-z0-9._-]{1,64}$")
MANAGED_OAUTH_CONTRACT_VERSION = 1
MANAGED_OAUTH_SOURCE_TYPES = {
    "codex": "codex",
    "gemini": "gemini-legacy",
}

# A crash must not turn in-memory credentials into a core file.
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


class ImportFailure(RuntimeError):
    """A deliberately secret-free operator-facing failure."""


class UniqueSafeLoader(yaml.SafeLoader):
    """Safe YAML loader that rejects aliases and duplicate mapping keys."""

    def compose_node(self, parent: object, index: object) -> yaml.Node:
        if self.check_event(yaml.AliasEvent):
            raise ImportFailure("CPA config YAML aliases are not supported")
        return super().compose_node(parent, index)


def construct_unique_mapping(
    loader: UniqueSafeLoader, node: yaml.MappingNode, deep: bool = False
) -> dict[object, object]:
    loader.flatten_mapping(node)
    result: dict[object, object] = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        try:
            duplicate = key in result
        except TypeError as error:
            raise ImportFailure("CPA config contains a non-scalar mapping key") from error
        if duplicate:
            raise ImportFailure("CPA config contains a duplicate mapping key")
        result[key] = loader.construct_object(value_node, deep=deep)
    return result


UniqueSafeLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, construct_unique_mapping
)


@dataclass(frozen=True)
class DirectAccount:
    source_id: str
    name: str
    driver: str
    config: dict[str, object]
    header: str
    prefix: str
    secret_ref: str
    disabled: bool


@dataclass(frozen=True)
class SubscriptionAccount:
    source_id: str
    source_basename: str
    provider: str
    document_ref: str


@dataclass(frozen=True)
class ManagedOAuthAccount:
    stable_id: str
    source_type: str
    payload_ref: str


@dataclass(frozen=True)
class Inventory:
    direct: tuple[DirectAccount, ...]
    subscriptions: tuple[SubscriptionAccount, ...]
    managed_oauth: tuple[ManagedOAuthAccount, ...]
    disabled_source_count: int


class SecretStore:
    """Credential material intentionally separated from printable plan records."""

    def __init__(self) -> None:
        self._values: dict[str, object] = {}

    def put(self, reference: str, value: object) -> None:
        if reference in self._values:
            raise ImportFailure("CPA source identity is duplicated")
        self._values[reference] = value

    def get_string(self, reference: str) -> str:
        value = self._values.get(reference)
        if not isinstance(value, str):
            raise ImportFailure("internal credential reference is invalid")
        return value

    def get_document(self, reference: str) -> dict[str, object]:
        value = self._values.get(reference)
        if not isinstance(value, dict):
            raise ImportFailure("internal subscription reference is invalid")
        return value

    def take_document(self, reference: str) -> dict[str, object]:
        value = self._values.pop(reference, None)
        if not isinstance(value, dict):
            raise ImportFailure("internal managed OAuth reference is invalid")
        return value


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        del req, fp, code, msg, headers, newurl
        return None


def bounded_read(stream: BinaryIO, limit: int, label: str) -> bytes:
    value = stream.read(limit + 1)
    if len(value) > limit:
        raise ImportFailure(f"{label} exceeds the allowed size")
    return value


def read_owner_only_file(path_value: str, label: str, limit: int) -> bytes:
    path = pathlib.Path(path_value)
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ImportFailure(f"{label} is not a readable owner-only regular file") from error
    try:
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
        ):
            raise ImportFailure(f"{label} must be an owner-owned mode-0600 regular file")
        with os.fdopen(descriptor, "rb", closefd=False) as stream:
            return bounded_read(stream, limit, label)
    finally:
        os.close(descriptor)


def validate_auth_directory(path_value: str) -> pathlib.Path:
    path = pathlib.Path(path_value)
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ImportFailure("CPA auth directory is not readable") from error
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or path.is_symlink()
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.geteuid()
    ):
        raise ImportFailure("CPA auth directory must be an owner-owned mode-0700 directory")
    return path


def auth_files(root: pathlib.Path) -> list[tuple[str, pathlib.Path]]:
    found: list[tuple[str, pathlib.Path]] = []
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        current_path = pathlib.Path(current)
        current_metadata = current_path.lstat()
        if (
            not stat.S_ISDIR(current_metadata.st_mode)
            or current_path.is_symlink()
            or stat.S_IMODE(current_metadata.st_mode) != 0o700
            or current_metadata.st_uid != os.geteuid()
        ):
            raise ImportFailure("CPA auth directory contains an unsafe directory")
        for directory in directories:
            child = current_path / directory
            if child.is_symlink():
                raise ImportFailure("CPA auth directory contains a symbolic link")
        for filename in files:
            candidate = current_path / filename
            if candidate.is_symlink():
                raise ImportFailure("CPA auth directory contains a symbolic link")
            if candidate.suffix.lower() != ".json":
                raise ImportFailure("CPA auth directory contains an unsupported file")
            relative = candidate.relative_to(root).as_posix()
            try:
                relative.encode("utf-8", errors="strict")
            except UnicodeEncodeError as error:
                raise ImportFailure(
                    "CPA auth directory contains a non-UTF-8 relative path"
                ) from error
            found.append((relative, candidate))
    found.sort(key=lambda item: item[0])
    if len(found) > MAX_ACCOUNTS:
        raise ImportFailure("CPA auth directory contains too many records")
    return found


def decode_utf8(value: bytes, label: str) -> str:
    try:
        return value.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ImportFailure(f"{label} is not valid UTF-8") from error


def parse_config(raw: bytes) -> dict[str, object]:
    try:
        document = yaml.load(decode_utf8(raw, "CPA config"), Loader=UniqueSafeLoader)
    except ImportFailure:
        raise
    except yaml.YAMLError as error:
        raise ImportFailure("CPA config is not valid safe YAML") from error
    if not isinstance(document, dict) or not all(isinstance(key, str) for key in document):
        raise ImportFailure("CPA config must be a string-keyed mapping")
    auth_dir = document.get("auth-dir")
    if not isinstance(auth_dir, str) or not auth_dir.strip():
        raise ImportFailure("CPA config must declare auth-dir")
    for key, value in document.items():
        if key in {
            "api-keys",
            "gemini-api-key",
            "codex-api-key",
            "claude-api-key",
            "openai-compatibility",
        }:
            continue
        if (key.endswith("-api-key") or key.endswith("-compatibility")) and value:
            raise ImportFailure("CPA config contains an unsupported upstream credential section")
    return document


def reject_duplicate_json_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ImportFailure("CPA auth JSON contains a duplicate field")
        result[key] = value
    return result


def parse_auth_document(raw: bytes) -> dict[str, object]:
    try:
        document = json.loads(
            decode_utf8(raw, "CPA auth document"),
            object_pairs_hook=reject_duplicate_json_keys,
        )
    except ImportFailure:
        raise
    except (json.JSONDecodeError, RecursionError) as error:
        raise ImportFailure("CPA auth document is invalid JSON") from error
    if not isinstance(document, dict):
        raise ImportFailure("CPA auth document must be an object")
    return document


def require_mapping(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise ImportFailure(f"{label} must be a string-keyed mapping")
    return value


def require_list(value: object, label: str) -> list[object]:
    if value is None:
        return []
    if not isinstance(value, list):
        raise ImportFailure(f"{label} must be a list")
    return value


def require_exact_fields(value: dict[str, object], allowed: set[str], label: str) -> None:
    if set(value) - allowed:
        raise ImportFailure(f"{label} contains an unsupported field")


def secret_string(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise ImportFailure(f"{label} must be a string")
    if value != value.strip() or not value or len(value.encode("utf-8")) > 16 * 1024:
        raise ImportFailure(f"{label} is invalid")
    if any(character in value for character in ("\x00", "\r", "\n")):
        raise ImportFailure(f"{label} is invalid")
    return value


def public_url(value: object, label: str, allow_http_loopback: bool) -> str:
    if not isinstance(value, str):
        raise ImportFailure(f"{label} must be a URL string")
    try:
        parsed = urllib.parse.urlsplit(value)
        port = parsed.port
    except ValueError as error:
        raise ImportFailure(f"{label} is invalid") from error
    if (
        parsed.scheme not in {"https", "http"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise ImportFailure(f"{label} is invalid")
    if parsed.scheme == "http":
        hostname = parsed.hostname
        is_loopback = hostname == "localhost"
        if not is_loopback:
            try:
                is_loopback = ipaddress.ip_address(hostname).is_loopback
            except ValueError:
                is_loopback = False
        if not allow_http_loopback or not is_loopback:
            raise ImportFailure(f"{label} must use HTTPS")
    host = parsed.hostname
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    netloc = host if port is None else f"{host}:{port}"
    path = parsed.path.rstrip("/")
    return urllib.parse.urlunsplit((parsed.scheme, netloc, path, "", ""))


def source_identity(kind: str, *parts: object) -> str:
    components = [SOURCE_VERSION, kind, *(str(part) for part in parts)]
    return "\0".join(components)


def source_digest(source_id: str) -> str:
    return hashlib.sha256(source_id.encode("utf-8")).hexdigest()


def account_name(label: str, source_id: str) -> str:
    slug = SAFE_NAME_PATTERN.sub("-", label.lower()).strip("-")[:80] or "upstream"
    return f"cpa-{slug}-{source_digest(source_id)[:16]}"


def add_direct_account(
    records: list[DirectAccount],
    secrets: SecretStore,
    *,
    source_id: str,
    label: str,
    base_url: str,
    credential: str,
    header: str,
    prefix: str,
    disabled: bool,
) -> None:
    secret_ref = f"direct:{source_digest(source_id)}"
    secrets.put(secret_ref, credential)
    records.append(
        DirectAccount(
            source_id=source_id,
            name=account_name(label, source_id),
            driver="http-json",
            config={"base_url": base_url, "network_scope": "public"},
            header=header,
            prefix=prefix,
            secret_ref=secret_ref,
            disabled=disabled,
        )
    )


def reject_unsupported_transport_fields(entry: dict[str, object], label: str) -> None:
    if entry.get("proxy-url") not in (None, ""):
        raise ImportFailure(f"{label} uses a per-account proxy unsupported by the target API")
    headers = entry.get("headers")
    if headers not in (None, {}):
        raise ImportFailure(f"{label} uses custom headers unsupported by the target API")
    if entry.get("cloak") not in (None, {}):
        raise ImportFailure(f"{label} uses request cloaking unsupported by the target API")


def inventory_config_accounts(
    config: dict[str, object], secrets: SecretStore, allow_http_loopback: bool
) -> tuple[list[DirectAccount], int]:
    records: list[DirectAccount] = []
    disabled_count = 0
    compatibility = require_list(
        config.get("openai-compatibility"), "CPA openai-compatibility"
    )
    provider_names: set[str] = set()
    for raw_provider in compatibility:
        provider = require_mapping(raw_provider, "CPA openai-compatibility entry")
        require_exact_fields(
            provider,
            {
                "name",
                "disabled",
                "prefix",
                "base-url",
                "headers",
                "api-key-entries",
                "models",
                "excluded-models",
            },
            "CPA openai-compatibility entry",
        )
        name = provider.get("name")
        if not isinstance(name, str) or not name.strip() or len(name) > 200:
            raise ImportFailure("CPA openai-compatibility provider name is invalid")
        if name in provider_names:
            raise ImportFailure("CPA openai-compatibility provider name is duplicated")
        provider_names.add(name)
        disabled = provider.get("disabled", False)
        if not isinstance(disabled, bool):
            raise ImportFailure("CPA openai-compatibility disabled flag is invalid")
        reject_unsupported_transport_fields(provider, "CPA openai-compatibility entry")
        base_url = public_url(
            provider.get("base-url"),
            "CPA openai-compatibility base URL",
            allow_http_loopback,
        )
        entries = require_list(
            provider.get("api-key-entries"), "CPA openai-compatibility api-key-entries"
        )
        for key_index, raw_entry in enumerate(entries):
            entry = require_mapping(raw_entry, "CPA openai-compatibility API key entry")
            require_exact_fields(
                entry, {"api-key", "proxy-url"}, "CPA openai-compatibility API key entry"
            )
            reject_unsupported_transport_fields(
                entry, "CPA openai-compatibility API key entry"
            )
            credential = secret_string(entry.get("api-key"), "CPA upstream API key")
            record_source = source_identity(
                "config", "openai-compatibility", name, key_index
            )
            add_direct_account(
                records,
                secrets,
                source_id=record_source,
                label=name,
                base_url=base_url,
                credential=credential,
                header="authorization",
                prefix="Bearer ",
                disabled=disabled,
            )
            disabled_count += int(disabled)

    sections = {
        "gemini-api-key": (
            "gemini",
            "https://generativelanguage.googleapis.com",
            "x-goog-api-key",
            "",
        ),
        "codex-api-key": ("codex", None, "authorization", "Bearer "),
        "claude-api-key": ("claude", "https://api.anthropic.com", "x-api-key", ""),
    }
    allowed = {
        "api-key",
        "prefix",
        "base-url",
        "headers",
        "proxy-url",
        "models",
        "excluded-models",
        "cloak",
        "disabled",
    }
    for section, (label, default_url, header, credential_prefix) in sections.items():
        for index, raw_entry in enumerate(require_list(config.get(section), f"CPA {section}")):
            entry = require_mapping(raw_entry, f"CPA {section} entry")
            require_exact_fields(entry, allowed, f"CPA {section} entry")
            reject_unsupported_transport_fields(entry, f"CPA {section} entry")
            disabled = entry.get("disabled", False)
            if not isinstance(disabled, bool):
                raise ImportFailure(f"CPA {section} disabled flag is invalid")
            raw_base_url = entry.get("base-url", default_url)
            if raw_base_url is None:
                raise ImportFailure(
                    "CPA codex-api-key entry requires an explicit base-url for lossless import"
                )
            base_url = public_url(raw_base_url, f"CPA {section} base URL", allow_http_loopback)
            credential = secret_string(entry.get("api-key"), "CPA upstream API key")
            record_source = source_identity("config", section, index)
            add_direct_account(
                records,
                secrets,
                source_id=record_source,
                label=label,
                base_url=base_url,
                credential=credential,
                header=header,
                prefix=credential_prefix,
                disabled=disabled,
            )
            disabled_count += int(disabled)
    return records, disabled_count


def has_token_material(document: dict[str, object]) -> bool:
    token_fields = {"access_token", "refresh_token", "id_token", "token"}
    return any(field in document for field in token_fields)


def validate_oauth_shape(document: dict[str, object]) -> None:
    token = document.get("token")
    containers = [document]
    if token is not None:
        containers.append(require_mapping(token, "CPA OAuth token"))
    access_token = next(
        (
            container.get("access_token")
            for container in containers
            if container.get("access_token") is not None
        ),
        None,
    )
    refresh_token = next(
        (
            container.get("refresh_token")
            for container in containers
            if container.get("refresh_token") is not None
        ),
        None,
    )
    if access_token is None and refresh_token is None:
        raise ImportFailure("CPA OAuth record contains no recognized token material")
    if access_token is not None:
        secret_string(access_token, "CPA OAuth access token")
    if refresh_token is not None:
        secret_string(refresh_token, "CPA OAuth refresh token")


def validate_managed_oauth_relative_path(relative_path: str) -> None:
    encoded = relative_path.encode("utf-8", errors="strict")
    if (
        len(encoded) > 512
        or relative_path.startswith("/")
        or "\\" in relative_path
        or any(ord(character) < 32 or 127 <= ord(character) <= 159 for character in relative_path)
        or any(
            not segment or segment in {".", ".."}
            for segment in relative_path.split("/")
        )
    ):
        raise ImportFailure("CPA managed OAuth auth file has an invalid relative path")


def inventory_auth_accounts(
    root: pathlib.Path, secrets: SecretStore, allow_http_loopback: bool
) -> tuple[
    list[DirectAccount],
    list[SubscriptionAccount],
    list[ManagedOAuthAccount],
    int,
]:
    direct: list[DirectAccount] = []
    subscriptions: list[SubscriptionAccount] = []
    managed_oauth: list[ManagedOAuthAccount] = []
    disabled_count = 0
    basenames: set[str] = set()
    handles: set[str] = set()
    for relative_path, path in auth_files(root):
        document = parse_auth_document(
            read_owner_only_file(str(path), "CPA auth document", MAX_AUTH_BYTES)
        )
        disabled = document.get("disabled", False)
        if not isinstance(disabled, bool):
            raise ImportFailure("CPA auth disabled flag is invalid")
        type_value = document.get("type")
        if not isinstance(type_value, str) or not type_value.strip():
            raise ImportFailure("CPA auth document has no recognized type")
        record_type = type_value.strip().lower()
        upstream = document.get("upstream")
        is_subscription = upstream in {"copilot", "cursor"} and "handle" in document
        if is_subscription:
            if record_type not in {
                "subscription-bridge",
                "cpa-subscription-bridge",
                "copilot",
                "cursor",
            }:
                raise ImportFailure("CPA subscription auth document has an unsupported type")
            require_exact_fields(
                document,
                {"type", "upstream", "handle", "label", "login", "disabled"},
                "CPA subscription auth document",
            )
            if disabled:
                disabled_count += 1
                continue
            handle = secret_string(document.get("handle"), "CPA subscription handle")
            if not HANDLE_PATTERN.fullmatch(handle):
                raise ImportFailure("CPA subscription handle has an unsupported shape")
            if handle in handles:
                raise ImportFailure("CPA subscription handle is duplicated")
            handles.add(handle)
            basename = pathlib.PurePosixPath(relative_path).name
            if basename in basenames:
                raise ImportFailure("CPA subscription auth basenames are duplicated")
            basenames.add(basename)
            label = document.get("label")
            if label is not None and (
                not isinstance(label, str) or not label or len(label) > 200
            ):
                raise ImportFailure("CPA subscription label is invalid")
            record_source = source_identity("auth", relative_path, record_type, upstream)
            document_ref = f"subscription:{source_digest(record_source)}"
            normalized: dict[str, object] = {
                "type": "subscription-bridge",
                "upstream": upstream,
                "handle": handle,
            }
            if label is not None:
                normalized["label"] = label
            secrets.put(document_ref, normalized)
            subscriptions.append(
                SubscriptionAccount(
                    source_id=record_source,
                    source_basename=basename,
                    provider=str(upstream),
                    document_ref=document_ref,
                )
            )
            continue
        if record_type == "api_key":
            require_exact_fields(
                document,
                {
                    "type",
                    "name",
                    "provider",
                    "base_url",
                    "api_key",
                    "header",
                    "prefix",
                    "disabled",
                },
                "CPA API auth document",
            )
            base_url = public_url(
                document.get("base_url"), "CPA API auth base URL", allow_http_loopback
            )
            credential = secret_string(document.get("api_key"), "CPA upstream API key")
            header = document.get("header", "authorization")
            prefix = document.get("prefix", "Bearer ")
            if (
                not isinstance(header, str)
                or not HEADER_NAME_PATTERN.fullmatch(header)
                or not isinstance(prefix, str)
                or len(prefix) > 1024
                or "\r" in prefix
                or "\n" in prefix
                or "\x00" in prefix
            ):
                raise ImportFailure("CPA API auth header configuration is invalid")
            label_value = document.get("name", document.get("provider", "api"))
            if not isinstance(label_value, str) or not label_value or len(label_value) > 200:
                raise ImportFailure("CPA API auth account name is invalid")
            record_source = source_identity("auth", relative_path, record_type)
            add_direct_account(
                direct,
                secrets,
                source_id=record_source,
                label=label_value,
                base_url=base_url,
                credential=credential,
                header=header,
                prefix=prefix,
                disabled=disabled,
            )
            disabled_count += int(disabled)
            continue
        managed_source_type = MANAGED_OAUTH_SOURCE_TYPES.get(record_type)
        if managed_source_type is not None:
            validate_oauth_shape(document)
            validate_managed_oauth_relative_path(relative_path)
            record_source = source_identity("auth", relative_path, record_type)
            payload_ref = f"managed-oauth:{source_digest(record_source)}"
            secrets.put(
                payload_ref,
                {
                    "source": {
                        "kind": "auth_file",
                        "relative_path": relative_path,
                    },
                    "document": document,
                },
            )
            managed_oauth.append(
                ManagedOAuthAccount(
                    stable_id=source_digest(record_source),
                    source_type=managed_source_type,
                    payload_ref=payload_ref,
                )
            )
            disabled_count += int(disabled)
            continue
        if has_token_material(document):
            raise ImportFailure("CPA auth document has an unsupported managed OAuth type")
        raise ImportFailure("CPA auth document has an unsupported account type")
    return direct, subscriptions, managed_oauth, disabled_count


def build_inventory(
    config_path: str, auth_dir: str, allow_http_loopback: bool
) -> tuple[Inventory, SecretStore]:
    secrets = SecretStore()
    config = parse_config(
        read_owner_only_file(config_path, "CPA config", MAX_CONFIG_BYTES)
    )
    direct, disabled_config = inventory_config_accounts(
        config, secrets, allow_http_loopback
    )
    auth_root = validate_auth_directory(auth_dir)
    auth_direct, subscriptions, managed_oauth, disabled_auth = inventory_auth_accounts(
        auth_root, secrets, allow_http_loopback
    )
    direct.extend(auth_direct)
    if len(direct) + len(subscriptions) + len(managed_oauth) > MAX_ACCOUNTS:
        raise ImportFailure("CPA source contains too many upstream accounts")
    source_ids = [record.source_id for record in direct]
    source_ids.extend(record.source_id for record in subscriptions)
    source_ids.extend(record.stable_id for record in managed_oauth)
    names = [record.name for record in direct]
    if len(source_ids) != len(set(source_ids)) or len(names) != len(set(names)):
        raise ImportFailure("CPA source contains a stable identity conflict")
    if not direct and not subscriptions and not managed_oauth:
        raise ImportFailure("CPA source contains no active supported upstream accounts")
    return (
        Inventory(
            direct=tuple(direct),
            subscriptions=tuple(subscriptions),
            managed_oauth=tuple(managed_oauth),
            disabled_source_count=disabled_config + disabled_auth,
        ),
        secrets,
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


def bounded_http_read(response: Any, label: str) -> bytes:
    value = response.read(MAX_RESPONSE_BYTES + 1)
    if len(value) > MAX_RESPONSE_BYTES:
        value = b""
        raise ImportFailure(f"{label} response exceeds the allowed size")
    return value


def request_json(
    opener: urllib.request.OpenerDirector,
    *,
    method: str,
    url: str,
    token: str,
    label: str,
    expected_status: int,
    body: dict[str, object] | None = None,
    idempotency_key: str | None = None,
) -> object:
    _, value = request_json_with_status(
        opener,
        method=method,
        url=url,
        token=token,
        label=label,
        expected_statuses=(expected_status,),
        body=body,
        idempotency_key=idempotency_key,
    )
    return value


def request_json_with_status(
    opener: urllib.request.OpenerDirector,
    *,
    method: str,
    url: str,
    token: str,
    label: str,
    expected_statuses: tuple[int, ...],
    body: dict[str, object] | None = None,
    idempotency_key: str | None = None,
) -> tuple[int, object]:
    headers = {"Accept": "application/json", "Authorization": f"Bearer {token}"}
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        body = None
    if idempotency_key is not None:
        headers["Idempotency-Key"] = idempotency_key
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with opener.open(request, timeout=30) as response:
            status = response.getcode()
            if status not in expected_statuses:
                raise ImportFailure(f"{label} returned an unexpected status")
            raw = bounded_http_read(response, label)
    except ImportFailure:
        raise
    except urllib.error.HTTPError as error:
        # Do not read a peer body: it can reflect submitted credential material.
        error.close()
        raise ImportFailure(f"{label} returned an unexpected status") from None
    except (urllib.error.URLError, TimeoutError, OSError):
        raise ImportFailure(f"{label} failed") from None
    finally:
        # Do not retain serialized request material in an exception traceback.
        request = None
        data = None
        body = None
    try:
        response_text = raw.decode("utf-8", errors="strict")
        raw = b""
        value = json.loads(response_text)
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
        raise ImportFailure(f"{label} returned invalid JSON") from None
    finally:
        raw = b""
        response_text = ""
    return status, value


def validate_account(value: object, tenant: str, label: str) -> dict[str, object]:
    account = require_mapping(value, f"{label} account response")
    required = {"id", "tenant_external_id", "name", "driver", "config", "status", "updated_at"}
    if not required.issubset(account):
        raise ImportFailure(f"{label} returned an incomplete account")
    if account.get("tenant_external_id") != tenant:
        raise ImportFailure(f"{label} returned an account outside the selected tenant")
    if any(key in account for key in ("credential", "access_token", "refresh_token", "api_key")):
        raise ImportFailure(f"{label} returned credential material")
    return account


def preflight_target(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    tenant: str,
    inventory: Inventory,
) -> dict[str, dict[str, object]]:
    if inventory.managed_oauth:
        capabilities_raw = request_json(
            opener,
            method="GET",
            url=f"{base_url}/internal/v1/imports/cpa/managed-oauth/capabilities",
            token=token,
            label="CPA managed OAuth capability discovery",
            expected_status=200,
        )
        capabilities = require_mapping(
            capabilities_raw, "CPA managed OAuth capability response"
        )
        if set(capabilities) != {"contract_version", "source_types"}:
            raise ImportFailure(
                "CPA managed OAuth capability response has an unsupported shape"
            )
        contract_version = capabilities.get("contract_version")
        if (
            not isinstance(contract_version, int)
            or isinstance(contract_version, bool)
            or contract_version != MANAGED_OAUTH_CONTRACT_VERSION
        ):
            raise ImportFailure(
                "target does not support the required CPA managed OAuth contract"
            )
        source_types = capabilities.get("source_types")
        if (
            not isinstance(source_types, list)
            or not source_types
            or not all(
                isinstance(value, str) and MANAGED_SOURCE_TYPE_PATTERN.fullmatch(value)
                for value in source_types
            )
            or len(source_types) != len(set(source_types))
        ):
            raise ImportFailure(
                "CPA managed OAuth capability response has invalid source types"
            )
        required_source_types = {
            record.source_type for record in inventory.managed_oauth
        }
        if not required_source_types.issubset(set(source_types)):
            raise ImportFailure(
                "target is missing a managed OAuth source type required by the CPA source"
            )
    providers = request_json(
        opener,
        method="GET",
        url=f"{base_url}/internal/v1/provider-types",
        token=token,
        label="target provider discovery",
        expected_status=200,
    )
    if not isinstance(providers, list):
        raise ImportFailure("target provider discovery returned an invalid document")
    provider_ids = {
        value.get("id")
        for value in providers
        if isinstance(value, dict) and isinstance(value.get("id"), str)
    }
    required_drivers = {record.driver for record in inventory.direct}
    if inventory.subscriptions:
        required_drivers.add("cpa-subscription-bridge")
    if not required_drivers.issubset(provider_ids):
        raise ImportFailure("target is missing a provider driver required by the CPA source")
    query = urllib.parse.urlencode({"tenant_external_id": tenant})
    existing_raw = request_json(
        opener,
        method="GET",
        url=f"{base_url}/internal/v1/upstreams?{query}",
        token=token,
        label="target upstream inventory",
        expected_status=200,
    )
    if not isinstance(existing_raw, list):
        raise ImportFailure("target upstream inventory returned an invalid document")
    existing: dict[str, dict[str, object]] = {}
    for value in existing_raw:
        account = validate_account(value, tenant, "target upstream inventory")
        name = account.get("name")
        if not isinstance(name, str) or name in existing:
            raise ImportFailure("target upstream inventory contains a name conflict")
        existing[name] = account
    for record in inventory.direct:
        account = existing.get(record.name)
        if account is None:
            continue
        if account.get("driver") != record.driver or account.get("config") != record.config:
            raise ImportFailure("target account conflicts with a stable CPA source identity")
    return existing


def import_managed_oauth(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    tenant: str,
    inventory: Inventory,
    secrets: SecretStore,
) -> tuple[int, int]:
    created_count = 0
    replayed_count = 0
    for record in inventory.managed_oauth:
        payload = secrets.take_document(record.payload_ref)
        result: object | None = None
        if set(payload) != {"source", "document"}:
            payload.clear()
            raise ImportFailure("internal managed OAuth payload is invalid")
        payload["contract_version"] = MANAGED_OAUTH_CONTRACT_VERSION
        payload["tenant_external_id"] = tenant
        payload["source_type"] = record.source_type
        try:
            status, result = request_json_with_status(
                opener,
                method="POST",
                url=f"{base_url}/internal/v1/imports/cpa/managed-oauth",
                token=token,
                label="CPA managed OAuth import",
                expected_statuses=(200, 201),
                body=payload,
            )
            result_object = require_mapping(result, "CPA managed OAuth import response")
            if set(result_object) != {"disposition", "account"}:
                raise ImportFailure(
                    "CPA managed OAuth import returned an unsupported response"
                )
            expected_disposition = "created" if status == 201 else "replayed"
            if result_object.get("disposition") != expected_disposition:
                raise ImportFailure(
                    "CPA managed OAuth import returned an inconsistent disposition"
                )
            validate_account(
                result_object.get("account"), tenant, "CPA managed OAuth import"
            )
        finally:
            # The full source descriptor and OAuth document must not outlive the call.
            payload.clear()
            result = None
        if status == 201:
            created_count += 1
        else:
            replayed_count += 1
    return created_count, replayed_count


def import_subscriptions(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    tenant: str,
    bridge_base_url: str,
    bridge_secret: str | None,
    inventory: Inventory,
    secrets: SecretStore,
) -> int:
    imported_count = 0
    for record in inventory.subscriptions:
        body: dict[str, object] = {
            "tenant_external_id": tenant,
            "bridge_base_url": bridge_base_url,
            "auth_files": [
                {
                    "filename": record.source_basename,
                    "document": secrets.get_document(record.document_ref),
                }
            ],
        }
        if bridge_secret is not None:
            body["bridge_secret"] = bridge_secret
        result = request_json(
            opener,
            method="POST",
            url=f"{base_url}/internal/v1/imports/cpa/subscription-accounts",
            token=token,
            label="CPA subscription account import",
            expected_status=201,
            body=body,
        )
        result_object = require_mapping(result, "CPA subscription import response")
        imported = result_object.get("imported")
        skipped = result_object.get("skipped")
        if not isinstance(imported, list) or len(imported) != 1 or skipped != []:
            raise ImportFailure("CPA subscription account was not imported exactly once")
        imported_item = require_mapping(imported[0], "CPA subscription import item")
        if imported_item.get("provider") != record.provider:
            raise ImportFailure("CPA subscription import returned another provider")
        validate_account(imported_item.get("account"), tenant, "CPA subscription import")
        imported_count += 1
    return imported_count


def desired_credential(record: DirectAccount, secrets: SecretStore) -> dict[str, object]:
    credential = secrets.get_string(record.secret_ref)
    return {
        "type": "api_key",
        "value": credential,
        "header": record.header,
        "prefix": record.prefix,
    }


def apply_direct_accounts(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    token: str,
    tenant: str,
    inventory: Inventory,
    secrets: SecretStore,
    existing: dict[str, dict[str, object]],
) -> tuple[int, int]:
    created_count = 0
    replayed_count = 0
    for record in inventory.direct:
        credential = desired_credential(record, secrets)
        account = existing.get(record.name)
        if account is None:
            created = request_json(
                opener,
                method="POST",
                url=f"{base_url}/internal/v1/upstreams",
                token=token,
                label="target upstream creation",
                expected_status=201,
                body={
                    "tenant_external_id": tenant,
                    "name": record.name,
                    "driver": record.driver,
                    "config": record.config,
                    "credential": credential,
                },
            )
            account = validate_account(created, tenant, "target upstream creation")
            if (
                account.get("name") != record.name
                or account.get("driver") != record.driver
                or account.get("config") != record.config
            ):
                raise ImportFailure("target upstream creation returned another account")
            created_count += 1
        else:
            replayed_count += 1
        account_id = account.get("id")
        if not isinstance(account_id, str):
            raise ImportFailure("target account identifier is invalid")
        idempotency_key = f"cpa-import-v1-{source_digest(record.source_id)[:48]}"
        rotated = request_json(
            opener,
            method="PUT",
            url=f"{base_url}/internal/v1/upstreams/{urllib.parse.quote(account_id, safe='')}/credential",
            token=token,
            label="target upstream credential convergence",
            expected_status=200,
            body={"credential": credential},
            idempotency_key=idempotency_key,
        )
        account = validate_account(rotated, tenant, "target upstream credential convergence")
        desired_status = "disabled" if record.disabled else "active"
        if account.get("status") != desired_status:
            updated_at = account.get("updated_at")
            if not isinstance(updated_at, int):
                raise ImportFailure("target account version is invalid")
            account = validate_account(
                request_json(
                    opener,
                    method="PATCH",
                    url=f"{base_url}/internal/v1/upstreams/{urllib.parse.quote(account_id, safe='')}",
                    token=token,
                    label="target upstream status convergence",
                    expected_status=200,
                    body={
                        "tenant_external_id": tenant,
                        "status": desired_status,
                        "expected_updated_at": updated_at,
                    },
                ),
                tenant,
                "target upstream status convergence",
            )
            if account.get("status") != desired_status:
                raise ImportFailure("target upstream status did not converge")
    return created_count, replayed_count


def read_token(path: str, label: str) -> str:
    value = decode_utf8(read_owner_only_file(path, label, MAX_SECRET_BYTES), label)
    if value.endswith("\n"):
        value = value[:-1]
    return secret_string(value, label)


def summary(
    mode: str,
    inventory: Inventory,
    *,
    created_count: int = 0,
    replayed_count: int = 0,
    imported_subscription_count: int = 0,
    created_managed_oauth_count: int = 0,
    replayed_managed_oauth_count: int = 0,
) -> dict[str, object]:
    managed_oauth_source_type_counts = {
        source_type: sum(
            record.source_type == source_type for record in inventory.managed_oauth
        )
        for source_type in sorted(
            {record.source_type for record in inventory.managed_oauth}
        )
    }
    return {
        "api_account_count": len(inventory.direct),
        "created_count": created_count,
        "created_managed_oauth_count": created_managed_oauth_count,
        "disabled_source_count": inventory.disabled_source_count,
        "imported_subscription_count": imported_subscription_count,
        "managed_oauth_account_count": len(inventory.managed_oauth),
        "managed_oauth_source_type_counts": managed_oauth_source_type_counts,
        "mode": mode,
        "replayed_count": replayed_count,
        "replayed_managed_oauth_count": replayed_managed_oauth_count,
        "subscription_account_count": len(inventory.subscriptions),
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Import real CPA config.yaml/auth-dir upstreams (dry-run by default)."
    )
    parser.add_argument("--config", required=True, help="Mounted mode-0600 CPA config.yaml.")
    parser.add_argument("--auth-dir", required=True, help="Mounted mode-0700 CPA auth directory.")
    parser.add_argument("--tenant", default="default", help="Target tenant external ID.")
    parser.add_argument("--apply", action="store_true", help="Perform control API writes.")
    parser.add_argument("--target-api-base-url", help="Private Token Center control base URL.")
    parser.add_argument("--service-token-file", help="Mode-0600 target service token file.")
    parser.add_argument("--bridge-base-url", help="CPA subscription bridge base URL.")
    parser.add_argument("--bridge-secret-file", help="Optional mode-0600 bridge secret file.")
    parser.add_argument("--ca-file", help="Optional CA bundle for the target control endpoint.")
    parser.add_argument(
        "--allow-http-loopback",
        action="store_true",
        help="Allow explicit HTTP loopback URLs for black-box tests only.",
    )
    return parser.parse_args(argv)


def run(argv: list[str]) -> int:
    args = parse_args(argv)
    if not TENANT_PATTERN.fullmatch(args.tenant):
        raise ImportFailure("target tenant external ID is invalid")
    inventory, secrets = build_inventory(
        args.config, args.auth_dir, args.allow_http_loopback
    )
    bridge_base_url = None
    if inventory.subscriptions:
        if args.bridge_base_url is None:
            raise ImportFailure("bridge base URL is required for CPA subscription accounts")
        bridge_base_url = public_url(
            args.bridge_base_url, "CPA subscription bridge base URL", args.allow_http_loopback
        )
    if not args.apply:
        print(json.dumps(summary("dry-run", inventory), separators=(",", ":"), sort_keys=True))
        return 0
    if not args.target_api_base_url or not args.service_token_file:
        raise ImportFailure("apply requires target API base URL and service token file")
    target_base_url = public_url(
        args.target_api_base_url, "target API base URL", args.allow_http_loopback
    )
    token = read_token(args.service_token_file, "target service token file")
    bridge_secret = None
    if args.bridge_secret_file:
        bridge_secret = read_token(args.bridge_secret_file, "subscription bridge secret file")
    opener = opener_for(args.ca_file)
    existing = preflight_target(
        opener, target_base_url, token, args.tenant, inventory
    )
    created_managed_oauth_count, replayed_managed_oauth_count = import_managed_oauth(
        opener,
        target_base_url,
        token,
        args.tenant,
        inventory,
        secrets,
    )
    imported_subscription_count = import_subscriptions(
        opener,
        target_base_url,
        token,
        args.tenant,
        bridge_base_url or "",
        bridge_secret,
        inventory,
        secrets,
    )
    created_count, replayed_count = apply_direct_accounts(
        opener,
        target_base_url,
        token,
        args.tenant,
        inventory,
        secrets,
        existing,
    )
    print(
        json.dumps(
            summary(
                "apply",
                inventory,
                created_count=created_count,
                replayed_count=replayed_count,
                imported_subscription_count=imported_subscription_count,
                created_managed_oauth_count=created_managed_oauth_count,
                replayed_managed_oauth_count=replayed_managed_oauth_count,
            ),
            separators=(",", ":"),
            sort_keys=True,
        )
    )
    return 0


def fail(message: str) -> NoReturn:
    print(f"CPA upstream import stopped: {message}", file=sys.stderr)
    raise SystemExit(2)


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[1:]))
    except ImportFailure as error:
        fail(str(error))
