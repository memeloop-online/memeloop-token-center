import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.argv[2];
assert(root, 'endorsement evidence directory is required');
const pluginId = process.env.PLUGIN_ID;
const pluginDigest = process.env.PLUGIN_DIGEST;
const pluginSource = process.env.PLUGIN_SOURCE;
const upstreamIdentity = process.env.UPSTREAM_SIGNING_IDENTITY;
const signingIdentity = process.env.SIGNING_IDENTITY;
const json = (path: string) => JSON.parse(readFileSync(join(root, path), 'utf8'));
const digest = (bytes: Buffer) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`;

assert.equal(pluginId, 'mtc-health-intelligence');
assert.equal(pluginSource, 'ghcr.io/memeloop-online/memeloop-token-center-health-intelligence-plugin');
assert.equal(pluginDigest, 'sha256:d9d558a6a6118dadfd2693cfd7f479109dbca53ef8ddae201d8ea70fb93922c3');
const manifestBytes = readFileSync(join(root, 'plugin-oci-manifest.json'));
assert.equal(digest(manifestBytes), pluginDigest);
const oci = JSON.parse(manifestBytes.toString());
assert.equal(oci.artifactType, 'application/vnd.memeloop.token-center.plugin.v1');
assert.deepEqual(oci.layers.map((layer: any) => layer.annotations?.['org.opencontainers.image.title']).sort(),
  ['README.md', 'plugin.json', 'schemas-health-intelligence.json']);
const installed = json('plugin-installation.json');
assert.equal(installed.id, pluginId);
assert.equal(installed.digest, pluginDigest);
assert.equal(installed.source, pluginSource);
const plugin = json(`plugin-install/${pluginId}/plugin.json`);
assert.equal(plugin.id, pluginId);
assert.equal(plugin.wasm, null);
assert.equal(plugin.contributions.operator_ui[0].presentation, 'health_intelligence_v1');
assert.equal(plugin.contributions.service_data[0].required_scope, 'metrics:read');
const receipt = json(`plugin-install/${pluginId}/.mtc-oci-install.json`);
assert.equal(receipt.digest, pluginDigest);
assert.equal(receipt.source, pluginSource);
assert.equal(receipt.signature_policy, 'cosign-keyless');
for (const path of ['upstream-signature-verification.json', 'mtc-endorsement-verification.json']) {
  const signatures = json(path);
  assert(signatures.some((entry: any) => entry.critical?.image?.['docker-manifest-digest'] === pluginDigest));
}
writeFileSync(join(root, 'plugin-endorsement.json'), `${JSON.stringify({
  format_version: 1,
  reference: `${pluginSource}@${pluginDigest}`,
  upstream_identity: upstreamIdentity,
  endorsed_identity: signingIdentity,
  installer_reference: `${process.env.INSTALLER_SOURCE}@${process.env.INSTALLER_DIGEST}`,
  installation_verified: true,
}, null, 2)}\n`);
