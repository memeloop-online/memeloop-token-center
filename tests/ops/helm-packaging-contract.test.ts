import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { occurrences, read, repository, run } from './contract-helpers.ts';

const chart = join(repository, 'charts/memeloop-token-center');
const helm = process.env.HELM_BIN ?? 'helm';
const reviewed = `sha256:${'a'.repeat(64)}`;
const installer = `sha256:${'c'.repeat(64)}`;
const artifact = `sha256:${'d'.repeat(64)}`;

test('Helm chart packaging, security, ingress, and schema contracts', () => {
  const workspace = mkdtempSync(join(tmpdir(), 'mtc-helm-contract-'));
  try {
    run(helm, ['lint', '--strict', chart]);
    run(helm, ['lint', '--strict', chart, '--values', join(chart, 'values-dev.yaml')]);
    const render = (name: string, flags: string[] = []): string => run(helm, ['template', `token-center-${name}`, chart, '--namespace', 'token-center', ...flags]);
    const output: Record<string, string> = {
      default: render('default'),
      dev: render('dev', ['--values', join(chart, 'values-dev.yaml')]),
      observed: render('observed', ['--set', 'serviceMonitor.enabled=true', '--set', 'roles.gateway.autoscaling.enabled=true']),
      digest: render('digest', ['--set-string', 'image.tag=must-not-render', '--set-string', `image.digest=${reviewed}`]),
      configmap: render('configmap-plugin', ['--set', 'plugins.enabled=true', '--set', 'plugins.existingConfigMap=token-center-plugins']),
      pvc: render('pvc-plugin', ['--set', 'plugins.enabled=true', '--set', 'plugins.existingClaim=token-center-plugins']),
      oci: render('oci-plugin', [
        '--set', 'plugins.enabled=true', '--set', 'plugins.ociInstaller.enabled=true',
        '--set-string', `plugins.ociInstaller.image.digest=${installer}`,
        '--set-string', `plugins.ociInstaller.artifacts[0].reference=ghcr.io/example/plugin@${artifact}`,
        '--set-string', 'plugins.ociInstaller.artifacts[0].allowedSource=ghcr.io/example/plugin',
        '--set-string', 'plugins.ociInstaller.cosignPublicKeysSecret.name=plugin-cosign-keys',
        '--set-string', 'plugins.ociInstaller.cosignPublicKeysSecret.keys[0]=cosign.pub',
        '--set-string', 'plugins.ociInstaller.registryAuthSecret.name=plugin-registry-auth',
      ]),
      migration: render('migration', ['--show-only', 'templates/migration-job.yaml', '--set', 'imagePullSecrets[0].name=registry-credentials']),
      webhook: render('webhook', ['--set', 'config.memeloopCloudWebhookSecret.name=memeloop-cloud-integration', '--set', 'config.memeloopCloudWebhookSecret.key=webhook-secret']),
      gateway: render('gateway', ['--show-only', 'templates/ingress.yaml', '--set', 'ingress.gateway.enabled=true', '--set', 'ingress.gateway.className=public-gateway', '--set', 'ingress.gateway.sourceRanges[0]=100.64.0.2/32', '--set-string', 'ingress.gateway.annotations.marker=gateway-only', '--set', 'ingress.gateway.host=gateway.example.test', '--set', 'ingress.gateway.tlsSecretName=gateway-tls']),
      control: render('control', ['--show-only', 'templates/ingress.yaml', '--set', 'ingress.control.enabled=true', '--set', 'ingress.control.className=higress-private', '--set', 'ingress.control.sourceRanges[0]=10.0.0.0/8', '--set-string', 'ingress.control.annotations.marker=control-only', '--set', 'ingress.control.host=control.internal.example.test', '--set', 'ingress.control.tlsSecretName=control-tls']),
      both: render('both', ['--show-only', 'templates/ingress.yaml', '--set', 'ingress.gateway.enabled=true', '--set', 'ingress.gateway.className=public-gateway', '--set', 'ingress.gateway.sourceRanges[0]=100.64.0.2/32', '--set-string', 'ingress.gateway.annotations.marker=gateway-only', '--set', 'ingress.gateway.host=gateway.example.test', '--set', 'ingress.gateway.tlsSecretName=gateway-tls', '--set', 'ingress.control.enabled=true', '--set', 'ingress.control.className=higress-private', '--set', 'ingress.control.sourceRanges[0]=10.0.0.0/8', '--set-string', 'ingress.control.annotations.marker=control-only', '--set', 'ingress.control.host=control.internal.example.test', '--set', 'ingress.control.tlsSecretName=control-tls']),
      loadBalancer: render('lb', ['--show-only', 'templates/service.yaml', '--set', 'roles.gateway.service.type=LoadBalancer']),
      hostAlias: render('host-alias', ['--show-only', 'templates/deployment.yaml', '--set-string', 'hostAliases[0].ip=10.28.0.22', '--set-string', 'hostAliases[0].hostnames[0]=private-upstream.example.test']),
      recreate: render('recreate', ['--set', 'deploymentStrategy=Recreate']),
    };
    const has = (key: string, needle: string): void => assert.ok(output[key]!.includes(needle), `${key} render lacks ${needle}`);
    const lacks = (key: string, pattern: string | RegExp): void => assert.ok(typeof pattern === 'string' ? !output[key]!.includes(pattern) : !pattern.test(output[key]!), `${key} render contains forbidden ${String(pattern)}`);
    const count = (key: string, pattern: string | RegExp, expected: number): void => assert.equal(occurrences(output[key]!, pattern), expected, `${key} count for ${String(pattern)}`);

    has('default', 'kind: NetworkPolicy'); has('default', 'kind: PodDisruptionBudget');
    has('observed', 'kind: HorizontalPodAutoscaler'); has('observed', 'kind: ServiceMonitor');
    const migrationVersions = (directory: string): number[] => readdirSync(join(repository, 'migrations', directory)).flatMap((name) => /^([0-9]{4})_.*\.sql$/.exec(name)?.[1] ?? []).map(Number);
    const sqlite = Math.max(...migrationVersions('common'), ...migrationVersions('sqlite'));
    const postgres = Math.max(...migrationVersions('common'), ...migrationVersions('postgres'));
    assert.equal(sqlite, postgres);
    assert.equal(sqlite, 59, 'release chart must require schema v59');
    assert.equal(Number(/^  schemaVersion: ([0-9]+)$/m.exec(read('charts/memeloop-token-center/values.yaml'))?.[1]), sqlite);
    has('default', `memeloop.io/schema-generation: "v${sqlite}"`);
    count('default', 'image: "ghcr.io/memeloop-online/memeloop-token-center:0.1.0"', 4);
    count('digest', `image: "ghcr.io/memeloop-online/memeloop-token-center@${reviewed}"`, 4); lacks('digest', 'must-not-render');
    count('default', 'type: RollingUpdate', 3); count('recreate', 'type: Recreate', 3); lacks('recreate', 'rollingUpdate:');
    has('configmap', 'configMap:'); has('pvc', 'persistentVolumeClaim:');
    for (const needle of ['name: install-plugin-0', `image: "ghcr.io/memeloop-online/memeloop-token-center-plugin-installer@${installer}"`, '- --registry-username-file', '- --registry-password-file', '- --cosign-public-key', 'medium: Memory', 'sizeLimit: "16Mi"', 'secretName: plugin-cosign-keys', 'secretName: plugin-registry-auth']) count('oci', needle, 3);
    count('oci', 'readOnlyRootFilesystem: true', 7); count('oci', 'allowPrivilegeEscalation: false', 7);
    assert.ok(occurrences(output.oci!, /seccompProfile:.*RuntimeDefault/g) >= 6); lacks('oci', 'MTC_PLUGIN_REGISTRY_'); lacks('oci', /memeloop-token-center-plugin-installer:[^\s]/);
    count('default', 'name: MTC_RUN_MIGRATIONS_ON_START', 3); has('default', 'args: ["migrate"]'); has('migration', 'restartPolicy: Never'); has('migration', '- name: registry-credentials');
    count('default', 'name: MTC_ARCHIVE_BACKEND', 3); count('default', 'value: "s3"', 3); lacks('default', 'name: MTC_MEMELOOP_CLOUD_WEBHOOK_SECRET'); has('webhook', 'name: memeloop-cloud-integration'); has('webhook', 'key: webhook-secret');
    count('default', /^kind: Ingress$/gm, 0); count('gateway', /^kind: Ingress$/gm, 1); count('control', /^kind: Ingress$/gm, 1); count('both', /^kind: Ingress$/gm, 2);
    for (const needle of ['ingressClassName: public-gateway', 'marker: gateway-only', '100.64.0.2/32', 'host: "gateway.example.test"', 'secretName: gateway-tls', '- path: /v1', '- path: /self', '- path: /portal', '- path: /ui-assets']) has('gateway', needle);
    lacks('gateway', /control\.internal|higress-private|control-only|control-tls|path:\s*\/internal|path:\s*\/operator/);
    for (const needle of ['ingressClassName: higress-private', 'marker: control-only', '10.0.0.0/8', 'ssl-redirect: "true"', 'force-ssl-redirect: "true"', 'host: "control.internal.example.test"', 'secretName: control-tls', '- path: /operator', '- path: /ui-assets', '- path: /internal/v1']) has('control', needle);
    lacks('control', /gateway\.example|public-gateway|gateway-only|gateway-tls|path:\s*\/v1|path:\s*\/self|path:\s*\/portal/);
    count('both', '- path:', 8); has('loadBalancer', 'type: LoadBalancer'); count('hostAlias', 'ip: 10.28.0.22', 3); count('hostAlias', '- private-upstream.example.test', 3);
    lacks('default', /^      hostAliases:/m); lacks('default', /type:\s*(?:NodePort|LoadBalancer)/); lacks('default', /^kind:\s*Secret\s*$/m); lacks('default', /^\s*-\s*\{\}\s*$/m); lacks('default', /port:\s*(?:1080|5432|9000)(?:\D|$)/); lacks('default', /MTC_ARCHIVE_PATH|mountPath:\s*\/.*archive/);
    const chartSources = readdirSync(chart, { recursive: true, encoding: 'utf8' })
      .filter((path) => !path.endsWith('/'))
      .flatMap((path) => { try { return [readFileSync(join(chart, path), 'utf8')]; } catch { return []; } })
      .join('\n');
    assert.ok(!chartSources.includes('kubectl.kubernetes.io/last-applied-configuration'));

    const invalid: string[][] = [
      ['networkPolicy.egress.clusterDependencies.enabled=true'], ['config.archiveBackend=filesystem'], ['config.archiveBackend=memory'], ['image.digest=sha256:abc123'], ['probes.readiness.timeoutSeconds=6'], [`image.digest=sha256:${'A'.repeat(64)}`],
      ['plugins.enabled=true'], ['plugins.enabled=true','plugins.existingConfigMap=x','plugins.existingClaim=x'], ['plugins.ociInstaller.enabled=true'],
      ['roles.gateway.replicaCounnt=2'], ['ingress.gateway.classname=nginx'], ['ingress.enabled=true'], ['ingress.gateway.enabled=true'], ['ingress.control.enabled=true'],
      ['ingress.control.enabled=true','ingress.control.host=x'], ['ingress.control.enabled=true','ingress.control.className=higress-private','ingress.control.host=x','ingress.control.sourceRanges[0]=0.0.0.0/0','ingress.control.tlsSecretName=x'],
      ['roles.control.service.type=NodePort'], ['roles.control.service.type=LoadBalancer'], ['roles.all.service.type=NodePort'], ['roles.all.service.type=LoadBalancer'],
      ['serviceAccount.automount=true'], ['plugins.mountpath=/plugins'], ['hostAliases[0].ip=10.28.0.22'], ['config.databaseMaxConnection=8'],
    ];
    for (const [index, values] of invalid.entries()) {
      const args = ['template', `invalid-${index}`, chart, ...values!.flatMap((value) => ['--set-string', value])];
      const result = spawnSync(helm, args, { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, `values schema accepted invalid case ${values!.join(',')}`);
    }
    const oldSchema = spawnSync(helm, ['template', 'invalid-old-schema', chart, '--set', 'migration.schemaVersion=58'], { cwd: repository, encoding: 'utf8', shell: false });
    assert.notEqual(oldSchema.status, 0, 'release values schema accepted migration.schemaVersion=58');

    if (process.env.KUBECONFORM_BIN) {
      const result = spawnSync(process.env.KUBECONFORM_BIN, ['-strict', '-summary', '-ignore-missing-schemas'], { cwd: repository, input: Object.values(output).join('\n---\n'), encoding: 'utf8', shell: false });
      assert.equal(result.status, 0, result.stderr);
    }
  } finally { rmSync(workspace, { recursive: true, force: true }); }
});
