import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
import test from 'node:test';
import {
  fixtureDirectory, parseDocuments, readFixtureBytes, resourceFiles, sourceCommit,
  validateExistingEgress, validateStructure, verifyProvenance, type Manifest,
} from './contract.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Backup egress automated checks run only in GitHub Actions');

interface Fixture {
  resources: Manifest[];
  kustomization: Manifest;
  parent: Manifest;
  baseline: Manifest;
}

function fixture(): Fixture {
  const bytes = readFixtureBytes();
  const document = (name: string): Manifest => parseDocuments(bytes[name]!.toString('utf8'))[0]!;
  return {
    resources: resourceFiles.flatMap(name => parseDocuments(bytes[name]!.toString('utf8'))),
    kustomization: document('kustomization.yaml'),
    parent: document('test-kustomization.yaml'),
    baseline: document('existing-networkpolicy-values.yaml'),
  };
}

const deployment = (value: Fixture): Manifest => value.resources.find(resource => resource.kind === 'Deployment')!;
const pod = (value: Fixture): Manifest => deployment(value).spec.template.spec;
const container = (value: Fixture): Manifest => pod(value).containers[0];
const service = (value: Fixture): Manifest => value.resources.find(resource => resource.kind === 'Service')!;
const proxyPolicy = (value: Fixture): Manifest => value.resources.find(resource => resource.kind === 'NetworkPolicy' && resource.metadata.name === 'mtc-backup-egress')!;
const clientPolicy = (value: Fixture): Manifest => value.resources.find(resource => resource.metadata.name === 'mtc-test-to-backup-egress')!;

test('fixture provenance pins the full reviewed GitOps commit, bytes, and original Git blobs', () => {
  verifyProvenance(readFixtureBytes());
  console.log(`Reviewed source: ${sourceCommit}; static preparation only, no runtime or GitOps deployment acceptance`);
});

test('fixture integrity rejects byte edits, unreviewed provenance, omissions, and extra inputs', () => {
  const edits = readFixtureBytes();
  edits['deployment.yaml'] = Buffer.concat([edits['deployment.yaml']!, Buffer.from('\n')]);
  assert.throws(() => verifyProvenance(edits), /Fixture hash mismatch/);
  const changedRevision = readFixtureBytes();
  changedRevision['provenance.json'] = Buffer.from(changedRevision['provenance.json']!.toString().replace(sourceCommit, '0'.repeat(40)));
  assert.throws(() => verifyProvenance(changedRevision), /Reviewed provenance changed/);
  const missing = readFixtureBytes();
  delete missing['networkpolicy.yaml'];
  assert.throws(() => verifyProvenance(missing), /Only reviewed fixture files/);
  const extra = readFixtureBytes();
  extra['external-script.ts'] = Buffer.from('throw new Error("never execute fixture code");');
  assert.throws(() => verifyProvenance(extra), /Only reviewed fixture files/);
});

test('reviewed structure preserves isolated Secret/emptyDir preparation and additive egress prerequisites', () => {
  const value = fixture();
  validateStructure(value.resources, value.kustomization, value.parent);
  validateExistingEgress(value.baseline);
});

