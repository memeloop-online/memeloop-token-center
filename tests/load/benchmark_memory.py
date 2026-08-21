#!/usr/bin/env python3
"""Reproducible gateway/worker RSS and streaming acceptance harness.

Only Python's standard library is used.  The harness starts isolated control,
gateway, and worker processes over one temporary SQLite database, plus a local
streaming mock upstream.  Its JSON report is intended to be kept as CI or
release evidence.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import contextlib
import datetime as dt
import hashlib
import http.client
import json
import math
import os
import pathlib
import platform
import shutil
import signal
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid
from collections.abc import Iterable
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MIB = 1024 * 1024
RESPONSE_LIMIT_BYTES = 64 * MIB
MP4_PREFIX = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2"
ASSET_DECAY_COOLDOWN_SECONDS = 2.0
ASSET_PHASE_START_SAMPLE_SECONDS = 1.0
ASSET_PHASE_START_SAMPLE_INTERVAL = 0.1
ASSET_DOWNLOAD_CHUNK_BYTES = 256 * 1024
ASSET_DOWNLOAD_CHUNK_DELAY_SECONDS = 0.001
ASSET_RANGE_START = 32
ASSET_RANGE_BYTES = 4096


class HarnessFailure(RuntimeError):
    """A functional acceptance assertion failed."""


class PrerequisiteFailure(RuntimeError):
    """A local prerequisite or service startup failed."""


class MockState:
    def __init__(self) -> None:
        self.assets: dict[str, int] = {}
        self.image_raw_bytes = 11 * MIB
        self.active_streams = 0
        self.peak_streams = 0
        self.lock = threading.Lock()

    def begin_stream(self) -> None:
        with self.lock:
            self.active_streams += 1
            self.peak_streams = max(self.peak_streams, self.active_streams)

    def end_stream(self) -> None:
        with self.lock:
            self.active_streams -= 1

    def reset_stream_peak(self) -> None:
        with self.lock:
            if self.active_streams:
                raise HarnessFailure("cannot reset mock concurrency while a stream is active")
            self.peak_streams = 0

    def observed_stream_peak(self) -> int:
        with self.lock:
            return self.peak_streams


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: MockState

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _json_body(self) -> dict[str, Any]:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        value = json.loads(raw)
        if not isinstance(value, dict):
            raise ValueError("body is not an object")
        return value

    def _send_json(self, status: int, value: object) -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _stream_bytes(
        self,
        total: int,
        chunk_size: int,
        delay_ms: float,
        content_type: str = "application/octet-stream",
        prefix: bytes = b"",
    ) -> None:
        if len(prefix) > total:
            raise ValueError("stream prefix exceeds the configured response size")
        self.state.begin_stream()
        try:
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(total))
            self.end_headers()
            remaining = total
            try:
                if prefix:
                    self.wfile.write(prefix)
                    self.wfile.flush()
                    remaining -= len(prefix)
                block = b"x" * min(chunk_size, remaining)
                while remaining:
                    take = min(len(block), remaining)
                    self.wfile.write(block[:take])
                    self.wfile.flush()
                    remaining -= take
                    if delay_ms:
                        time.sleep(delay_ms / 1000.0)
            except (BrokenPipeError, ConnectionResetError):
                # Disconnect and response-cap tests intentionally close early.
                return
        finally:
            self.state.end_stream()

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        try:
            body = self._json_body()
        except (ValueError, json.JSONDecodeError):
            self._send_json(400, {"error": "invalid request"})
            return

        if self.path == "/v1/chat/completions":
            benchmark = body.get("benchmark", {})
            if isinstance(benchmark, dict) and benchmark.get("mode") in {
                "stream",
                "oversize",
            }:
                total = int(benchmark.get("bytes", 0))
                chunk = max(4096, min(int(benchmark.get("chunk_bytes", 262144)), MIB))
                delay_ms = float(benchmark.get("delay_ms", 0))
                self._stream_bytes(
                    total,
                    chunk,
                    delay_ms,
                    content_type="text/event-stream",
                )
                return
            self._send_json(
                200,
                {
                    "id": "chatcmpl-memory-benchmark",
                    "object": "chat.completion",
                    "model": body.get("model", "benchmark-text"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3},
                },
            )
            return

        if self.path == "/api/v3/contents/generations/tasks":
            asset_mib = int(body.get("benchmark_asset_mib", 100))
            job_id = f"bench-{asset_mib}-{uuid.uuid4().hex[:12]}"
            with self.state.lock:
                self.state.assets[job_id] = asset_mib * MIB
            self._send_json(200, {"id": job_id})
            return

        if self.path == "/v1/responses":
            result = base64.b64encode(b"x" * self.state.image_raw_bytes).decode("ascii")
            self._send_json(
                200,
                {
                    "id": "resp_memory_image",
                    "output": [
                        {
                            "type": "image_generation_call",
                            "id": "ig_memory_image",
                            "result": result,
                        }
                    ],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                },
            )
            return

        self._send_json(404, {"error": "mock route not found"})

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        task_prefix = "/api/v3/contents/generations/tasks/"
        asset_prefix = "/assets/"
        if self.path.startswith(task_prefix):
            job_id = self.path[len(task_prefix) :]
            with self.state.lock:
                asset_bytes = self.state.assets.get(job_id)
            if asset_bytes is None:
                self._send_json(404, {"error": "unknown job"})
                return
            host = self.headers.get("host", "127.0.0.1")
            self._send_json(
                200,
                {
                    "id": job_id,
                    "status": "succeeded",
                    "duration": 1,
                    "content": {"video_url": f"http://{host}/assets/{job_id}"},
                },
            )
            return
        if self.path.startswith(asset_prefix):
            job_id = self.path[len(asset_prefix) :]
            with self.state.lock:
                asset_bytes = self.state.assets.get(job_id)
            if asset_bytes is None:
                self._send_json(404, {"error": "unknown asset"})
                return
            # A small delay ensures the sampler observes the transfer without
            # making the 500 MiB acceptance profile unnecessarily slow.
            self._stream_bytes(
                asset_bytes,
                256 * 1024,
                0.25,
                content_type="video/mp4",
                prefix=MP4_PREFIX,
            )
            return
        self._send_json(404, {"error": "mock route not found"})


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(fraction * len(ordered)) - 1))
    return ordered[index]


def process_memory(pid: int) -> dict[str, float]:
    status: dict[str, int] = {}
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as source:
            for line in source:
                if line.startswith(("VmRSS:", "VmHWM:")):
                    name, value, _unit = line.split()
                    status[name.rstrip(":")] = int(value)
    except FileNotFoundError as error:
        raise HarnessFailure(f"process {pid} exited while memory was sampled") from error
    pss_kib = 0
    with contextlib.suppress(FileNotFoundError, PermissionError):
        with open(f"/proc/{pid}/smaps_rollup", encoding="utf-8") as source:
            for line in source:
                if line.startswith("Pss:"):
                    pss_kib = int(line.split()[1])
                    break
    return {
        "rss_mib": status.get("VmRSS", 0) / 1024.0,
        "high_water_mib": status.get("VmHWM", 0) / 1024.0,
        "pss_mib": pss_kib / 1024.0,
    }


class Sampler:
    def __init__(self, pids: dict[str, int], interval: float = 0.05) -> None:
        self.pids = pids
        self.interval = interval
        self.samples: list[dict[str, Any]] = []
        self.stop_event = threading.Event()
        self.first_sample = threading.Event()
        self.failure: HarnessFailure | None = None
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        started = time.monotonic()
        while not self.stop_event.is_set():
            sample: dict[str, Any] = {"elapsed_seconds": time.monotonic() - started}
            try:
                for name, pid in self.pids.items():
                    sample[name] = process_memory(pid)
                self.samples.append(sample)
                self.first_sample.set()
            except HarnessFailure as error:
                self.failure = error
                self.first_sample.set()
                return
            self.stop_event.wait(self.interval)

    def __enter__(self) -> "Sampler":
        self.thread.start()
        if not self.first_sample.wait(timeout=2):
            self.stop_event.set()
            self.thread.join(timeout=2)
            raise HarnessFailure("memory sampler did not produce its first sample")
        if self.failure is not None:
            raise self.failure
        return self

    def __exit__(self, exception_type: object, *_args: object) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)
        if exception_type is None and self.failure is not None:
            raise self.failure

    def max_current_rss(self, name: str) -> float:
        return max(
            (sample[name]["rss_mib"] for sample in self.samples),
            default=0.0,
        )

    def lifetime_high_water_evidence(self, name: str) -> dict[str, float]:
        if not self.samples:
            return {
                "start_rss_mib": 0.0,
                "end_rss_mib": 0.0,
                "growth_mib": 0.0,
            }
        start = self.samples[0][name]["high_water_mib"]
        end = self.samples[-1][name]["high_water_mib"]
        return {
            "start_rss_mib": round(start, 3),
            "end_rss_mib": round(end, 3),
            "growth_mib": round(max(0.0, end - start), 3),
        }


def memory_summary(
    pid: int,
    duration: float = 1.0,
    interval: float = 0.1,
) -> dict[str, float]:
    samples: list[dict[str, float]] = []
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        samples.append(process_memory(pid))
        time.sleep(interval)
    rss = [sample["rss_mib"] for sample in samples]
    pss = [sample["pss_mib"] for sample in samples if sample["pss_mib"] > 0]
    high_water = [sample["high_water_mib"] for sample in samples]
    return {
        "rss_mib_median": round(statistics.median(rss), 3),
        "rss_mib_p95": round(percentile(rss, 0.95), 3),
        "rss_mib_max": round(max(rss), 3),
        "pss_mib_median": round(statistics.median(pss), 3) if pss else 0.0,
        "lifetime_high_water_rss_mib": round(max(high_water), 3),
        "sample_count": len(samples),
    }


def post_decay_phase_start_memory(pid: int) -> dict[str, float]:
    """Measure a bounded phase baseline after jemalloc's one-second dirty decay."""
    before = process_memory(pid)
    time.sleep(ASSET_DECAY_COOLDOWN_SECONDS)
    summary = memory_summary(
        pid,
        ASSET_PHASE_START_SAMPLE_SECONDS,
        ASSET_PHASE_START_SAMPLE_INTERVAL,
    )
    return {
        "pre_cooldown_rss_mib": round(before["rss_mib"], 3),
        "cooldown_seconds": ASSET_DECAY_COOLDOWN_SECONDS,
        "sample_duration_seconds": ASSET_PHASE_START_SAMPLE_SECONDS,
        "sample_interval_seconds": ASSET_PHASE_START_SAMPLE_INTERVAL,
        **summary,
    }


