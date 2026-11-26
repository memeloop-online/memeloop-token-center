#!/usr/bin/env python3
"""Package/verify a GHA-built test executable and its complete runtime WIT tree.

This tool never compiles or provisions databases. Running the Stage contract is
explicit, once only, after artifact verification by the sole Stage operator.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

SELECTOR = "plugin::application::oauth_stage_fixture::stage_existing_database_cold_oauth_authority"
PIN_NAMES = (
    "DATABASE", "DATABASE_ROLE", "CLUSTER_UID", "CLUSTER_IDENTITY_PATH",
    "NAMESPACE", "NAMESPACE_UID", "PG_HOST", "PG_PORT", "PG_SERVICE_UID",
    "WORK_ROOT", "SOURCE",
)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def package(args):
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    pins = {name: os.environ.get("MTC_STAGE_FIXTURE_" + name, "") for name in PIN_NAMES}
    if any(not value for value in pins.values()) or pins["SOURCE"] != source:
        raise RuntimeError("all compile-time pins must be present and source must match checkout")
    executables = set()
    for line in Path(args.messages).read_text().splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (item.get("reason") == "compiler-artifact"
                and item.get("profile", {}).get("test")
                and item.get("target", {}).get("name") == "memeloop_token_center"
                and "lib" in item.get("target", {}).get("kind", [])
                and item.get("executable")):
            executables.add(item["executable"])
    if len(executables) != 1:
        raise RuntimeError("cargo output must identify exactly one libtest executable")
    output = Path(args.bundle)
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    shutil.copy2(executables.pop(), output / "oauth-fixture")
    shutil.copytree("wit", output / "wit", symlinks=False)
    shutil.copy2(__file__, output / "stage_oauth_fixture_bundle.py")
    files = {}
    for path in sorted(output.rglob("*")):
        if path.is_symlink():
            raise RuntimeError("bundle symlinks forbidden")
        if path.is_file():
            files[path.relative_to(output).as_posix()] = digest(path)
    manifest = {"source": source, "selector": SELECTOR, "target_pins": pins, "sha256": files}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    verify(output)
    print("bundle verified: source=" + source + " files=" + str(len(files)) + " selected_tests=1")


def verify(bundle):
    bundle = Path(bundle).resolve(strict=True)
    manifest = json.loads((bundle / "manifest.json").read_text())
    if manifest.get("selector") != SELECTOR or not re.fullmatch(r"[0-9a-f]{40}", manifest.get("source", "")):
        raise RuntimeError("unexpected fixture identity")
    files = manifest["sha256"]
    if "oauth-fixture" not in files or "wit/token-center.wit" not in files:
        raise RuntimeError("missing libtest or WIT source")
    actual = set()
    for path in bundle.rglob("*"):
        if path.is_symlink():
            raise RuntimeError("bundle symlinks forbidden")
        if path.is_file() and path != bundle / "manifest.json":
            actual.add(path.relative_to(bundle).as_posix())
    if actual != set(files):
        raise RuntimeError("bundle file set differs from fixed manifest")
    for name, expected in files.items():
        path = bundle / name
        if path.resolve().parent != bundle and bundle not in path.resolve().parents:
            raise RuntimeError("bundle path escape")
        if digest(path) != expected:
            raise RuntimeError("bundle file digest mismatch")
    result = subprocess.run([str(bundle / "oauth-fixture"), "--exact", SELECTOR,
                             "--ignored", "--list"], cwd=bundle,
                            capture_output=True, text=True, check=True)
    selected = [line for line in result.stdout.splitlines() if line.endswith(": test")]
    if selected != [SELECTOR + ": test"]:
        raise RuntimeError("exact selector did not enumerate exactly one test")
    return bundle, manifest


def run(args):
    bundle, manifest = verify(args.bundle)
    if os.environ.get("MTC_STAGE_FIXTURE_ARMED") != "existing-database-once":
        raise RuntimeError("Stage execution not explicitly armed")
    if not os.environ.get("DATABASE_URL") or not os.environ.get("KEY_PEPPER"):
        raise RuntimeError("runtime SecretRef inputs missing")
    # No retry, second attempt, or database preparation. Rust performs its own
    # pinned identity and durable-attempt guard before any fixture write.
    receipt_dir = Path(os.environ["MTC_STAGE_FIXTURE_RECEIPT_DIR"])
    expected_parent = Path(manifest["target_pins"]["WORK_ROOT"]).resolve(strict=True)
    if not receipt_dir.is_absolute() or receipt_dir.resolve().parent != expected_parent:
        raise RuntimeError("receipt directory must be an owned child of the pinned work root")
    receipt_dir.mkdir(mode=0o700, parents=False, exist_ok=False)
    result = subprocess.run([str(bundle / "oauth-fixture"), "--exact", SELECTOR,
                             "--ignored", "--nocapture", "--test-threads=1"],
                            cwd=bundle, capture_output=True, text=True)
    output = result.stdout + result.stderr
    log = receipt_dir / "fixture.log"
    log.write_text(output)
    log.chmod(0o600)
    passed = result.returncode == 0 and re.search(
        r"test result: ok\. 1 passed; 0 failed; 0 ignored;", output) is not None
    receipt = {"source": manifest["source"], "selector": SELECTOR,
               "returncode": result.returncode, "one_actual_pass": passed,
               "log_sha256": digest(log)}
    (receipt_dir / "execution.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt))
    if not passed:
        raise RuntimeError("Stage contract incomplete; retain attempt and do not retry")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("package")
    create.add_argument("--messages", required=True)
    create.add_argument("--bundle", required=True)
    check = commands.add_parser("verify")
    check.add_argument("--bundle", required=True)
    execute = commands.add_parser("run")
    execute.add_argument("--bundle", required=True)
    args = parser.parse_args()
    if args.command == "package":
        package(args)
    elif args.command == "verify":
        _, manifest = verify(args.bundle)
        print("verified source=" + manifest["source"] + " selected_tests=1")
    else:
        run(args)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # No connection strings, credential values or arbitrary subprocess
        # output may escape via a Python traceback.
        print("fixture command rejected; inspect retained authorized receipts", file=sys.stderr)
        sys.exit(1)
