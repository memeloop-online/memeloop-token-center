import json
import os
from pathlib import Path
import resource
import sys
import time

import peer_fingerprint as helper


PEERS = ("192.0.2.1", "192.0.2.2")
STATE = {"stage": "initializing"}


def memory():
    root = Path("/sys/fs/cgroup")
    return {
        name: int((root / name).read_text())
        for name in ("memory.current", "memory.peak", "memory.max")
    }


class CountingSink:
    def __init__(self):
        self.length = 0

    def write(self, text):
        self.length += len(text.encode("utf-8"))

    def flush(self):
        pass


def main():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    assembler = helper.Assembler(PEERS)
    started = time.monotonic()
    measurements = {"baseline": memory()}
    STATE["measurements"] = measurements
    STATE["capabilities"] = {
        line.split(":", 1)[0]: line.split(":", 1)[1].strip()
        for line in Path("/proc/self/status").read_text().splitlines()
        if line.startswith(("CapEff:", "CapPrm:", "CapAmb:", "CapBnd:"))
    }
    output = CountingSink()

    def consume(stream):
        STATE["stage"] = "synthetic_assembly"
        payload = bytes(range(256)) * 16
        for index in range(helper.MAX_BLOCKS):
            assembler.segment(*PEERS, 30000, 2380, (index + 1) * helper.BLOCK_SIZE, 16, payload, 2_000_000_000)
        assembler.segment(*PEERS, 30000, 2380, 4096, 16, payload, 2_000_000_000)
        summary = assembler.summary("0" * 32, "left", 1_000_000_000, 3_000_000_000, 1_000_000_000, 3_000_000_000)
        STATE["stage"] = "serialize_summary"
        if len(summary["records"]) != helper.MAX_BLOCKS or summary["issues"]:
            raise RuntimeError("synthetic_assembly_incomplete")
        helper.safe_emit(summary, output)
        measurements["full_assembly_and_summary"] = memory()
        measurements["assembly_seconds"] = round(time.monotonic() - started, 3)
        try:
            assembler.segment(*PEERS, 30000, 2380, (helper.MAX_BLOCKS + 1) * helper.BLOCK_SIZE, 16, payload, 2_000_000_000)
        except helper.CaptureLimit as error:
            if str(error) != "assembly_budget":
                raise
        else:
            raise RuntimeError("assembly_budget_not_enforced")
        STATE["stage"] = "read_idle_capture_header"
        helper.read_pcap(stream, helper.Assembler(PEERS))

    STATE["stage"] = "start_capture_group"
    _, returncode, diagnostic = helper.supervise(helper.capture_command(PEERS, interface="lo"), consume, 45)
    STATE["stage"] = "capture_statistics"
    statistics = helper.capture_statistics(diagnostic)
    measurements["after_cleanup"] = memory()
    if returncode != 0 or statistics != {"captured": 0, "received": 0, "dropped": 0}:
        raise RuntimeError("isolated_capture_smoke_failed")
    children = Path(f"/proc/self/task/{os.getpid()}/children").read_text().strip()
    if children:
        raise RuntimeError("unreaped_capture_children")
    if measurements["assembly_seconds"] >= 45:
        raise RuntimeError("stress_did_not_overlap_live_capture")
    print(json.dumps({
        "probe": "whole-cgroup-synthetic-assembly-with-idle-tcpdump",
        "python": sys.version.split()[0],
        "blocks": assembler.block_count,
        "summary_bytes": output.length,
        "measurements": measurements,
        "tcpdump_returncode": returncode,
        "capture_statistics": statistics,
        "children_after_cleanup": 0,
        "production_capture": False,
    }), flush=True)


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        print(json.dumps({
            "probe_failure": "bounded_probe_failed",
            "error_type": type(error).__name__,
            "capture_returncode": getattr(error, "capture_returncode", None),
            "capture_startup_rejected": getattr(error, "capture_startup_rejected", None),
            "state": STATE,
            "production_capture": False,
        }), flush=True)
        sys.exit(1)
