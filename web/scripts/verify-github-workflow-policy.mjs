#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { spawnSync } from 'node:child_process';
import { parseDocument } from 'yaml';

function fail(message) {
  throw new Error(`GitHub workflow policy: ${message}`);
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function sortedEntries(value) {
  return Object.entries(value).sort(([left], [right]) => left.localeCompare(right));
}

function assertExactPermissions(actual, expected, label) {
  if (!isRecord(actual)) {
    fail(`${label} permissions must be a mapping`);
  }
  if (JSON.stringify(sortedEntries(actual)) !== JSON.stringify(sortedEntries(expected))) {
    fail(`${label} permissions must equal ${JSON.stringify(expected)}`);
  }
}

function assertNoLegacyOwner(repository) {
  const retiredSelfOwner = ['linonetwo', 'memeloop-token-center'].join('/');
  const result = spawnSync(
    'git',
    ['-C', repository, 'grep', '-n', '-I', '-F', retiredSelfOwner, '--', '.'],
    { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 },
  );
  if (result.status === 0) {
    fail(`tracked snapshot contains the retired Token Center owner:\n${result.stdout.trim()}`);
  }
  if (result.status !== 1) {
    fail(`unable to scan the tracked repository snapshot: ${result.stderr.trim()}`);
  }
}

function findStep(job, predicate, label) {
  if (!Array.isArray(job.steps)) {
    fail(`${label} must define steps`);
  }
  const matches = job.steps.filter(predicate);
  if (matches.length !== 1) {
    fail(`${label} must contain exactly one matching step`);
  }
  return matches[0];
}

function verifyDependencySecurity(job) {
  if (!isRecord(job)) {
    fail('dependency-security job is missing');
  }
  const serializedJob = JSON.stringify(job);
  if (/--ignore(?:\s|=|$)/iu.test(serializedJob)) {
    fail('dependency-security must not suppress RustSec advisories with --ignore');
  }
  if (serializedJob.includes('rustsec/audit-check@')) {
    fail('dependency-security must use the lockfile-pinned cargo-audit installation');
  }
  findStep(
    job,
    (step) => typeof step.run === 'string'
      && step.run.trim() === 'cargo install cargo-audit --version 0.22.2 --locked',
    'dependency-security cargo-audit installer',
  );
  findStep(
    job,
    (step) => typeof step.run === 'string'
      && step.run.trim() === 'cargo audit --deny warnings',
    'dependency-security RustSec enforcement',
  );
}

function verifyPublisher(job) {
  if (!isRecord(job)) {
    fail('publish-ghcr job is missing');
  }
  assertExactPermissions(job.permissions, { contents: 'read', packages: 'write' }, 'publish-ghcr');

  const expectedImages = [
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center',
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-importer',
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-plugin-installer',
  ].sort();
  const include = job.strategy?.matrix?.include;
  if (!Array.isArray(include) || include.length !== expectedImages.length) {
    fail('publish-ghcr matrix must contain exactly three images');
  }
  const actualImages = include.map((entry) => entry?.image).sort();
  if (JSON.stringify(actualImages) !== JSON.stringify(expectedImages)) {
    fail('publish-ghcr matrix must use the dynamic transferred GHCR owner');
  }

  const buildStep = findStep(
    job,
    (step) => typeof step.uses === 'string' && step.uses.startsWith('docker/build-push-action@'),
    'publish-ghcr BuildKit publisher',
  );
  if (!isRecord(buildStep.with)) {
    fail('BuildKit publisher inputs must be a mapping');
  }
  if (Object.prototype.hasOwnProperty.call(buildStep.with, 'push')) {
    fail('BuildKit publisher must not combine the push shorthand with an explicit exporter');
  }
  if (buildStep.with.outputs !== 'type=image,push=true,oci-mediatypes=true,oci-artifact=true') {
    fail('BuildKit publisher must use the reviewed native OCI attestation exporter');
  }
  if (buildStep.with.platforms !== 'linux/amd64') {
    fail('BuildKit publisher must remain a single linux/amd64 release');
  }
  if (buildStep.with.provenance !== 'mode=max' || buildStep.with.sbom !== true) {
    fail('BuildKit publisher must generate max provenance and an SBOM');
  }
}

function verifyWorkflow(workflow) {
  if (!isRecord(workflow)) {
    fail('workflow root must be a mapping');
  }
  assertExactPermissions(workflow.permissions, { contents: 'read' }, 'top-level');
  if (!isRecord(workflow.jobs)) {
    fail('jobs must be a mapping');
  }

  verifyDependencySecurity(workflow.jobs['dependency-security']);
  verifyPublisher(workflow.jobs['publish-ghcr']);

  const verifier = workflow.jobs['verify-ghcr-release'];
  if (!isRecord(verifier)) {
    fail('verify-ghcr-release job is missing');
  }
  assertExactPermissions(
    verifier.permissions,
    { contents: 'read', packages: 'read' },
    'verify-ghcr-release',
  );

  for (const [name, job] of Object.entries(workflow.jobs)) {
    if (name === 'publish-ghcr') continue;
    if (isRecord(job?.permissions) && job.permissions.packages === 'write') {
      fail(`${name} must not receive packages: write`);
    }
  }
}

function main() {
  const workflowPath = process.argv[2];
  const repositoryPath = process.argv[3];
  if (!workflowPath || !repositoryPath) {
    fail('usage: verify-github-workflow-policy.mjs <workflow> <repository>');
  }
  const document = parseDocument(fs.readFileSync(workflowPath, 'utf8'), {
    uniqueKeys: true,
  });
  if (document.errors.length > 0) {
    fail(`workflow YAML is invalid: ${document.errors.join('; ')}`);
  }
  const workflow = document.toJS({ maxAliasCount: 0 });
  verifyWorkflow(workflow);
  assertNoLegacyOwner(path.resolve(repositoryPath));
  process.stdout.write('Structured GitHub workflow policy OK\n');
}

try {
  main();
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
}
