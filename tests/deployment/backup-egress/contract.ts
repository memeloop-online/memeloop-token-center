import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { parseAllDocuments } from 'yaml';

export const sourceCommit = 'f48a28a127f34a85856de925debdd4e8beda25a1';
export const fixtureDirectory = join(import.meta.dirname, 'fixtures');
export const resourceFiles = ['deployment.yaml', 'service.yaml', 'networkpolicy.yaml'];
const fixtureFiles = [...resourceFiles, 'kustomization.yaml', 'test-kustomization.yaml', 'existing-networkpolicy-values.yaml'];
const namespace = 'memeloop-token-center-test';
const proxyName = 'mtc-backup-egress';
const proxyLabels = { 'app.kubernetes.io/name': proxyName };
const namespaceSelector = { matchLabels: { 'kubernetes.io/metadata.name': namespace } };
const mtcSelector = {
  matchLabels: { 'app.kubernetes.io/name': 'memeloop-token-center', 'app.kubernetes.io/instance': namespace },
  matchExpressions: [{ key: 'app.kubernetes.io/component', operator: 'In', values: ['gateway', 'control'] }],
};

export type Manifest = Record<string, any>;
export type FixtureBytes = Record<string, Buffer>;

export function readFixtureBytes(): FixtureBytes {
  return Object.fromEntries(readdirSync(fixtureDirectory).map(name => [name, readFileSync(join(fixtureDirectory, name))]));
}

export function verifyProvenance(bytes: FixtureBytes): void {
  assert.deepEqual(Object.keys(bytes).sort(), [...fixtureFiles, 'provenance.json'].sort(), 'Only reviewed fixture files are allowed');
  const provenanceBytes = bytes['provenance.json']!;
  assert.equal(createHash('sha256').update(provenanceBytes).digest('hex'), '466a592da82ebb7eb0484a041194a3b66160bef16967ae7cfa4647bf743f705a', 'Reviewed provenance changed');
  const provenance = JSON.parse(provenanceBytes.toString('utf8'));
  assert.equal(provenance.sourceCommit, sourceCommit);
  assert.equal(provenance.sourceRepository, 'llm/k3s-gitops');
  assert.equal(provenance.version, 1);
  assert.deepEqual(provenance.files.map((entry: Manifest) => entry.name).sort(), [...fixtureFiles].sort());
  for (const entry of provenance.files) {
    const content = bytes[entry.name]!;
    assert.equal(createHash('sha256').update(content).digest('hex'), entry.sha256, `Fixture hash mismatch: ${entry.name}`);
    if (entry.projection) {
      assert.equal(entry.name, 'existing-networkpolicy-values.yaml');
      assert.equal(entry.projection, 'networkPolicy');
      assert.equal(entry.sourcePath, 'apps/memeloop-token-center-test/values.yaml');
      assert.match(entry.sourceSha256, /^[a-f0-9]{64}$/);
    } else {
      const blob = createHash('sha1').update(`blob ${content.length}\0`).update(content).digest('hex');
      assert.equal(blob, entry.sourceBlob, `Git blob mismatch: ${entry.name}`);
    }
  }
}

export function parseDocuments(text: string): Manifest[] {
  return parseAllDocuments(text, { uniqueKeys: true }).map(document => {
    assert.equal(document.errors.length, 0, 'Invalid YAML');
    assert.equal(document.warnings.length, 0, 'Unsupported YAML');
    const value = document.toJS({ maxAliasCount: 0 });
    assert.ok(value && typeof value === 'object' && !Array.isArray(value), 'Expected a YAML mapping');
    return value as Manifest;
  });
}

