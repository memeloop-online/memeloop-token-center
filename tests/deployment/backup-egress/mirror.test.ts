import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'yaml';
import { sha256, sourceDigest, sourceImage, targetImage, targetTag, verifyGraph, verifyIndexBytes } from '../../../scripts/ci/backup-egress-mirror-provenance.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Backup egress checks run only in GitHub Actions');

function graph() {
  const bodies = new Map<string, Buffer>();
  const add = (kind: string, mediaType: string, value: unknown) => {
    const bytes = Buffer.from(JSON.stringify(value));
    const descriptor = { mediaType, digest: sha256(bytes), size: bytes.length };
    bodies.set(`${kind}/${descriptor.digest}`, bytes);
    return descriptor;
  };
  const config = add('blobs', 'application/vnd.oci.image.config.v1+json', { architecture: 'amd64' });
  const layer = add('blobs', 'application/vnd.oci.image.layer.v1.tar+gzip', 'fixture layer');
  const attestation = add('blobs', 'application/vnd.in-toto+json', { predicateType: 'fixture' });
  const image = add('manifests', 'application/vnd.oci.image.manifest.v1+json', { schemaVersion: 2, mediaType: 'application/vnd.oci.image.manifest.v1+json', config, layers: [layer] });
  const evidence = add('manifests', 'application/vnd.oci.image.manifest.v1+json', { schemaVersion: 2, mediaType: 'application/vnd.oci.image.manifest.v1+json', config, layers: [attestation] });
  const root = add('manifests', 'application/vnd.oci.image.index.v1+json', { schemaVersion: 2, mediaType: 'application/vnd.oci.image.index.v1+json', manifests: [image, evidence] });
  const requests: string[] = [];
  const request = async (route: string) => {
    requests.push(route);
    const bytes = bodies.get(route);
    return bytes ? new Response(new Uint8Array(bytes)) : new Response(null, { status: 404 });
  };
  return { bodies, requests, request, root, config, layer, add };
}

test('full graph GET includes image configs, layers, and index attestations without duplicate reads', async () => {
  const fixture = graph();
  const receipts = await verifyGraph(fixture.root, fixture.request);
  assert.equal(receipts.length, fixture.bodies.size);
  assert.equal(fixture.requests.length, fixture.bodies.size);
  assert.equal(receipts.reduce((total, receipt) => total + receipt.size, 0), [...fixture.bodies.values()].reduce((total, bytes) => total + bytes.length, 0));
});

test('corrupt, truncated, and oversized remote layers cannot pass', async () => {
  for (const change of ['corrupt', 'truncated', 'oversized']) {
    const fixture = graph();
    const route = `blobs/${fixture.layer.digest}`;
    const bytes = fixture.bodies.get(route)!;
    fixture.bodies.set(route, change === 'corrupt' ? Buffer.alloc(bytes.length, 120) : change === 'truncated' ? bytes.subarray(1) : Buffer.concat([bytes, Buffer.from('extra')]));
    await assert.rejects(verifyGraph(fixture.root, fixture.request), /size or digest mismatch|exceeds descriptor size/);
  }
});

test('remote absence and authentication failures cannot pass', async () => {
  for (const status of [401, 403, 404, 500]) {
    await assert.rejects(verifyGraph(graph().root, async () => new Response(null, { status })), /Registry GET failed/);
  }
});

test('bounds reject object count, aggregate bytes, manifests, and blobs', async () => {
  for (const bounds of [{ objects: 2 }, { totalBytes: 1 }, { manifestBytes: 1 }, { blobBytes: 1 }]) {
    const fixture = graph();
    await assert.rejects(verifyGraph(fixture.root, fixture.request, bounds), /bound exceeded/);
  }
});

test('external descriptors and invalid digest paths are rejected before any request', async () => {
  for (const change of [{ digest: '../secret' }, { urls: ['https://example.invalid/blob'] }, { size: -1 }]) {
    const fixture = graph();
    await assert.rejects(verifyGraph({ ...fixture.root, ...change }, fixture.request), /Invalid or external descriptor/);
    assert.equal(fixture.requests.length, 0);
  }
});

test('conflicting descriptor metadata is rejected even for a previously verified blob', async () => {
  const fixture = graph();
  const root = fixture.add('manifests', 'application/vnd.oci.image.manifest.v1+json', {
    schemaVersion: 2, mediaType: 'application/vnd.oci.image.manifest.v1+json', config: fixture.config, layers: [{ ...fixture.config, size: fixture.config.size + 1 }],
  });
  await assert.rejects(verifyGraph(root, fixture.request), /Conflicting descriptor/);
});

test('same-byte arbitrary images do not satisfy the reviewed source allowlist', () => {
  const arbitrary = Buffer.from('{}');
  assert.throws(() => verifyIndexBytes(arbitrary, arbitrary), /Unreviewed source/);
});

test('workflow separates read-only PR validation from a bounded master-only fixed-image publication', () => {
  const text = readFileSync(new URL('../../../.github/workflows/mirror-backup-egress.yml', import.meta.url), 'utf8');
  const workflow = parse(text);
  assert.deepEqual(Object.keys(workflow.on).sort(), ['pull_request', 'workflow_dispatch']);
  assert.equal(workflow.on.workflow_dispatch, null);
  assert.deepEqual(workflow.permissions, { contents: 'read' });
  assert.deepEqual(Object.keys(workflow.jobs).sort(), ['mirror', 'validate']);
  assert.equal(workflow.jobs.validate.permissions, undefined);
  const publish = workflow.jobs.mirror;
  assert.equal(publish.if, "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master' && github.repository == 'memeloop-online/memeloop-token-center'");
  assert.equal(publish.needs, 'validate');
  assert.deepEqual(publish.permissions, { contents: 'read', packages: 'write' });
  assert.equal(publish['timeout-minutes'], 30);
  assert.equal(sourceDigest, 'sha256:bac1a74de365ea59270ba134016762c6be05654d55246518de256113bd1c225e');
  assert.equal(publish.env.SOURCE_IMAGE, sourceImage);
  assert.equal(publish.env.SOURCE_DIGEST, sourceDigest);
  assert.equal(publish.env.TARGET_IMAGE, targetImage);
  assert.equal(publish.env.TARGET_TAG, targetTag);
  assert.doesNotMatch(text, /pull_request_target|repository_dispatch|inputs\.|--platform|--all-tags|build-push-action|kubectl|KUBECONFIG|id-token:/);
  assert.deepEqual([...text.matchAll(/secrets\.([A-Z_]+)/g)].map(match => match[1]), ['GITHUB_TOKEN']);
  const copy = publish.steps.find((step: { id?: string }) => step.id === 'copy').run;
  assert.match(copy, /timeout --kill-after=10s 12m crane copy --jobs 2 --no-clobber "\$source_ref" "\$target_ref"/);
  assert.match(copy, /\[\[ "\$existing_digest" == "\$SOURCE_DIGEST" \]\]/);
  assert.match(copy, /cmp "\$evidence\/source-index.json" "\$evidence\/target-index.json"/);
  const artifact = publish.steps.find((step: { uses?: string }) => step.uses?.startsWith('actions/upload-artifact@'));
  assert.equal(artifact.if, "always() && steps.copy.outcome == 'success'");
  assert.doesNotMatch(artifact.with.path, /docker|auth|\*|token/);
});