def asset_gateway_rss_evidence(
    phase_peak_rss_mib: float,
    phase_start_rss_mib: float,
    original_idle_rss_mib: float,
) -> dict[str, float]:
    """Separate asset-phase growth from cumulative pressure since original idle."""
    return {
        "gateway_phase_delta_rss_mib": round(
            max(0.0, phase_peak_rss_mib - phase_start_rss_mib), 3
        ),
        "gateway_cumulative_delta_from_original_idle_mib": round(
            max(0.0, phase_peak_rss_mib - original_idle_rss_mib), 3
        ),
    }


def rss_slope_mib_per_minute(samples: list[dict[str, Any]], process: str) -> float:
    if len(samples) < 3:
        return 0.0
    tail = samples[len(samples) // 3 :]
    xs = [float(sample["elapsed_seconds"]) for sample in tail]
    ys = [float(sample[process]["rss_mib"]) for sample in tail]
    mean_x, mean_y = statistics.mean(xs), statistics.mean(ys)
    denominator = sum((value - mean_x) ** 2 for value in xs)
    if denominator == 0:
        return 0.0
    per_second = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys)) / denominator
    return per_second * 60.0


def api_request(
    base_url: str,
    method: str,
    path: str,
    token: str,
    payload: object | None = None,
    timeout: float = 30.0,
    extra_headers: dict[str, str] | None = None,
) -> tuple[int, bytes, dict[str, str]]:
    parsed = urllib.parse.urlsplit(base_url)
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    headers = {"authorization": f"Bearer {token}", "connection": "close"}
    if body is not None:
        headers["content-type"] = "application/json"
    if extra_headers is not None:
        headers.update(extra_headers)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        response_body = response.read()
        return response.status, response_body, {key.lower(): value for key, value in response.getheaders()}
    finally:
        connection.close()


def api_json(
    base_url: str,
    method: str,
    path: str,
    token: str,
    payload: object | None = None,
    expected: tuple[int, ...] = (200, 201, 202),
    timeout: float = 30.0,
) -> Any:
    status, body, _headers = api_request(base_url, method, path, token, payload, timeout)
    if status not in expected:
        message = body[:1000].decode(errors="replace")
        raise HarnessFailure(f"{method} {path} returned HTTP {status}: {message}")
    try:
        return json.loads(body)
    except json.JSONDecodeError as error:
        raise HarnessFailure(f"{method} {path} returned non-JSON success") from error


def wait_ready(url: str, process: subprocess.Popen[bytes], log_path: pathlib.Path) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            tail = log_path.read_text(errors="replace")[-4000:]
            raise PrerequisiteFailure(f"service exited with {process.returncode}:\n{tail}")
        try:
            status, _body, _headers = api_request(url, "GET", "/readyz", "unused", timeout=1)
            if status == 200:
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise PrerequisiteFailure(f"service at {url} did not become ready")


def run_migration(binary: pathlib.Path, environment: dict[str, str], log_path: pathlib.Path) -> None:
    with log_path.open("wb") as log:
        result = subprocess.run(
            [str(binary), "migrate"],
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            timeout=60,
            check=False,
        )
    if result.returncode != 0:
        tail = log_path.read_text(errors="replace")[-4000:]
        raise PrerequisiteFailure(f"database migration failed:\n{tail}")


def start_role(
    binary: pathlib.Path,
    role: str,
    port: int,
    environment: dict[str, str],
    log_path: pathlib.Path,
) -> subprocess.Popen[bytes]:
    role_environment = environment | {"MTC_LISTEN": f"127.0.0.1:{port}"}
    log = log_path.open("wb")
    process = subprocess.Popen(
        [str(binary), "serve", "--role", role],
        env=role_environment,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    log.close()
    return process


def stop_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=3)


def custom_model_route_payload(
    tenant: str,
    public_model: str,
    upstream_account_id: str,
    upstream_model: str,
    protocol: str,
) -> dict[str, object]:
    """Build a benchmark-only route that explicitly opts into a custom model."""
    return {
        "tenant_external_id": tenant,
        "public_model": public_model,
        "upstream_account_id": upstream_account_id,
        "upstream_model": upstream_model,
        "protocol": protocol,
        "priority": 0,
        "custom_model_confirmed": True,
    }