const mutations: Array<[string, (value: Fixture) => void]> = [
  ['activation before review', value => { deployment(value).spec.replicas = 1; }],
  ['production namespace', value => { value.kustomization.namespace = 'memeloop-token-center'; }],
  ['external kustomize input', value => { value.kustomization.resources.push('https://example.invalid/unreviewed.yaml'); }],
  ['removed test integration', value => { value.parent.resources = []; }],
  ['additional Secret resource', value => { value.resources.push({ apiVersion: 'v1', kind: 'Secret', metadata: { name: 'unreviewed' } }); }],
  ['host networking', value => { pod(value).hostNetwork = true; }],
  ['host PID sharing', value => { pod(value).hostPID = true; }],
  ['service account token', value => { pod(value).automountServiceAccountToken = true; }],
  ['unreviewed init command', value => { pod(value).initContainers = [{ name: 'unreviewed' }]; }],
  ['wrong node placement', value => { pod(value).nodeSelector['kubernetes.io/hostname'] = 'westlake'; }],
  ['root process', value => { pod(value).securityContext.runAsUser = 0; }],
  ['unconfined seccomp', value => { pod(value).securityContext.seccompProfile.type = 'Unconfined'; }],
  ['privilege escalation', value => { container(value).securityContext.allowPrivilegeEscalation = true; }],
  ['writable root filesystem', value => { container(value).securityContext.readOnlyRootFilesystem = false; }],
  ['network administration capability', value => { container(value).securityContext.capabilities.add = ['NET_ADMIN']; }],
  ['mutable image reference', value => { container(value).image = 'docker.io/metacubex/mihomo:latest'; }],
  ['alternate executable', value => { container(value).command = ['/bin/sh']; }],
  ['alternate configuration', value => { container(value).args = ['-f', '/untrusted/config.yaml']; }],
  ['inline runtime configuration', value => { container(value).env = [{ name: 'CONFIG', value: 'unreviewed' }]; }],
  ['missing resource bound', value => { delete container(value).resources.limits.memory; }],
  ['raw subscription mount', value => { pod(value).volumes[0].secret.secretName = 'mtc-backup-egress-subscription'; }],
  ['writable configuration', value => { container(value).volumeMounts[0].readOnly = false; }],
  ['shared plugin PVC', value => { pod(value).volumes[1] = { name: 'runtime', persistentVolumeClaim: { claimName: 'plugin-runtime' } }; }],
  ['unbounded emptyDir', value => { delete pod(value).volumes[1].emptyDir.sizeLimit; }],
  ['public NodePort', value => { service(value).spec.type = 'NodePort'; }],
  ['public external IP', value => { service(value).spec.externalIPs = ['192.0.2.1']; }],
  ['host port', value => { container(value).ports[0].hostPort = 1080; }],
  ['UDP listener exposure', value => { service(value).spec.ports[0].protocol = 'UDP'; }],
  ['missing proxy policy', value => { value.resources = value.resources.filter(resource => resource !== proxyPolicy(value)); }],
  ['namespace-wide ingress', value => { proxyPolicy(value).spec.ingress[0].from[0].podSelector = {}; }],
  ['cross-namespace ingress', value => { proxyPolicy(value).spec.ingress[0].from[0].namespaceSelector = {}; }],
  ['namespace/pod OR instead of AND', value => {
    const peer = proxyPolicy(value).spec.ingress[0].from[0];
    proxyPolicy(value).spec.ingress[0].from = [{ namespaceSelector: peer.namespaceSelector }, { podSelector: peer.podSelector }];
  }],
  ['broad Internet egress', value => { proxyPolicy(value).spec.egress[0].to = [{ ipBlock: { cidr: '0.0.0.0/0' } }]; }],
  ['unrestricted egress ports', value => { delete proxyPolicy(value).spec.egress[0].ports; }],
  ['wrong upstream port', value => { proxyPolicy(value).spec.egress[0].ports[0].port = 80; }],
  ['missing egress isolation', value => { proxyPolicy(value).spec.policyTypes = ['Ingress']; }],
  ['additional allow-all policy', value => { value.resources.push({ kind: 'NetworkPolicy', metadata: { name: 'allow-all' }, spec: { podSelector: {}, egress: [{}] } }); }],
  ['overbroad MTC grant', value => { clientPolicy(value).spec.podSelector = {}; }],
  ['wrong grant destination', value => { clientPolicy(value).spec.egress[0].to[0].podSelector = {}; }],
  ['disabled existing isolation', value => { value.baseline.networkPolicy.enabled = false; }],
  ['missing existing DNS access', value => { value.baseline.networkPolicy.egress.dns.enabled = false; }],
  ['missing existing PostgreSQL access', value => { value.baseline.networkPolicy.egress.extraRules.shift(); }],
  ['missing existing objects access', value => { value.baseline.networkPolicy.egress.extraRules.pop(); }],
  ['missing existing SOCKS access', value => { value.baseline.networkPolicy.egress.outboundProxy.enabled = false; }],
  ['new direct Internet access', value => { value.baseline.networkPolicy.egress.publicInternet.enabled = true; }],
];

for (const [name, mutate] of mutations) {
  test(`safety contract rejects ${name} independently of fixture hashes`, () => {
    const value = fixture();
    mutate(value);
    assert.throws(() => {
      validateStructure(value.resources, value.kustomization, value.parent);
      validateExistingEgress(value.baseline);
    }, assert.AssertionError);
  });
}

test('YAML parsing rejects duplicate security keys and aliases', () => {
  assert.throws(() => parseDocuments('kind: Deployment\nkind: Secret\n'), /Invalid YAML/);
  assert.throws(() => parseDocuments('spec: &shared {}\nother: *shared\n'));
});

test('the four frozen Kubernetes resources pass the existing pinned schema validator', () => {
  assert.equal(process.env.KUBECONFORM_BIN, '/tmp/kubeconform', 'Reuse only the packaging job validator');
  execFileSync('/tmp/kubeconform', ['-strict', '-summary', '-exit-on-error', ...resourceFiles.map(name => join(fixtureDirectory, name))], {
    encoding: 'utf8', timeout: 90_000, stdio: ['ignore', 'pipe', 'pipe'],
  });
});
