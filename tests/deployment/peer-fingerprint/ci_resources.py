import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid


IMAGE = "docker.io/nicolaka/netshoot@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70"
CONFIG = "sha256:2298d942ce5f8e8083c036b6d62e129379998712ef78343f88d41d782d507141"
SUPPORT = Path(__file__).resolve().parent


def docker(*arguments, timeout=30):
    return subprocess.run(["docker", *arguments], check=True, capture_output=True, text=True, timeout=timeout).stdout


def pinned_tests():
    name = "peer-fingerprint-unit-" + uuid.uuid4().hex[:16]
    try:
        docker(
            "run", "--name", name, "--network", "none", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--user", "0:0", "--read-only",
            "--ulimit", "core=0:0", "--env", "PYTHONDONTWRITEBYTECODE=1",
            "--mount", f"type=bind,source={SUPPORT},target=/support,readonly",
            "--entrypoint", "/usr/bin/python3", IMAGE,
            "-B", "-m", "unittest", "discover", "-v", "-s", "/support", "-p", "test_*.py", timeout=90,
        )
        return True
    except subprocess.SubprocessError:
        return False
    finally:
        docker("rm", "--force", name)


def measure(limit):
    name = "peer-fingerprint-ci-" + uuid.uuid4().hex[:16]
    result = {"limit_mib": limit, "cpu_limit": 0.2, "passed": False, "sampled_cgroup_peak_bytes": 0}
    try:
        docker(
            "create", "--name", name, "--network", "none", "--cap-drop", "ALL", "--cap-add", "NET_RAW",
            "--security-opt", "no-new-privileges", "--user", "65532:65532", "--read-only",
            "--memory", f"{limit}m", "--memory-swap", f"{limit}m", "--cpus", "0.2", "--pids-limit", "12",
            "--ulimit", "core=0:0", "--env", "PYTHONDONTWRITEBYTECODE=1",
            "--mount", f"type=bind,source={SUPPORT},target=/support,readonly",
            "--entrypoint", "/usr/bin/python3", IMAGE, "-B", "/support/ci_probe.py",
        )
        docker("start", name)
        deadline = time.monotonic() + 90
        while True:
            state = json.loads(docker("inspect", "--format", "{{json .State}}", name))
            if not state["Running"]:
                result["exit_code"] = state["ExitCode"]
                result["oom_killed"] = state["OOMKilled"]
                break
            if time.monotonic() >= deadline:
                result["hard_timeout"] = True
                docker("kill", name)
                break
            try:
                membership = Path(f"/proc/{state['Pid']}/cgroup").read_text()
                relative = next(line[3:] for line in membership.splitlines() if line.startswith("0::"))
                peak = int((Path("/sys/fs/cgroup") / relative.lstrip("/") / "memory.peak").read_text())
                result["sampled_cgroup_peak_bytes"] = max(result["sampled_cgroup_peak_bytes"], peak)
            except (OSError, StopIteration):
                result["host_cgroup_sampling_unavailable"] = True
            time.sleep(0.2)
        logs = docker("logs", name)
        if len(logs) <= 16384:
            try:
                result["probe"] = json.loads(logs)
            except ValueError:
                result["invalid_probe_summary"] = True
        else:
            result["probe_summary_limit"] = True
        probe = result.get("probe", {})
        measurements = probe.get("measurements", {})
        after = measurements.get("after_cleanup", {})
        result["passed"] = (
            result.get("exit_code") == 0 and result.get("oom_killed") is False
            and probe.get("children_after_cleanup") == 0 and probe.get("blocks") == 1536
            and after.get("memory.max") == limit * 1024 * 1024
            and 0 < after.get("memory.peak", 0) <= limit * 1024 * 1024
        )
    except (subprocess.SubprocessError, ValueError, OSError):
        result["harness_failure"] = True
    finally:
        try:
            docker("rm", "--force", name)
            result["container_removed"] = True
        except subprocess.SubprocessError:
            result["container_removed"] = False
            result["passed"] = False
    return result


def main():
    report = {
        "image": IMAGE,
        "expected_config": CONFIG,
        "commit": os.environ.get("HELPER_COMMIT"),
        "helper_sha256": hashlib.sha256((SUPPORT / "peer_fingerprint.py").read_bytes()).hexdigest(),
        "production_capture": False,
        "capture_uid": 0,
        "capture_identity_requires_parent_review": True,
        "measurements": [],
    }
    passed = False
    try:
        docker("pull", "--platform", "linux/amd64", IMAGE, timeout=240)
        inspected = json.loads(docker("image", "inspect", IMAGE))[0]
        report["actual_config"] = inspected["Id"]
        report["architecture"] = inspected["Architecture"]
        if inspected["Id"] != CONFIG or inspected["Architecture"] != "amd64":
            raise ValueError("image_identity_mismatch")
        report["pinned_runtime_synthetic_tests_passed"] = pinned_tests()
        primary = measure(32)
        report["measurements"].append(primary)
        passed = primary["passed"] and report["pinned_runtime_synthetic_tests_passed"]
        if not primary["passed"]:
            report["measurements"].append(measure(64))
            report["budget_change_requires_review"] = True
    except (subprocess.SubprocessError, ValueError, OSError):
        report["setup_failure"] = True
    report["32mib_gate_passed"] = passed
    output = json.dumps(report, indent=2, sort_keys=True) + "\n"
    Path(os.environ["RUNNER_TEMP"], "peer-fingerprint-resource-report.json").write_text(output)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
        summary.write("## Passive helper: isolated CI only\n\n```json\n" + output + "```\n")
    print(output, end="")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