def seed(control_url: str, gateway_url: str, service_token: str, mock_url: str) -> str:
    tenant = "memory-benchmark"
    text_model = "benchmark-text"
    asset_model = "benchmark-seedance"
    image_model = "benchmark-image"
    text_upstream = api_json(
        control_url,
        "POST",
        "/internal/v1/upstreams",
        service_token,
        {
            "tenant_external_id": tenant,
            "name": "Memory benchmark text",
            "driver": "http-json",
            "config": {"base_url": mock_url},
            "credential": {"type": "none"},
        },
    )
    api_json(
        control_url,
        "POST",
        "/internal/v1/model-routes",
        service_token,
        custom_model_route_payload(
            tenant, text_model, text_upstream["id"], text_model, "openai"
        ),
    )
    api_json(
        control_url,
        "POST",
        f"/internal/v1/prices/USD/{text_model}",
        service_token,
        {"input_per_million": "0.01", "output_per_million": "0.02"},
    )
    asset_upstream = api_json(
        control_url,
        "POST",
        "/internal/v1/upstreams",
        service_token,
        {
            "tenant_external_id": tenant,
            "name": "Memory benchmark asset",
            "driver": "volcengine-seedance",
            "config": {"base_url": mock_url},
            "credential": {"type": "none"},
        },
    )
    api_json(
        control_url,
        "POST",
        "/internal/v1/model-routes",
        service_token,
        custom_model_route_payload(
            tenant, asset_model, asset_upstream["id"], asset_model, "generation"
        ),
    )
    api_json(
        control_url,
        "POST",
        f"/internal/v1/generation-prices/USD/{asset_model}",
        service_token,
        {"billing_unit": "second", "price_per_unit": "0.01"},
    )
    image_upstream = api_json(
        control_url,
        "POST",
        "/internal/v1/upstreams",
        service_token,
        {
            "tenant_external_id": tenant,
            "name": "Memory benchmark image",
            "driver": "http-json",
            "config": {
                "base_url": mock_url,
                "image_api_mode": "responses-tool",
                "image_main_model": "benchmark-main",
            },
            "credential": {"type": "none"},
        },
    )
    api_json(
        control_url,
        "POST",
        "/internal/v1/model-routes",
        service_token,
        custom_model_route_payload(
            tenant,
            image_model,
            image_upstream["id"],
            "benchmark-image-upstream",
            "generation",
        ),
    )
    api_json(
        control_url,
        "POST",
        f"/internal/v1/generation-prices/USD/{image_model}",
        service_token,
        {"billing_unit": "image", "price_per_unit": "0.01"},
    )
    issued = api_json(
        control_url,
        "POST",
        "/internal/v1/keys",
        service_token,
        {
            "tenant_external_id": tenant,
            "principal_external_id": "memory-benchmark-user",
            "alias": "Memory benchmark credential",
            "currency": "USD",
            "initial_balance": "100000",
            "policy": {
                "allowed_models": [text_model, asset_model, image_model],
                "requests_per_minute": 1000000,
                "tokens_per_minute": 1000000000,
                "max_concurrency": 32,
                "daily_budget": None,
                "weekly_budget": None,
                "lifetime_budget": None,
            },
        },
    )
    key = str(issued["key"])
    # Prove the key is accepted by the gateway before measuring it.
    small_chat(gateway_url, key)
    return key


def bulk_insert(
    connection: sqlite3.Connection,
    sql: str,
    rows: Iterable[tuple[object, ...]],
    batch_size: int = 1000,
) -> None:
    batch: list[tuple[object, ...]] = []
    for row in rows:
        batch.append(row)
        if len(batch) == batch_size:
            connection.executemany(sql, batch)
            batch.clear()
    if batch:
        connection.executemany(sql, batch)


def scale_uuid(namespace: int, sequence: int) -> str:
    return str(uuid.UUID(int=(namespace << 112) | sequence))


def seed_control_scale(database: pathlib.Path, row_count: int = 100000) -> None:
    connection = sqlite3.connect(database, timeout=60)
    try:
        connection.execute("PRAGMA busy_timeout = 60000")
        tenant_id = connection.execute(
            "SELECT id FROM tenants WHERE external_id = ?", ("memory-benchmark",)
        ).fetchone()[0]
        bulk_insert(
            connection,
            "INSERT INTO tenants (id, external_id, created_at) VALUES (?, ?, ?)",
            (
                (scale_uuid(5, index), f"scale-tenant-{index:06d}", index)
                for index in range(1, row_count + 1)
            ),
        )
        bulk_insert(
            connection,
            "INSERT INTO upstream_accounts "
            "(id, tenant_id, name, driver, auth_kind, config_json, status, "
            "credential_generation, created_at, updated_at) "
            "VALUES (?, ?, ?, 'http-json', 'none', '{}', 'active', 1, ?, ?)",
            (
                (
                    scale_uuid(6, index),
                    tenant_id,
                    f"scale-upstream-{index:06d}",
                    index,
                    index,
                )
                for index in range(1, row_count + 1)
            ),
        )
        bulk_insert(
            connection,
            "INSERT INTO model_routes "
            "(id, tenant_id, public_model, upstream_account_id, upstream_model, "
            "protocol, priority, enabled, created_at, updated_at) "
            "VALUES (?, ?, ?, ?, ?, 'openai', 0, 1, ?, ?)",
            (
                (
                    scale_uuid(7, index),
                    tenant_id,
                    f"scale-model-{index:06d}",
                    scale_uuid(6, index),
                    f"scale-model-{index:06d}",
                    index,
                    index,
                )
                for index in range(1, row_count + 1)
            ),
        )
        bulk_insert(
            connection,
            "INSERT INTO service_principals "
            "(id, name, status, credential_generation, created_at, updated_at) "
            "VALUES (?, ?, 'active', 1, ?, ?)",
            (
                (
                    scale_uuid(8, index),
                    f"scale-service-{index:06d}",
                    index,
                    index,
                )
                for index in range(1, row_count + 1)
            ),
        )
        bulk_insert(
            connection,
            "INSERT INTO service_credentials "
            "(id, service_principal_id, generation, secret_hash, fingerprint, "
            "scopes_json, tenant_external_id, created_at, revoked_at) "
            "VALUES (?, ?, 1, ?, ?, '[\"requests:read\"]', NULL, ?, NULL)",
            (
                (
                    scale_uuid(9, index),
                    scale_uuid(8, index),
                    b"scale-fixture-hash",
                    f"scale-fingerprint-{index:06d}",
                    index,
                )
                for index in range(1, row_count + 1)
            ),
        )
        connection.commit()
        for table in ("tenants", "upstream_accounts", "model_routes", "service_principals"):
            connection.execute(f"ANALYZE {table}")
        connection.commit()
    finally:
        connection.close()


def run_control_scale(
    control_url: str,
    service_token: str,
    control_pid: int,
    idle_control_rss: float,
) -> dict[str, Any]:
    paths = [
        "/internal/v1/tenants?limit=1000000",
        "/internal/v1/service-tokens?limit=1000000",
        "/internal/v1/upstreams?limit=1000000",
        "/internal/v1/model-routes?limit=1000000",
    ]

    def fetch(path: str) -> dict[str, Any]:
        started = time.monotonic()
        status, body, _headers = api_request(
            control_url, "GET", path, service_token, timeout=60
        )
        try:
            rows = json.loads(body)
        except json.JSONDecodeError as error:
            raise HarnessFailure(f"control scale path returned invalid JSON: {path}") from error
        if status != 200 or not isinstance(rows, list):
            raise HarnessFailure(f"control scale path failed with HTTP {status}: {path}")
        return {
            "path": path,
            "status": status,
            "rows": len(rows),
            "response_bytes": len(body),
            "latency_ms": round((time.monotonic() - started) * 1000, 3),
        }

    started = time.monotonic()
    with Sampler({"control": control_pid}) as sampler:
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            pages = list(pool.map(fetch, paths * 4))
    peak = sampler.max_current_rss("control")
    return {
        "fixture_rows_per_resource": 100000,
        "concurrency": 16,
        "pages": pages,
        "maximum_page_rows": max(page["rows"] for page in pages),
        "maximum_response_bytes": max(page["response_bytes"] for page in pages),
        "maximum_latency_ms": max(page["latency_ms"] for page in pages),
        "duration_seconds": round(time.monotonic() - started, 3),
        "control_peak_rss_mib": round(peak, 3),
        "control_delta_rss_mib": round(peak - idle_control_rss, 3),
        "sample_count": len(sampler.samples),
    }


def chat_payload(mode: str = "small", byte_count: int = 0, delay_ms: float = 0) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "model": "benchmark-text",
        "messages": [{"role": "user", "content": "memory benchmark"}],
        "max_tokens": 1,
    }
    if mode != "small":
        payload["benchmark"] = {
            "mode": mode,
            "bytes": byte_count,
            "chunk_bytes": 256 * 1024,
            "delay_ms": delay_ms,
        }
    return payload


def small_chat(gateway_url: str, key: str) -> int:
    status, body, _headers = api_request(
        gateway_url,
        "POST",
        "/v1/chat/completions",
        key,
        chat_payload(),
        timeout=30,
    )
    if status != 200 or not body:
        raise HarnessFailure(f"small chat failed with HTTP {status}")
    return len(body)