export function validateStructure(resources: Manifest[], kustomization: Manifest, parent: Manifest): void {
  assert.deepEqual(kustomization, {
    apiVersion: 'kustomize.config.k8s.io/v1beta1', kind: 'Kustomization', namespace, resources: resourceFiles,
  });
  assert.deepEqual(parent, {
    apiVersion: 'kustomize.config.k8s.io/v1beta1', kind: 'Kustomization', namespace, resources: ['backup-egress'],
  });
  assert.deepEqual(resources.map(resource => `${resource.kind}/${resource.metadata?.name}`).sort(), [
    `Deployment/${proxyName}`, `Service/${proxyName}`, `NetworkPolicy/${proxyName}`, 'NetworkPolicy/mtc-test-to-backup-egress',
  ].sort(), 'Only the four reviewed resources are permitted');
  for (const resource of resources) assert.equal(resource.metadata.namespace, undefined, 'Namespace comes only from the reviewed kustomization');
  const deployment = resources.find(resource => resource.kind === 'Deployment')!;
  assert.equal(deployment.spec.replicas, 0, 'This fixture proves inactive preparation, not deployment');
  assert.deepEqual(deployment.spec.selector, { matchLabels: proxyLabels });
  assert.deepEqual(deployment.spec.strategy, { type: 'Recreate' });
  assert.equal(deployment.metadata.annotations['argocd.argoproj.io/sync-wave'], '1');
  const template = deployment.spec.template;
  assert.deepEqual(template.metadata.labels, { ...proxyLabels, 'app.kubernetes.io/part-of': namespace });
  const pod = template.spec;
  assert.deepEqual(Object.keys(pod).sort(), [
    'automountServiceAccountToken', 'enableServiceLinks', 'hostNetwork', 'hostPID', 'hostIPC',
    'nodeSelector', 'securityContext', 'terminationGracePeriodSeconds', 'containers', 'volumes',
  ].sort(), 'No init containers, sidecars, extra credentials, or host integrations');
  for (const field of ['automountServiceAccountToken', 'enableServiceLinks', 'hostNetwork', 'hostPID', 'hostIPC']) assert.equal(pod[field], false);
  assert.deepEqual(pod.nodeSelector, { 'kubernetes.io/hostname': 'haixia' });
  assert.deepEqual(pod.securityContext, {
    runAsNonRoot: true, runAsUser: 65532, runAsGroup: 65532, fsGroup: 65532,
    fsGroupChangePolicy: 'OnRootMismatch', seccompProfile: { type: 'RuntimeDefault' },
  });
  assert.equal(pod.containers.length, 1);
  const container = pod.containers[0];
  assert.deepEqual(Object.keys(container).sort(), [
    'name', 'image', 'imagePullPolicy', 'command', 'args', 'ports', 'securityContext',
    'resources', 'startupProbe', 'readinessProbe', 'livenessProbe', 'volumeMounts',
  ].sort());
  assert.equal(container.image, 'docker.io/metacubex/mihomo:v1.19.32@sha256:bac1a74de365ea59270ba134016762c6be05654d55246518de256113bd1c225e');
  assert.deepEqual(container.command, ['/mihomo']);
  assert.deepEqual(container.args, ['-d', '/var/lib/mihomo', '-f', '/etc/mihomo/config.yaml']);
  assert.deepEqual(container.securityContext, { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ['ALL'] } });
  assert.deepEqual(container.ports, [{ name: 'socks', containerPort: 1080, protocol: 'TCP' }]);
  assert.deepEqual(container.resources, {
    requests: { cpu: '50m', memory: '64Mi', 'ephemeral-storage': '16Mi' },
    limits: { cpu: '500m', memory: '256Mi', 'ephemeral-storage': '64Mi' },
  });
  assert.deepEqual(container.volumeMounts, [
    { name: 'config', mountPath: '/etc/mihomo', readOnly: true },
    { name: 'runtime', mountPath: '/var/lib/mihomo' },
  ]);
  assert.deepEqual(pod.volumes, [
    { name: 'config', secret: { secretName: 'mtc-backup-egress-config', optional: false, defaultMode: 288, items: [{ key: 'config.yaml', path: 'config.yaml' }] } },
    { name: 'runtime', emptyDir: { medium: 'Memory', sizeLimit: '16Mi' } },
  ]);
  const service = resources.find(resource => resource.kind === 'Service')!;
  assert.deepEqual(service.spec, {
    type: 'ClusterIP', selector: proxyLabels, ports: [{ name: 'socks', port: 1080, targetPort: 'socks', protocol: 'TCP' }],
  });
  const proxyPolicy = resources.find(resource => resource.kind === 'NetworkPolicy' && resource.metadata.name === proxyName)!;
  assert.equal(proxyPolicy.metadata.annotations['argocd.argoproj.io/sync-wave'], '-1');
  assert.deepEqual(proxyPolicy.spec, {
    podSelector: { matchLabels: proxyLabels }, policyTypes: ['Ingress', 'Egress'],
    ingress: [{ from: [{ namespaceSelector, podSelector: mtcSelector }], ports: [{ protocol: 'TCP', port: 1080 }] }],
    egress: [{ to: ['45.87.164.194/32', '45.87.165.184/32', '45.87.166.186/32'].map(cidr => ({ ipBlock: { cidr } })), ports: [{ protocol: 'TCP', port: 443 }] }],
  });
  const clientPolicy = resources.find(resource => resource.metadata.name === 'mtc-test-to-backup-egress')!;
  assert.equal(clientPolicy.metadata.annotations['argocd.argoproj.io/sync-wave'], '-1');
  assert.deepEqual(clientPolicy.spec, {
    podSelector: mtcSelector, policyTypes: ['Egress'],
    egress: [{ to: [{ namespaceSelector, podSelector: { matchLabels: proxyLabels } }], ports: [{ protocol: 'TCP', port: 1080 }] }],
  });
}

export function validateExistingEgress(values: Manifest): void {
  const policy = values.networkPolicy;
  assert.equal(policy.enabled, true, 'The additive allow assumes MTC already has its baseline policy');
  const egress = policy.egress;
  assert.deepEqual(egress.dns, {
    enabled: true,
    namespaceSelector: { matchLabels: { 'kubernetes.io/metadata.name': 'kube-system' } },
    podSelector: { matchLabels: { 'k8s-app': 'kube-dns' } },
  });
  assert.deepEqual(egress.outboundProxy, { enabled: true, cidrs: ['100.64.0.2/32'], ports: [{ protocol: 'TCP', port: 1080 }] });
  assert.equal(egress.allowSameNamespace, false);
  assert.equal(egress.clusterDependencies.enabled, false);
  assert.equal(egress.publicInternet.enabled, false);
  assert.deepEqual(egress.extraRules, [
    { to: [{ podSelector: { matchLabels: { 'cnpg.io/cluster': 'memeloop-token-center-test-pg' } } }], ports: [{ protocol: 'TCP', port: 5432 }] },
    { to: [{ podSelector: { matchLabels: { 'app.kubernetes.io/name': 'memeloop-token-center-test-objects' } } }], ports: [{ protocol: 'TCP', port: 9000 }] },
  ], 'Preserve database and objects access alongside the additive SOCKS allow');
}
