#!/usr/bin/env python3
"""Reproducible gateway/worker RSS and streaming acceptance harness.

Only Python's standard library is used.  The harness starts isolated control,
gateway, and worker processes over one temporary SQLite database, plus a local
streaming mock upstream.  Its JSON report is intended to be kept as CI or
release evidence.
"""

from __future__ import annotations

import argparse
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
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MIB = 1024 * 1024
RESPONSE_LIMIT_BYTES = 64 * MIB


class HarnessFailure(RuntimeError):
    """A functional acceptance assertion failed."""


class PrerequisiteFailure(RuntimeError):
    """A local prerequisite or service startup failed."""


class MockState:
    def __init__(self) -> None:
        self.assets: dict[str, int] = {}
        self.lock = threading.Lock()


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

    def _stream_bytes(self, total: int, chunk_size: int, delay_ms: float) -> None:
        self.send_response(200)
        self.send_header("content-type", "application/octet-stream")
        self.send_header("content-length", str(total))
        self.end_headers()
        block = b"x" * min(chunk_size, total)
        remaining = total
        try:
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
                self._stream_bytes(total, chunk, delay_ms)
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
            self._stream_bytes(asset_bytes, 256 * 1024, 0.25)
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
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        started = time.monotonic()
        while not self.stop_event.is_set():
            sample: dict[str, Any] = {"elapsed_seconds": time.monotonic() - started}
            try:
                for name, pid in self.pids.items():
                    sample[name] = process_memory(pid)
                self.samples.append(sample)
            except HarnessFailure:
                return
            self.stop_event.wait(self.interval)

    def __enter__(self) -> "Sampler":
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)

    def max_rss(self, name: str) -> float:
        return max((sample[name]["rss_mib"] for sample in self.samples), default=0.0)


def memory_summary(pid: int, duration: float = 1.0) -> dict[str, float]:
    samples: list[dict[str, float]] = []
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        samples.append(process_memory(pid))
        time.sleep(0.1)
    rss = [sample["rss_mib"] for sample in samples]
    pss = [sample["pss_mib"] for sample in samples if sample["pss_mib"] > 0]
    return {
        "rss_mib_median": round(statistics.median(rss), 3),
        "rss_mib_p95": round(percentile(rss, 0.95), 3),
        "rss_mib_max": round(max(rss), 3),
        "pss_mib_median": round(statistics.median(pss), 3) if pss else 0.0,
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
) -> tuple[int, bytes, dict[str, str]]:
    parsed = urllib.parse.urlsplit(base_url)
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    headers = {"authorization": f"Bearer {token}", "connection": "close"}
    if body is not None:
        headers["content-type"] = "application/json"
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