def stream_chat(gateway_url: str, key: str, byte_count: int, delay_ms: float = 0) -> int:
    status, body, _headers = api_request(
        gateway_url,
        "POST",
        "/v1/chat/completions",
        key,
        chat_payload("stream", byte_count, delay_ms),
        timeout=180,
    )
    if status != 200 or len(body) != byte_count:
        raise HarnessFailure(
            f"stream returned HTTP {status} and {len(body)} bytes; expected {byte_count}"
        )
    return len(body)


def run_synchronous_images(
    gateway_url: str,
    key: str,
    gateway_pid: int,
    idle_gateway_rss: float,
) -> dict[str, Any]:
    def generate(index: int) -> int:
        status, body, _headers = api_request(
            gateway_url,
            "POST",
            "/v1/images/generations",
            key,
            {
                "model": "benchmark-image",
                "prompt": "bounded memory image",
                "n": 1,
                "response_format": "b64_json",
            },
            timeout=120,
            extra_headers={"idempotency-key": f"memory-image-{index}-{uuid.uuid4()}"},
        )
        if status != 200:
            raise HarnessFailure(f"synchronous image failed with HTTP {status}")
        response = json.loads(body)
        if len(response.get("data", [])) != 1:
            raise HarnessFailure("synchronous image did not return exactly one result")
        return len(body)

    started = time.monotonic()
    with Sampler({"gateway": gateway_pid}) as sampler:
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            response_bytes = list(pool.map(generate, range(2)))
    peak = sampler.max_current_rss("gateway")
    return {
        "concurrency": 2,
        "responses": len(response_bytes),
        "maximum_response_bytes": max(response_bytes),
        "duration_seconds": round(time.monotonic() - started, 3),
        "gateway_peak_rss_mib": round(peak, 3),
        "gateway_delta_rss_mib": round(peak - idle_gateway_rss, 3),
        "sample_count": len(sampler.samples),
    }


def disconnect_chat(gateway_url: str, key: str, byte_count: int) -> int:
    parsed = urllib.parse.urlsplit(gateway_url)
    payload = json.dumps(chat_payload("stream", byte_count, 0.5), separators=(",", ":")).encode()
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=30)
    connection.request(
        "POST",
        "/v1/chat/completions",
        body=payload,
        headers={
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
            "connection": "close",
        },
    )
    response = connection.getresponse()
    received = len(response.read(64 * 1024))
    connection.close()
    return received


def oversize_chat(gateway_url: str, key: str, byte_count: int) -> tuple[int, int, str | None]:
    parsed = urllib.parse.urlsplit(gateway_url)
    payload = json.dumps(chat_payload("oversize", byte_count), separators=(",", ":")).encode()
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=180)
    received = 0
    error_name: str | None = None
    status = 0
    try:
        connection.request(
            "POST",
            "/v1/chat/completions",
            body=payload,
            headers={
                "authorization": f"Bearer {key}",
                "content-type": "application/json",
                "connection": "close",
            },
        )
        response = connection.getresponse()
        status = response.status
        while True:
            chunk = response.read(256 * 1024)
            if not chunk:
                break
            received += len(chunk)
    except (http.client.HTTPException, OSError) as error:
        error_name = type(error).__name__
    finally:
        connection.close()
    return status, received, error_name


def wait_for_request_errors(
    gateway_url: str,
    key: str,
    error_code: str,
    minimum: int,
    timeout: float = 20,
) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        values = api_json(
            gateway_url,
            "GET",
            "/self/v1/requests?"
            + urllib.parse.urlencode(
                {"status": "error", "error_code": error_code, "limit": 100}
            ),
            key,
        )
        if isinstance(values, list) and len(values) >= minimum:
            return len(values)
        time.sleep(0.2)
    return 0


def archive_bytes(root: pathlib.Path) -> int:
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def archive_inventory(root: pathlib.Path) -> dict[str, int]:
    return {
        path.relative_to(root).as_posix(): path.stat().st_size
        for path in root.rglob("*")
        if path.is_file()
    }


def expected_mock_asset_sha256(total_bytes: int) -> str:
    if total_bytes < len(MP4_PREFIX):
        raise ValueError("mock asset is smaller than its MP4 prefix")
    digest = hashlib.sha256()
    digest.update(MP4_PREFIX)
    remaining = total_bytes - len(MP4_PREFIX)
    block = b"x" * MIB
    while remaining:
        take = min(remaining, len(block))
        digest.update(block[:take])
        remaining -= take
    return digest.hexdigest()


def stream_download_to_hash(
    base_url: str,
    path: str,
    token: str,
    *,
    range_header: str | None = None,
    timeout: float = 180.0,
    chunk_delay_seconds: float = 0.0,
) -> dict[str, Any]:
    """Consume a response into a bounded SHA-256/count sink."""
    parsed = urllib.parse.urlsplit(base_url)
    headers = {
        "authorization": f"Bearer {token}",
        "connection": "close",
    }
    if range_header is not None:
        headers["range"] = range_header
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout)
    digest = hashlib.sha256()
    received = 0
    maximum_chunk = 0
    try:
        connection.request("GET", path, headers=headers)
        response = connection.getresponse()
        response_headers = {
            key.lower(): value for key, value in response.getheaders()
        }
        while True:
            chunk = response.read(ASSET_DOWNLOAD_CHUNK_BYTES)
            if not chunk:
                break
            digest.update(chunk)
            received += len(chunk)
            maximum_chunk = max(maximum_chunk, len(chunk))
            if chunk_delay_seconds:
                time.sleep(chunk_delay_seconds)
        return {
            "status": response.status,
            "bytes_received": received,
            "sha256": digest.hexdigest(),
            "maximum_sink_chunk_bytes": maximum_chunk,
            "headers": response_headers,
        }
    finally:
        connection.close()


def verify_full_asset_download(download: dict[str, Any], expected_bytes: int) -> None:
    headers = download["headers"]
    expected_sha256 = expected_mock_asset_sha256(expected_bytes)
    valid = (
        download["status"] == 200
        and download["bytes_received"] == expected_bytes
        and download["sha256"] == expected_sha256
        and headers.get("content-length") == str(expected_bytes)
        and headers.get("content-type") == "video/mp4"
        and headers.get("accept-ranges") == "bytes"
        and "no-store"
        in {value.strip() for value in headers.get("cache-control", "").split(",")}
        and headers.get("content-disposition", "").startswith("attachment;")
        and download["maximum_sink_chunk_bytes"] <= ASSET_DOWNLOAD_CHUNK_BYTES
    )
    if not valid:
        raise HarnessFailure(
            "archived asset full download failed status/size/hash/header validation: "
            + json.dumps(download, sort_keys=True)
        )
    download["expected_sha256"] = expected_sha256


def verify_asset_range_download(download: dict[str, Any], expected_bytes: int) -> None:
    range_end = ASSET_RANGE_START + ASSET_RANGE_BYTES - 1
    headers = download["headers"]
    expected_sha256 = hashlib.sha256(b"x" * ASSET_RANGE_BYTES).hexdigest()
    valid = (
        download["status"] == 206
        and download["bytes_received"] == ASSET_RANGE_BYTES
        and download["sha256"] == expected_sha256
        and headers.get("content-length") == str(ASSET_RANGE_BYTES)
        and headers.get("content-range")
        == f"bytes {ASSET_RANGE_START}-{range_end}/{expected_bytes}"
        and headers.get("content-type") == "video/mp4"
        and headers.get("accept-ranges") == "bytes"
        and "no-store"
        in {value.strip() for value in headers.get("cache-control", "").split(",")}
        and download["maximum_sink_chunk_bytes"] <= ASSET_DOWNLOAD_CHUNK_BYTES
    )
    if not valid:
        raise HarnessFailure(
            "archived asset bounded range failed status/size/hash/header validation: "
            + json.dumps(download, sort_keys=True)
        )
    download["expected_sha256"] = expected_sha256


