#!/usr/bin/env python3
"""Contract tests for the standalone memory acceptance harness."""

from __future__ import annotations

import importlib.util
import pathlib
import unittest
from unittest import mock


HARNESS_PATH = pathlib.Path(__file__).with_name("benchmark_memory.py")
SPEC = importlib.util.spec_from_file_location("benchmark_memory", HARNESS_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import memory harness from {HARNESS_PATH}")
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class BenchmarkSeedContractTests(unittest.TestCase):
    def test_every_benchmark_route_explicitly_confirms_its_custom_model(self) -> None:
        requests: list[tuple[str, dict[str, object]]] = []

        def api_json(
            _base_url: str,
            _method: str,
            path: str,
            _token: str,
            payload: dict[str, object],
        ) -> dict[str, str]:
            requests.append((path, payload))
            if path == "/internal/v1/upstreams":
                return {"id": f"upstream-{len(requests)}"}
            if path == "/internal/v1/keys":
                return {"key": "mts_test"}
            return {}

        with (
            mock.patch.object(HARNESS, "api_json", side_effect=api_json),
            mock.patch.object(HARNESS, "small_chat"),
        ):
            issued_key = HARNESS.seed(
                "http://control.invalid",
                "http://gateway.invalid",
                "service-token",
                "http://mock.invalid",
            )

        routes = [
            payload
            for path, payload in requests
            if path == "/internal/v1/model-routes"
        ]

        self.assertEqual(issued_key, "mts_test")
        self.assertEqual(len(routes), 3)
        self.assertEqual({route["protocol"] for route in routes}, {"openai", "generation"})
        for route in routes:
            with self.subTest(model=route["public_model"]):
                self.assertIs(route["custom_model_confirmed"], True)
                self.assertEqual(route["priority"], 0)

    def test_stream_fixture_exercises_the_streaming_proxy_path(self) -> None:
        handler = type(
            "BoundMockHandler",
            (HARNESS.MockHandler,),
            {"state": HARNESS.MockState()},
        )
        server = HARNESS.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = HARNESS.threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        connection = HARNESS.http.client.HTTPConnection(
            "127.0.0.1", server.server_port, timeout=5
        )
        try:
            payload = HARNESS.json.dumps(
                HARNESS.chat_payload("stream", 4096), separators=(",", ":")
            ).encode()
            connection.request(
                "POST",
                "/v1/chat/completions",
                body=payload,
                headers={"content-type": "application/json"},
            )
            response = connection.getresponse()

            self.assertEqual(response.status, 200)
            self.assertEqual(response.headers.get_content_type(), "text/event-stream")
            self.assertEqual(len(response.read()), 4096)
        finally:
            connection.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    unittest.main(verbosity=2)