def seed(control_url: str, gateway_url: str, service_token: str, mock_url: str) -> str:
    tenant = "memory-benchmark"
    text_model = "benchmark-text"
    asset_model = "benchmark-seedance"
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
        {
            "tenant_external_id": tenant,
            "public_model": text_model,
            "upstream_account_id": text_upstream["id"],
            "upstream_model": text_model,
            "protocol": "openai",
            "priority": 0,
        },
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
        {
            "tenant_external_id": tenant,
            "public_model": asset_model,
            "upstream_account_id": asset_upstream["id"],
            "upstream_model": asset_model,
            "protocol": "generation",
            "priority": 0,
        },
    )
    api_json(
        control_url,
        "POST",
        f"/internal/v1/generation-prices/USD/{asset_model}",
        service_token,
        {"billing_unit": "second", "price_per_unit": "0.01"},
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
                "allowed_models": [text_model, asset_model],
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


def wait_for_stream_errors(gateway_url: str, key: str, minimum: int, timeout: float = 20) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        values = api_json(
            gateway_url,
            "GET",
            "/self/v1/requests?status=error&error_code=upstream_stream&limit=100",
            key,
        )
        if isinstance(values, list) and len(values) >= minimum:
            return len(values)
        time.sleep(0.2)
    return 0


def archive_bytes(root: pathlib.Path) -> int:
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def run_asset(
    gateway_url: str,
    key: str,
    asset_mib: int,
    pids: dict[str, int],
    archive_root: pathlib.Path,
) -> dict[str, Any]:
    archive_before = archive_bytes(archive_root)
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
    deadline = time.monotonic() + max(120, asset_mib * 2)
    started = time.monotonic()
    final: dict[str, Any] | None = None
    with Sampler(pids) as sampler:
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
    expected_bytes = asset_mib * MIB
    archive_after = archive_bytes(archive_root)
    return {
        "job_id": job_id,
        "status": final["status"],
        "error_code": final.get("error_code"),
        "asset_mib": asset_mib,
        "expected_asset_bytes": expected_bytes,
        "archive_bytes_before": archive_before,
        "archive_bytes_after": archive_after,
        "archive_growth_bytes": archive_after - archive_before,
        "duration_seconds": round(time.monotonic() - started, 3),
        "gateway_peak_rss_mib": round(sampler.max_rss("gateway"), 3),
        "worker_peak_rss_mib": round(sampler.max_rss("worker"), 3),
        "sample_count": len(sampler.samples),
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
        "successes": successes,
        "failures": failures,
        "achieved_rps": round(successes / elapsed, 3) if elapsed else 0.0,
        "latency_ms_p50": round(percentile(latencies_ms, 0.50), 3),
        "latency_ms_p95": round(percentile(latencies_ms, 0.95), 3),
        "gateway_peak_rss_mib": round(sampler.max_rss("gateway"), 3),
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
    parser.add_argument("--idle-max-mib", type=float, default=256.0)
    parser.add_argument("--stream-delta-max-mib", type=float, default=192.0)
    parser.add_argument("--asset-gateway-delta-max-mib", type=float, default=96.0)
    parser.add_argument("--asset-worker-delta-max-mib", type=float, default=192.0)
    parser.add_argument("--retained-delta-max-mib", type=float, default=96.0)
    parser.add_argument("--soak-slope-max-mib-per-minute", type=float, default=2.0)
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
    if args.comparison_rss_mib <= 0 or not 0 < args.comparison_max_ratio <= 1:
        raise PrerequisiteFailure(
            "comparison-rss-mib must be positive and comparison-max-ratio must be in (0, 1]"
        )
    binary = (args.binary or repository / "target/release/memeloop-token-center").resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise PrerequisiteFailure(
            f"executable not found: {binary}; run cargo build --release --bin memeloop-token-center"
        )
    if not pathlib.Path("/proc/self/status").exists():
        raise PrerequisiteFailure("RSS acceptance requires Linux /proc")

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
        environment = os.environ.copy() | {
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

            stream_bytes = values["stream_mib"] * MIB
            stream_started = time.monotonic()
            stream_pids = {"gateway": processes["gateway"].pid}
            with Sampler(stream_pids) as stream_sampler:
                with concurrent.futures.ThreadPoolExecutor(
                    max_workers=values["concurrency"]
                ) as pool:
                    stream_results = list(
                        pool.map(
                            lambda _index: stream_chat(
                                urls["gateway"], key, stream_bytes, delay_ms=0.25
                            ),
                            range(values["concurrency"]),
                        )
                    )
            stream_peak = stream_sampler.max_rss("gateway")
            stream = {
                "concurrency": values["concurrency"],
                "bytes_per_response": stream_bytes,
                "bytes_received": sum(stream_results),
                "duration_seconds": round(time.monotonic() - stream_started, 3),
                "gateway_peak_rss_mib": round(stream_peak, 3),
                "gateway_delta_rss_mib": round(stream_peak - idle_gateway, 3),
                "sample_count": len(stream_sampler.samples),
            }

            disconnect_attempts = max(2, min(4, values["concurrency"]))
            with concurrent.futures.ThreadPoolExecutor(max_workers=disconnect_attempts) as pool:
                disconnected = list(
                    pool.map(
                        lambda _index: disconnect_chat(urls["gateway"], key, 16 * MIB),
                        range(disconnect_attempts),
                    )
                )
            disconnect_errors = wait_for_stream_errors(
                urls["gateway"], key, disconnect_attempts, timeout=20
            )
            disconnect = {
                "attempts": disconnect_attempts,
                "client_bytes_before_close": sum(disconnected),
                "recorded_upstream_stream_errors": disconnect_errors,
            }

            oversize_status, oversize_received, oversize_error = oversize_chat(
                urls["gateway"], key, RESPONSE_LIMIT_BYTES + MIB
            )
            total_stream_errors = wait_for_stream_errors(
                urls["gateway"], key, disconnect_attempts + 1, timeout=20
            )
            response_limit = {
                "upstream_bytes": RESPONSE_LIMIT_BYTES + MIB,
                "configured_limit_bytes": RESPONSE_LIMIT_BYTES,
                "http_status_before_stream_abort": oversize_status,
                "client_bytes_received": oversize_received,
                "client_error": oversize_error,
                "recorded_upstream_stream_errors": total_stream_errors,
            }

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
            asset["gateway_delta_rss_mib"] = round(
                asset["gateway_peak_rss_mib"] - idle_gateway, 3
            )
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
                    "gateway idle RSS",
                    idle_gateway,
                    "<=",
                    args.idle_max_mib,
                    idle_gateway <= args.idle_max_mib,
                ),
                check(
                    "concurrent stream RSS delta",
                    stream["gateway_delta_rss_mib"],
                    "<=",
                    args.stream_delta_max_mib,
                    stream["gateway_delta_rss_mib"] <= args.stream_delta_max_mib,
                ),
                check(
                    "disconnects recorded as stream errors",
                    disconnect_errors,
                    ">=",
                    disconnect_attempts,
                    disconnect_errors >= disconnect_attempts,
                ),
                check(
                    "64 MiB response cap stopped the upstream body",
                    oversize_received,
                    "<=",
                    RESPONSE_LIMIT_BYTES,
                    oversize_received <= RESPONSE_LIMIT_BYTES
                    and total_stream_errors >= disconnect_attempts + 1,
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
                    asset["archive_growth_bytes"],
                    ">=",
                    asset["expected_asset_bytes"],
                    asset["archive_growth_bytes"] >= asset["expected_asset_bytes"],
                ),
                check(
                    "large asset gateway RSS delta",
                    asset["gateway_delta_rss_mib"],
                    "<=",
                    args.asset_gateway_delta_max_mib,
                    asset["gateway_delta_rss_mib"] <= args.asset_gateway_delta_max_mib,
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

            observed_gateway_peak = max(
                stream["gateway_peak_rss_mib"],
                asset["gateway_peak_rss_mib"],
                soak["gateway_peak_rss_mib"],
            )
            comparison = {
                "reference": "user-observed CPA process",
                "reference_rss_mib": args.comparison_rss_mib,
                "gateway_peak_rss_mib": observed_gateway_peak,
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

            report = {
                "schema_version": 1,
                "benchmark": "memeloop-token-center-memory",
                "profile": args.profile,
                "started_at": started_at.isoformat(),
                "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                "git_revision": git_revision(repository),
                "git_dirty": git_dirty(repository),
                "binary": str(binary),
                "binary_sha256": file_sha256(binary),
                "binary_mtime": dt.datetime.fromtimestamp(
                    binary.stat().st_mtime, tz=dt.timezone.utc
                ).isoformat(),
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
                "idle": idle,
                "stream": stream,
                "disconnect": disconnect,
                "response_limit": response_limit,
                "asset": asset,
                "soak": soak,
                "comparison": comparison,
                "checks": checks,
                "passed": all(item["passed"] for item in checks),
            }
            report["exit_code"] = 0 if report["passed"] else 2
            output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
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
        print(
            json.dumps(
                {"passed": False, "exit_code": 3, "error_kind": "prerequisite", "error": str(error)}
            ),
            file=sys.stderr,
        )
        return 3
    except (HarnessFailure, OSError, subprocess.SubprocessError) as error:
        print(
            json.dumps(
                {"passed": False, "exit_code": 4, "error_kind": "functional", "error": str(error)}
            ),
            file=sys.stderr,
        )
        return 4


if __name__ == "__main__":
    raise SystemExit(main())