def run_asset(
    gateway_url: str,
    key: str,
    asset_mib: int,
    pids: dict[str, int],
    archive_root: pathlib.Path,
) -> dict[str, Any]:
    archive_before = archive_bytes(archive_root)
    inventory_before = archive_inventory(archive_root)
    deadline = time.monotonic() + max(120, asset_mib * 2)
    started = time.monotonic()
    final: dict[str, Any] | None = None
    with Sampler(pids) as sampler:
        job = api_json(
            gateway_url,
            "POST",
            "/v1/videos/generations",
            key,
            {
                "model": "benchmark-seedance",
                "input": {"duration": 1, "benchmark_asset_mib": asset_mib},
            },
        )
        job_id = str(job["job_id"])
        while time.monotonic() < deadline:
            candidate = api_json(
                gateway_url,
                "GET",
                f"/self/v1/generations/{job_id}",
                key,
            )
            if candidate["status"] in {"succeeded", "failed", "cancelled"}:
                final = candidate
                break
            time.sleep(0.1)
        if final is None:
            raise HarnessFailure("large asset generation did not finish before timeout")
        assets = final.get("assets")
        if not isinstance(assets, list) or len(assets) != 1:
            raise HarnessFailure("large asset generation did not expose exactly one asset")
        asset_id = str(assets[0].get("asset_id", ""))
        if not asset_id:
            raise HarnessFailure("large asset generation omitted its archived asset id")
        expected_bytes = asset_mib * MIB
        asset_path = f"/self/v1/generations/{job_id}/assets/{asset_id}"
        range_end = ASSET_RANGE_START + ASSET_RANGE_BYTES - 1
        with Sampler({"gateway": pids["gateway"]}) as download_sampler:
            full_download = stream_download_to_hash(
                gateway_url,
                asset_path,
                key,
                timeout=max(180, asset_mib * 2),
                chunk_delay_seconds=ASSET_DOWNLOAD_CHUNK_DELAY_SECONDS,
            )
            verify_full_asset_download(full_download, expected_bytes)
            range_download = stream_download_to_hash(
                gateway_url,
                asset_path,
                key,
                range_header=f"bytes={ASSET_RANGE_START}-{range_end}",
            )
            verify_asset_range_download(range_download, expected_bytes)
    archive_after = archive_bytes(archive_root)
    inventory_after = archive_inventory(archive_root)
    new_asset_objects = sorted(
        path
        for path, size in inventory_after.items()
        if size == expected_bytes and inventory_before.get(path) != size
    )
    return {
        "job_id": job_id,
        "status": final["status"],
        "error_code": final.get("error_code"),
        "asset_id": asset_id,
        "asset_mib": asset_mib,
        "expected_asset_bytes": expected_bytes,
        "archive_bytes_before": archive_before,
        "archive_bytes_after": archive_after,
        "archive_growth_bytes": archive_after - archive_before,
        "exact_size_asset_objects": new_asset_objects,
        "duration_seconds": round(time.monotonic() - started, 3),
        "gateway_peak_rss_mib": round(sampler.max_current_rss("gateway"), 3),
        "worker_peak_rss_mib": round(sampler.max_current_rss("worker"), 3),
        "gateway_lifetime_high_water": sampler.lifetime_high_water_evidence("gateway"),
        "worker_lifetime_high_water": sampler.lifetime_high_water_evidence("worker"),
        "sample_count": len(sampler.samples),
        "download": {
            "route": asset_path,
            "full": full_download,
            "range": range_download,
            "gateway_peak_rss_mib": round(
                download_sampler.max_current_rss("gateway"), 3
            ),
            "gateway_lifetime_high_water": download_sampler.lifetime_high_water_evidence(
                "gateway"
            ),
            "sample_count": len(download_sampler.samples),
        },
    }


def run_soak(
    gateway_url: str,
    key: str,
    seconds: int,
    concurrency: int,
    target_rps: float,
    gateway_pid: int,
) -> dict[str, Any]:
    deadline = time.monotonic() + seconds
    lock = threading.Lock()
    successes = 0
    failures = 0
    latencies_ms: list[float] = []
    per_worker_pause = concurrency / target_rps if target_rps > 0 else 0

    def worker() -> None:
        nonlocal successes, failures
        while time.monotonic() < deadline:
            started = time.monotonic()
            try:
                small_chat(gateway_url, key)
                ok = True
            except (HarnessFailure, OSError, http.client.HTTPException):
                ok = False
            elapsed = time.monotonic() - started
            with lock:
                if ok:
                    successes += 1
                    latencies_ms.append(elapsed * 1000)
                else:
                    failures += 1
            if per_worker_pause > elapsed:
                time.sleep(per_worker_pause - elapsed)

    started = time.monotonic()
    with Sampler({"gateway": gateway_pid}, interval=0.2) as sampler:
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            futures = [pool.submit(worker) for _ in range(concurrency)]
            for future in futures:
                future.result()
    elapsed = time.monotonic() - started
    return {
        "configured_seconds": seconds,
        "duration_seconds": round(elapsed, 3),
        "target_rps": target_rps,
        "concurrency": concurrency,
        "successes": successes,
        "failures": failures,
        "achieved_rps": round(successes / elapsed, 3) if elapsed else 0.0,
        "latency_ms_p50": round(percentile(latencies_ms, 0.50), 3),
        "latency_ms_p95": round(percentile(latencies_ms, 0.95), 3),
        "latency_ms_p99": round(percentile(latencies_ms, 0.99), 3),
        "gateway_peak_rss_mib": round(sampler.max_current_rss("gateway"), 3),
        "gateway_lifetime_high_water": sampler.lifetime_high_water_evidence("gateway"),
        "gateway_rss_slope_mib_per_minute": round(
            rss_slope_mib_per_minute(sampler.samples, "gateway"), 3
        ),
        "sample_count": len(sampler.samples),
    }


def git_revision(repository: pathlib.Path) -> str | None:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repository,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def git_dirty(repository: pathlib.Path) -> bool | None:
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=repository,
        capture_output=True,
        text=True,
        check=False,
    )
    return bool(result.stdout) if result.returncode == 0 else None


def file_sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(MIB):
            digest.update(chunk)
    return digest.hexdigest()


def write_json_report(output: pathlib.Path, report: dict[str, Any]) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


def failure_report(
    args: argparse.Namespace,
    exit_code: int,
    error_kind: str,
    error: BaseException,
) -> dict[str, Any]:
    repository = pathlib.Path(__file__).resolve().parents[2]
    binary = (args.binary or repository / "target/release/memeloop-token-center").resolve()
    report: dict[str, Any] = {
        "schema_version": 2,
        "benchmark": "memeloop-token-center-memory",
        "profile": args.profile,
        "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "git_revision": git_revision(repository),
        "git_dirty": git_dirty(repository),
        "binary": str(binary),
        "passed": False,
        "exit_code": exit_code,
        "error_kind": error_kind,
        "error": str(error)[:4000],
    }
    if binary.is_file():
        with contextlib.suppress(OSError):
            report["binary_sha256"] = file_sha256(binary)
            report["binary_mtime"] = dt.datetime.fromtimestamp(
                binary.stat().st_mtime, tz=dt.timezone.utc
            ).isoformat()
    return report


def write_failure_report(args: argparse.Namespace, report: dict[str, Any]) -> None:
    repository = pathlib.Path(__file__).resolve().parents[2]
    output = (
        args.output or repository / "tests/load/results/memory-latest.json"
    ).resolve()
    with contextlib.suppress(OSError):
        write_json_report(output, report)


def total_memory_mib() -> float | None:
    with contextlib.suppress(OSError, ValueError):
        with open("/proc/meminfo", encoding="utf-8") as source:
            for line in source:
                if line.startswith("MemTotal:"):
                    return round(int(line.split()[1]) / 1024.0, 3)
    return None


