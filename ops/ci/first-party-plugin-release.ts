import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { firstPartyPackage } from './first-party-plugin-package.ts';

const root = process.argv[2];
assert(root, 'evidence directory is required');
const digest = (bytes: Buffer) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
const json = (path: string) => JSON.parse(readFileSync(join(root, path), 'utf8'));
const source = process.env.PLUGIN_SOURCE;
const selectedPackage = firstPartyPackage(process.env.REQUESTED_PLUGIN_PACKAGE);
assert.equal(source, selectedPackage.source);
const expectedDigest = process.env.PLUGIN_DIGEST;
const manifestBytes = readFileSync(join(root, 'plugin-oci-manifest.json'));
// oras manifest fetch writes the exact registry body; digest must bind it.
assert.equal(digest(manifestBytes), expectedDigest);
const manifest = JSON.parse(manifestBytes.toString());
assert.equal(manifest.artifactType, 'application/vnd.memeloop.token-center.plugin.v1');
assert.equal(manifest.config.mediaType, 'application/vnd.memeloop.token-center.plugin.config.v1+json');
assert.equal(manifest.layers.length, 2);
const files = ['plugin.json', 'plugin.wasm'].map((name) => {
  const bytes = readFileSync(join(root, 'plugin-package', name));
  const layer = manifest.layers.find((item: any) => item.annotations?.['org.opencontainers.image.title'] === name);
  assert(layer, `missing ${name} descriptor`);
  assert.equal(layer.digest, digest(bytes));
  assert.equal(layer.size, bytes.length);
  return { name, digest: digest(bytes), size: bytes.length };
});
const installed = json('plugin-installation.json');
assert.equal(installed.id, selectedPackage.id);
assert.equal(installed.digest, expectedDigest);
assert.equal(installed.source, source);
const packageManifest = json('plugin-package/plugin.json');
assert.equal(packageManifest.id, selectedPackage.id);
assert.equal(installed.version, packageManifest.version);
const receipt = json(`plugin-install/${selectedPackage.id}/.mtc-oci-install.json`);
assert.equal(receipt.signature_policy, 'cosign-keyless');
assert.equal(receipt.digest, expectedDigest);
assert.equal(receipt.source, source);
const verification = json('plugin-signature-verification.json');
assert(Array.isArray(verification) && verification.length > 0);
assert(verification.some((entry) => entry.critical?.image?.['docker-manifest-digest'] === expectedDigest));
writeFileSync(join(root, 'plugin-release.json'), `${JSON.stringify({
  format_version: 1, plugin_id: installed.id, version: installed.version,
  reference: `${source}@${expectedDigest}`, git_sha: process.env.GITHUB_SHA,
  workflow_run: `https://github.com/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`,
  signature: { policy: 'cosign-keyless', issuer: process.env.SIGNING_ISSUER, identity: process.env.SIGNING_IDENTITY },
  installer_reference: `${process.env.INSTALLER_SOURCE}@${process.env.INSTALLER_DIGEST}`,
  installer_source_revision: process.env.INSTALLER_SOURCE_REVISION,
  files, installation_verified: true, registry_access: 'workflow-token; anonymous access not established',
}, null, 2)}\n`);
