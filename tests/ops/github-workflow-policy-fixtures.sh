#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
verifier="$repository/web/scripts/verify-github-workflow-policy.mjs"
source_workflow="$repository/.github/workflows/ci.yml"
fixture=$(mktemp -d "${TMPDIR:-/tmp}/mtc-github-policy-fixture.XXXXXX")
cleanup() { rm -rf -- "$fixture"; }
trap cleanup EXIT HUP INT TERM

workflow="$fixture/.github/workflows/ci.yml"
mkdir -p "$fixture/.github/workflows"
git -C "$fixture" init --quiet

write_good() {
  cp "$source_workflow" "$workflow"
  printf '%s\n' 'clean fixture' >"$fixture/README.md"
  git -C "$fixture" add --all
}

verify() {
  node "$verifier" "$workflow" "$fixture"
}

expect_rejected() {
  label=$1
  if verify >"$fixture/$label.out" 2>"$fixture/$label.err"; then
    echo "malicious GitHub workflow fixture was accepted: $label" >&2
    exit 1
  fi
}

mutate() {
  mode=$1
  node --input-type=module - "$workflow" "$mode" <<'JS'
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(path.join(process.cwd(), 'web/package.json'));
const { parse, stringify } = require('yaml');
const workflowPath = process.argv[2];
const mode = process.argv[3];
const payload = parse(fs.readFileSync(workflowPath, 'utf8'));
const publish = payload.jobs['publish-ghcr'];
const dependency = payload.jobs['dependency-security'];
const buildStep = publish.steps.find((step) => String(step.uses ?? '').startsWith('docker/build-push-action@'));

switch (mode) {
  case 'top-permissions':
    payload.permissions.actions = 'write';
    break;
  case 'publish-permissions':
    publish.permissions.packages = 'read';
    break;
  case 'other-packages-write':
    payload.jobs.rust.permissions = { contents: 'read', packages: 'write' };
    break;
  case 'other-write-all':
    payload.jobs.rust.permissions = 'write-all';
    break;
  case 'other-actions-write':
    payload.jobs.rust.permissions = { actions: 'write' };
    break;
  case 'other-extra-scope':
    payload.jobs.rust.permissions = { contents: 'read', checks: 'read' };
    break;
  case 'rustsec-folded-ignore': {
    const audit = dependency.steps.find((step) => String(step.run ?? '').includes('cargo audit'));
    audit.run = 'cargo audit --deny warnings\n  --ignore RUSTSEC-2099-0001';
    break;
  }
  case 'exporter-downgrade':
    buildStep.with.outputs = 'type=image,push=true';
    break;
  case 'push-shorthand-conflict':
    buildStep.with.push = true;
    break;
  default:
    throw new Error(`unknown mutation: ${mode}`);
}

fs.writeFileSync(workflowPath, stringify(payload));
JS
}

write_good
verify >/dev/null

write_good
mutate top-permissions
expect_rejected top-permissions-expansion

write_good
mutate publish-permissions
expect_rejected publish-permissions-downgrade

write_good
mutate other-packages-write
expect_rejected unrelated-packages-write

write_good
mutate other-write-all
expect_rejected unrelated-write-all

write_good
mutate other-actions-write
expect_rejected unrelated-actions-write

write_good
mutate other-extra-scope
expect_rejected unrelated-extra-scope

# YAML parsing resolves the complete block scalar before the RustSec policy
# checks it, so line folding cannot hide an advisory suppression flag.
write_good
mutate rustsec-folded-ignore
expect_rejected rustsec-folded-ignore

write_good
mutate exporter-downgrade
expect_rejected exporter-downgrade

write_good
mutate push-shorthand-conflict
expect_rejected push-shorthand-conflict

# Scan only the retired Token Center identity across all tracked files. This
# intentionally does not match external linonetwo/cpa-* integration sources.
write_good
retired_owner=linonetwo
retired_repository=memeloop-token-center
printf 'https://github.com/%s/%s\n' "$retired_owner" "$retired_repository" \
  >"$fixture/retired-owner.txt"
git -C "$fixture" add retired-owner.txt
expect_rejected retired-self-owner

echo 'GitHub workflow policy malicious fixtures OK'