def cgroup_memory_limit_mib() -> float | None:
    for filename in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        with contextlib.suppress(OSError, ValueError):
            value = pathlib.Path(filename).read_text().strip()
            if value != "max":
                limit = int(value)
                # Some v1 hosts expose an effectively-unbounded sentinel.
                if limit < 1 << 60:
                    return round(limit / MIB, 3)
    return None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("short", "acceptance"), default="short")
    parser.add_argument("--binary", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--asset-mib", type=int)
    parser.add_argument("--soak-seconds", type=int)
    parser.add_argument("--concurrency", type=int)
    parser.add_argument("--stream-mib", type=int)
    parser.add_argument("--target-rps", type=float, default=20.0)
    parser.add_argument("--idle-max-mib", type=float, default=96.0)
    parser.add_argument("--stream-delta-max-mib", type=float, default=128.0)
    parser.add_argument("--control-list-delta-max-mib", type=float, default=64.0)
    parser.add_argument("--image-delta-max-mib", type=float, default=128.0)
    parser.add_argument("--asset-gateway-delta-max-mib", type=float, default=96.0)
    parser.add_argument("--asset-worker-delta-max-mib", type=float, default=192.0)
    parser.add_argument("--retained-delta-max-mib", type=float, default=64.0)
    parser.add_argument("--soak-slope-max-mib-per-minute", type=float, default=2.0)
    parser.add_argument("--gateway-limit-mib", type=float, default=256.0)
    parser.add_argument("--gateway-headroom-mib", type=float, default=32.0)
    parser.add_argument("--comparison-rss-mib", type=float, default=1024.0)
    parser.add_argument("--comparison-max-ratio", type=float, default=0.25)
    return parser.parse_args()


def profile_values(args: argparse.Namespace) -> dict[str, int]:
    defaults = {
        "short": {"asset_mib": 100, "soak_seconds": 30, "concurrency": 4, "stream_mib": 4},
        "acceptance": {
            "asset_mib": 500,
            "soak_seconds": 900,
            "concurrency": 12,
            "stream_mib": 16,
        },
    }[args.profile]
    return {
        name: int(getattr(args, name) if getattr(args, name) is not None else value)
        for name, value in defaults.items()
    }


def check(name: str, actual: object, operator: str, expected: object, passed: bool) -> dict[str, Any]:
    return {
        "name": name,
        "actual": actual,
        "operator": operator,
        "expected": expected,
        "passed": bool(passed),
    }


def execute(args: argparse.Namespace) -> tuple[int, dict[str, Any]]:
    repository = pathlib.Path(__file__).resolve().parents[2]
    values = profile_values(args)
    if not 1 <= values["concurrency"] <= 16:
        raise PrerequisiteFailure("concurrency must be between 1 and the gateway cap of 16")
    if not 100 <= values["asset_mib"] <= 500:
        raise PrerequisiteFailure("asset-mib must be in the MM-05 acceptance range 100..500")
    if values["soak_seconds"] < 1:
        raise PrerequisiteFailure("soak-seconds must be positive")
    if values["stream_mib"] < 1:
        raise PrerequisiteFailure("stream-mib must be positive")
    if args.profile == "acceptance" and (
        values["asset_mib"] != 500
        or values["soak_seconds"] < 900
        or values["concurrency"] < 12
        or values["stream_mib"] < 16
    ):
        raise PrerequisiteFailure(
            "acceptance profile requires a 500 MiB asset, at least a 900 second soak, "
            "at least 12 concurrent streams, and at least 16 MiB per stream"
        )
    numeric_limits = {
        "target-rps": args.target_rps,
        "idle-max-mib": args.idle_max_mib,
        "stream-delta-max-mib": args.stream_delta_max_mib,
        "control-list-delta-max-mib": args.control_list_delta_max_mib,
        "image-delta-max-mib": args.image_delta_max_mib,
        "asset-gateway-delta-max-mib": args.asset_gateway_delta_max_mib,
        "asset-worker-delta-max-mib": args.asset_worker_delta_max_mib,
        "retained-delta-max-mib": args.retained_delta_max_mib,
        "soak-slope-max-mib-per-minute": args.soak_slope_max_mib_per_minute,
        "gateway-limit-mib": args.gateway_limit_mib,
        "gateway-headroom-mib": args.gateway_headroom_mib,
        "comparison-rss-mib": args.comparison_rss_mib,
        "comparison-max-ratio": args.comparison_max_ratio,
    }
    invalid_limits = [
        name for name, value in numeric_limits.items() if not math.isfinite(value) or value <= 0
    ]
    if invalid_limits:
        raise PrerequisiteFailure(
            "numeric limits must be finite and positive: " + ", ".join(invalid_limits)
        )
    if args.comparison_rss_mib <= 0 or not 0 < args.comparison_max_ratio <= 1:
        raise PrerequisiteFailure(
            "comparison-rss-mib must be positive and comparison-max-ratio must be in (0, 1]"
        )
    gateway_budget_mib = args.gateway_limit_mib - args.gateway_headroom_mib
    if args.gateway_limit_mib <= 0 or not 0 < args.gateway_headroom_mib < args.gateway_limit_mib:
        raise PrerequisiteFailure(
            "gateway-limit-mib must be positive and gateway-headroom-mib must be in (0, limit)"
        )
    if args.idle_max_mib >= gateway_budget_mib:
        raise PrerequisiteFailure(
            "idle-max-mib must leave room below the gateway limit/headroom budget"
        )
    binary = (args.binary or repository / "target/release/memeloop-token-center").resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise PrerequisiteFailure(
            f"executable not found: {binary}; run cargo build --release --bin memeloop-token-center"
        )
    if not pathlib.Path("/proc/self/status").exists():
        raise PrerequisiteFailure("RSS acceptance requires Linux /proc")
    binary_sha256 = file_sha256(binary)
    binary_mtime = dt.datetime.fromtimestamp(
        binary.stat().st_mtime, tz=dt.timezone.utc
    ).isoformat()

    output = args.output or repository / "tests/load/results/memory-latest.json"
    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    service_token = "benchmark-bootstrap-token-not-for-production"
    processes: dict[str, subprocess.Popen[bytes]] = {}
    logs: dict[str, pathlib.Path] = {}
    mock_server: ThreadingHTTPServer | None = None
    started_at = dt.datetime.now(dt.timezone.utc)

    with tempfile.TemporaryDirectory(prefix="mtc-memory-benchmark-") as directory:
        temporary = pathlib.Path(directory)
        archive_root = temporary / "archive"
        archive_root.mkdir()
        database = temporary / "benchmark.db"
        environment = {
            name: os.environ[name]
            for name in (
                "HOME",
                "LANG",
                "LC_ALL",
                "NO_COLOR",
                "PATH",
                "RUST_BACKTRACE",
                "SSL_CERT_DIR",
                "SSL_CERT_FILE",
                "TMPDIR",
                "TZ",
            )
            if name in os.environ
        } | {
            "MTC_DATABASE_URL": f"sqlite://{database}?mode=rwc",
            "MTC_DATABASE_MAX_CONNECTIONS": "2",
            "MTC_KEY_PEPPER": "memory-benchmark-pepper-has-at-least-32-bytes",
            "MTC_SERVICE_TOKEN": service_token,
            "MTC_ARCHIVE_BACKEND": "filesystem",
            "MTC_ARCHIVE_PATH": str(archive_root),
            "MTC_ALLOW_OAUTH_LOOPBACK": "true",
            "MTC_RUN_MIGRATIONS_ON_START": "true",
            "RUST_LOG": os.environ.get("RUST_LOG", "warn"),
        }
        migration_log = temporary / "migration.log"
        run_migration(binary, environment, migration_log)
        environment["MTC_RUN_MIGRATIONS_ON_START"] = "false"

        state = MockState()
        handler = type("BoundMockHandler", (MockHandler,), {"state": state})
        mock_server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        mock_thread = threading.Thread(target=mock_server.serve_forever, daemon=True)
        mock_thread.start()
        mock_url = f"http://127.0.0.1:{mock_server.server_port}"

        ports = {role: free_port() for role in ("control", "gateway", "worker")}
        try:
            for role in ("control", "gateway", "worker"):
                logs[role] = temporary / f"{role}.log"
                processes[role] = start_role(binary, role, ports[role], environment, logs[role])
            urls = {role: f"http://127.0.0.1:{ports[role]}" for role in ports}
            for role in ("control", "gateway", "worker"):
                wait_ready(urls[role], processes[role], logs[role])

            key = seed(urls["control"], urls["gateway"], service_token, mock_url)
            for _ in range(3):
                small_chat(urls["gateway"], key)
            time.sleep(1)
            idle = {
                role: memory_summary(process.pid, 1.0) for role, process in processes.items()
            }
            idle_gateway = idle["gateway"]["rss_mib_median"]
            idle_worker = idle["worker"]["rss_mib_median"]
            idle_control = idle["control"]["rss_mib_median"]

            seed_control_scale(database)
            control_scale = run_control_scale(
                urls["control"],
                service_token,
                processes["control"].pid,
                idle_control,
            )

            stream_bytes = values["stream_mib"] * MIB
            stream_started = time.monotonic()
            stream_pids = {"gateway": processes["gateway"].pid}
            state.reset_stream_peak()
            stream_start_barrier = threading.Barrier(values["concurrency"])

            def run_concurrent_stream(_index: int) -> int:
                stream_start_barrier.wait(timeout=10)
                return stream_chat(urls["gateway"], key, stream_bytes, delay_ms=5.0)

            with Sampler(stream_pids) as stream_sampler:
                with concurrent.futures.ThreadPoolExecutor(
                    max_workers=values["concurrency"]
                ) as pool:
                    stream_results = list(
                        pool.map(run_concurrent_stream, range(values["concurrency"]))
                    )
            stream_peak = stream_sampler.max_current_rss("gateway")
            stream = {
                "concurrency": values["concurrency"],
                "bytes_per_response": stream_bytes,
                "bytes_received": sum(stream_results),
                "observed_peak_concurrency": state.observed_stream_peak(),
                "duration_seconds": round(time.monotonic() - stream_started, 3),
                "gateway_peak_rss_mib": round(stream_peak, 3),
                "gateway_delta_rss_mib": round(stream_peak - idle_gateway, 3),
                "gateway_lifetime_high_water": stream_sampler.lifetime_high_water_evidence(
                    "gateway"
                ),
                "sample_count": len(stream_sampler.samples),
            }

            synchronous_image = run_synchronous_images(
                urls["gateway"], key, processes["gateway"].pid, idle_gateway
            )

            disconnect_attempts = max(2, min(4, values["concurrency"]))
            with Sampler({"gateway": processes["gateway"].pid}) as disconnect_sampler:
                with concurrent.futures.ThreadPoolExecutor(
                    max_workers=disconnect_attempts
                ) as pool:
                    disconnected = list(
                        pool.map(
                            lambda _index: disconnect_chat(urls["gateway"], key, 16 * MIB),
                            range(disconnect_attempts),
                        )
                    )
                disconnect_errors = wait_for_request_errors(
                    urls["gateway"],
                    key,
                    "downstream_disconnected",
                    disconnect_attempts,
                    timeout=20,
                )
            disconnect = {
                "attempts": disconnect_attempts,
                "client_bytes_before_close": sum(disconnected),
                "recorded_downstream_disconnected_errors": disconnect_errors,
                "gateway_peak_rss_mib": round(
                    disconnect_sampler.max_current_rss("gateway"), 3
                ),
                "gateway_lifetime_high_water": disconnect_sampler.lifetime_high_water_evidence(
                    "gateway"
                ),
                "sample_count": len(disconnect_sampler.samples),
            }

            with Sampler({"gateway": processes["gateway"].pid}) as response_limit_sampler:
                oversize_status, oversize_received, oversize_error = oversize_chat(
                    urls["gateway"], key, RESPONSE_LIMIT_BYTES + MIB
                )
                response_limit_errors = wait_for_request_errors(
                    urls["gateway"],
                    key,
                    "upstream_response_too_large",
                    1,
                    timeout=20,
                )
            response_limit = {
                "upstream_bytes": RESPONSE_LIMIT_BYTES + MIB,
                "configured_limit_bytes": RESPONSE_LIMIT_BYTES,
                "http_status_before_stream_abort": oversize_status,
                "client_bytes_received": oversize_received,
                "client_error": oversize_error,
                "recorded_upstream_response_too_large_errors": response_limit_errors,
                "gateway_peak_rss_mib": round(
                    response_limit_sampler.max_current_rss("gateway"), 3
                ),
                "gateway_lifetime_high_water": response_limit_sampler.lifetime_high_water_evidence(
                    "gateway"
                ),
                "sample_count": len(response_limit_sampler.samples),
            }

            asset_phase_start = post_decay_phase_start_memory(
                processes["gateway"].pid
            )
            asset = run_asset(
                urls["gateway"],
                key,
                values["asset_mib"],
                {
                    "gateway": processes["gateway"].pid,
                    "worker": processes["worker"].pid,
                },
                archive_root,
            )
            asset["gateway_phase_start"] = asset_phase_start
            asset.update(
                asset_gateway_rss_evidence(
                    asset["gateway_peak_rss_mib"],
                    asset_phase_start["rss_mib_median"],
                    idle_gateway,
                )
            )
            # Preserve the legacy field as cumulative, informational evidence;
            # the asset gate below uses only post-decay phase-specific growth.
            asset["gateway_delta_rss_mib"] = asset[
                "gateway_cumulative_delta_from_original_idle_mib"
            ]
            asset["worker_delta_rss_mib"] = round(
                asset["worker_peak_rss_mib"] - idle_worker, 3
            )

            pre_soak_gateway = process_memory(processes["gateway"].pid)["rss_mib"]
            soak = run_soak(
                urls["gateway"],
                key,
                values["soak_seconds"],
                min(4, values["concurrency"]),
                args.target_rps,
                processes["gateway"].pid,
            )
            time.sleep(5)
            cooldown = memory_summary(processes["gateway"].pid, 1.0)
            soak["pre_soak_gateway_rss_mib"] = round(pre_soak_gateway, 3)
            soak["cooldown_gateway_rss_mib"] = cooldown["rss_mib_median"]
            soak["retained_delta_from_idle_mib"] = round(
                cooldown["rss_mib_median"] - idle_gateway, 3
            )

            checks = [
                check(
                    "100k-row control resources remain page bounded",
                    control_scale["maximum_page_rows"],
                    "<=",
                    100,
                    control_scale["maximum_page_rows"] <= 100,
                ),
                check(
                    "bounded control pages remain below 1 MiB",
                    control_scale["maximum_response_bytes"],
                    "<=",
                    MIB,
                    control_scale["maximum_response_bytes"] <= MIB,
                ),
                check(
                    "concurrent control list RSS delta",
                    control_scale["control_delta_rss_mib"],
                    "<=",
                    args.control_list_delta_max_mib,
                    control_scale["control_delta_rss_mib"]
                    <= args.control_list_delta_max_mib,
                ),
                check(
                    "gateway idle RSS",
                    idle_gateway,
                    "<=",
                    args.idle_max_mib,
                    idle_gateway <= args.idle_max_mib,
                ),
                check(
                    "concurrent streams reached EOF",
                    stream["bytes_received"],
                    "==",
                    values["concurrency"] * stream_bytes,
                    stream["bytes_received"] == values["concurrency"] * stream_bytes,
                ),
                check(
                    "mock observed configured stream concurrency",
                    stream["observed_peak_concurrency"],
                    ">=",
                    values["concurrency"],
                    stream["observed_peak_concurrency"] >= values["concurrency"],
                ),
                check(
                    "concurrent stream memory samples",
                    stream["sample_count"],
                    ">=",
                    2,
                    stream["sample_count"] >= 2,
                ),
                check(
                    "concurrent stream RSS delta",
                    stream["gateway_delta_rss_mib"],
                    "<=",
                    args.stream_delta_max_mib,
                    stream["gateway_delta_rss_mib"] <= args.stream_delta_max_mib,
                ),
                check(
                    "two bounded synchronous image responses completed",
                    synchronous_image["responses"],
                    "==",
                    2,
                    synchronous_image["responses"] == 2,
                ),
                check(
                    "synchronous image final response cap",
                    synchronous_image["maximum_response_bytes"],
                    "<=",
                    16 * MIB,
                    synchronous_image["maximum_response_bytes"] <= 16 * MIB,
                ),
                check(
                    "synchronous image RSS delta",
                    synchronous_image["gateway_delta_rss_mib"],
                    "<=",
                    args.image_delta_max_mib,
                    synchronous_image["gateway_delta_rss_mib"]
                    <= args.image_delta_max_mib,
                ),
                check(
                    "disconnects recorded as downstream disconnects",
                    disconnect_errors,
                    ">=",
                    disconnect_attempts,
                    disconnect_errors >= disconnect_attempts,
                ),
                check(
                    "64 MiB response cap stopped and classified the upstream body",
                    oversize_received,
                    "between",
                    [RESPONSE_LIMIT_BYTES - MIB, RESPONSE_LIMIT_BYTES],
                    oversize_status == 200
                    and RESPONSE_LIMIT_BYTES - MIB <= oversize_received <= RESPONSE_LIMIT_BYTES
                    and response_limit_errors >= 1,
                ),
                check(
                    "large asset job succeeded",
                    asset["status"],
                    "==",
                    "succeeded",
                    asset["status"] == "succeeded",
                ),
                check(
                    "large asset fully archived",
                    len(asset["exact_size_asset_objects"]),
                    ">=",
                    1,
                    len(asset["exact_size_asset_objects"]) >= 1,
                ),
                check(
                    "large asset memory samples",
                    asset["sample_count"],
                    ">=",
                    2,
                    asset["sample_count"] >= 2,
                ),
                check(
                    "large asset phase-start RSS samples after bounded decay cooldown",
                    asset["gateway_phase_start"]["sample_count"],
                    ">=",
                    8,
                    asset["gateway_phase_start"]["sample_count"] >= 8,
                ),
                check(
                    "large archived asset streamed through the gateway",
                    asset["download"]["full"]["bytes_received"],
                    "==",
                    asset["expected_asset_bytes"],
                    asset["download"]["full"]["bytes_received"]
                    == asset["expected_asset_bytes"],
                ),
                check(
                    "archived asset gateway download memory samples",
                    asset["download"]["sample_count"],
                    ">=",
                    2,
                    asset["download"]["sample_count"] >= 2,
                ),
                check(
                    "archived asset bounded range verified",
                    asset["download"]["range"]["status"],
                    "==",
                    206,
                    asset["download"]["range"]["status"] == 206
                    and asset["download"]["range"]["bytes_received"]
                    == ASSET_RANGE_BYTES,
                ),
                check(
                    "large asset gateway phase RSS delta",
                    asset["gateway_phase_delta_rss_mib"],
                    "<=",
                    args.asset_gateway_delta_max_mib,
                    asset["gateway_phase_delta_rss_mib"]
                    <= args.asset_gateway_delta_max_mib,
                ),
                check(
                    "large asset worker RSS delta",
                    asset["worker_delta_rss_mib"],
                    "<=",
                    args.asset_worker_delta_max_mib,
                    asset["worker_delta_rss_mib"] <= args.asset_worker_delta_max_mib,
                ),
                check(
                    "soak request failures",
                    soak["failures"],
                    "==",
                    0,
                    soak["failures"] == 0,
                ),
                check(
                    "soak memory samples",
                    soak["sample_count"],
                    ">=",
                    2,
                    soak["sample_count"] >= 2,
                ),
                check(
                    "post-soak retained RSS delta",
                    soak["retained_delta_from_idle_mib"],
                    "<=",
                    args.retained_delta_max_mib,
                    soak["retained_delta_from_idle_mib"] <= args.retained_delta_max_mib,
                ),
            ]
            # Linear slope is too noisy to be a meaningful release gate on a
            # short run.  It becomes mandatory at the requested 15 minute soak.
            if values["soak_seconds"] >= 900:
                checks.append(
                    check(
                        "15 minute soak RSS slope",
                        soak["gateway_rss_slope_mib_per_minute"],
                        "<=",
                        args.soak_slope_max_mib_per_minute,
                        soak["gateway_rss_slope_mib_per_minute"]
                        <= args.soak_slope_max_mib_per_minute,
                    )
                )

            observed_gateway_peak = process_memory(processes["gateway"].pid)[
                "high_water_mib"
            ]
            comparison = {
                "reference": "user-observed CPA process",
                "reference_rss_mib": args.comparison_rss_mib,
                "gateway_peak_rss_mib": observed_gateway_peak,
                "measurement": "process lifetime VmHWM",
                "gateway_to_reference_ratio": round(
                    observed_gateway_peak / args.comparison_rss_mib, 4
                ),
            }
            checks.append(
                check(
                    "gateway peak versus observed CPA RSS",
                    comparison["gateway_to_reference_ratio"],
                    "<=",
                    args.comparison_max_ratio,
                    comparison["gateway_to_reference_ratio"] <= args.comparison_max_ratio,
                )
            )
            roles_alive = {
                role: process.poll() is None for role, process in processes.items()
            }
            checks.append(
                check(
                    "control, gateway, and worker remained alive",
                    roles_alive,
                    "==",
                    {"control": True, "gateway": True, "worker": True},
                    all(roles_alive.values()),
                )
            )
            binary_sha256_after = file_sha256(binary)
            checks.append(
                check(
                    "release binary unchanged during the run",
                    binary_sha256_after,
                    "==",
                    binary_sha256,
                    binary_sha256_after == binary_sha256,
                )
            )
            checks.append(
                check(
                    "gateway process RSS budget under the 256 MiB deployment limit",
                    observed_gateway_peak,
                    "<=",
                    gateway_budget_mib,
                    observed_gateway_peak <= gateway_budget_mib,
                )
            )

            report = {
                "schema_version": 4,
                "benchmark": "memeloop-token-center-memory",
                "profile": args.profile,
                "started_at": started_at.isoformat(),
                "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                "git_revision": git_revision(repository),
                "git_dirty": git_dirty(repository),
                "binary": str(binary),
                "binary_sha256": binary_sha256,
                "binary_sha256_after": binary_sha256_after,
                "binary_mtime": binary_mtime,
                "system": {
                    "kernel": platform.release(),
                    "architecture": platform.machine(),
                    "logical_cpu_count": os.cpu_count(),
                    "host_memory_mib": total_memory_mib(),
                    "cgroup_memory_limit_mib": cgroup_memory_limit_mib(),
                    "python": platform.python_version(),
                },
                "runtime_topology": "control/gateway/worker split; SQLite; filesystem archive",
                "configuration": values | {"target_rps": args.target_rps},
                "resource_budget": {
                    "gateway_limit_mib": args.gateway_limit_mib,
                    "required_gateway_headroom_mib": args.gateway_headroom_mib,
                    "maximum_observed_gateway_rss_mib": gateway_budget_mib,
                    "measured_quantity": "gateway process RSS",
                    "observed_gateway_lifetime_high_water_rss_mib": round(
                        observed_gateway_peak, 3
                    ),
                    "reserved_for": "container and cgroup-charged overhead outside process RSS",
                },
                "idle": idle,
                "control_scale": control_scale,
                "stream": stream,
                "synchronous_image": synchronous_image,
                "disconnect": disconnect,
                "response_limit": response_limit,
                "asset": asset,
                "soak": soak,
                "comparison": comparison,
                "checks": checks,
                "passed": all(item["passed"] for item in checks),
            }
            report["exit_code"] = 0 if report["passed"] else 2
            write_json_report(output, report)
            return int(report["exit_code"]), report
        finally:
            for process in processes.values():
                stop_process(process)
            if mock_server is not None:
                mock_server.shutdown()
                mock_server.server_close()


def main() -> int:
    args = parse_args()
    try:
        code, report = execute(args)
        print(json.dumps({"passed": report["passed"], "checks": report["checks"]}, indent=2))
        return code
    except PrerequisiteFailure as error:
        report = failure_report(args, 3, "prerequisite", error)
        write_failure_report(args, report)
        print(json.dumps(report), file=sys.stderr)
        return 3
    except (HarnessFailure, OSError, subprocess.SubprocessError) as error:
        report = failure_report(args, 4, "functional", error)
        write_failure_report(args, report)
        print(json.dumps(report), file=sys.stderr)
        return 4
    except Exception as error:  # Keep an artifact for harness defects as well.
        report = failure_report(args, 4, "harness", error)
        write_failure_report(args, report)
        print(json.dumps(report), file=sys.stderr)
        return 4


if __name__ == "__main__":
    raise SystemExit(main())
