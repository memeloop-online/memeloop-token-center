import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { parseAllDocuments } from 'yaml';
import test from 'node:test';
import { occurrences, read, repository, run } from './contract-helpers.ts';

const chart = join(repository, 'charts/memeloop-token-center');
const helm = process.env.HELM_BIN ?? 'helm';
const reviewed = `sha256:${'a'.repeat(64)}`;
const installer = `sha256:${'c'.repeat(64)}`;
const artifact = `sha256:${'d'.repeat(64)}`;
const runtimeFlags = [
  '--set', 'plugins.runtimeInventory.enabled=true',
  '--set', 'plugins.runtimeInventory.existingClaim=shared-plugin-inventories',
  '--set', 'plugins.runtimeInventory.installationEnabled=true',
  '--set', 'plugins.runtimeInventory.policyConfigMap=plugin-install-policy',
  '--set', 'plugins.runtimeInventory.cosignPublicKeysSecret.name=publisher-trust',
  '--set', 'plugins.runtimeInventory.cosignPublicKeysSecret.keys[0]=publisher.pem',
  '--set', 'plugins.runtimeInventory.registrySecrets[0].name=private-registry-auth',
  '--set', 'plugins.runtimeInventory.registrySecrets[0].keys[0]=username',
  '--set', 'plugins.runtimeInventory.registrySecrets[0].keys[1]=password',
];

interface RuntimeDeployment {
  kind: string;
  metadata: { annotations: Record<string, string> };
  spec: { template: { metadata: { labels: Record<string, string> }; spec: {
    securityContext: { runAsUser: number; runAsGroup: number; fsGroup: number };
    containers: Array<{ image: string; env: Array<{ name: string }>; volumeMounts: Array<{ name: string; readOnly?: boolean; subPath?: string }> }>;
    initContainers: Array<{ name: string; image: string; args: string[]; volumeMounts: Array<{ readOnly?: boolean }> }>;
    volumes: Array<{ name: string; emptyDir?: { sizeLimit?: string; medium?: string }; persistentVolumeClaim?: { claimName: string; readOnly: boolean }; secret?: { secretName: string } }>;
  } } };
}

test('Helm chart packaging, security, ingress, and schema contracts', () => {
  const workspace = mkdtempSync(join(tmpdir(), 'mtc-helm-contract-'));
  try {
    run(helm, ['lint', '--strict', chart]);
    run(helm, ['lint', '--strict', chart, '--values', join(chart, 'values-dev.yaml')]);
    const render = (name: string, flags: string[] = []): string => run(helm, ['template', `token-center-${name}`, chart, '--namespace', 'token-center', ...flags]);
    const output: Record<string, string> = {
      default: render('default'),
      runtime: render('runtime', runtimeFlags),
      runtimeReaders: render('runtime-readers', runtimeFlags.slice(0, 4)),
      runtimeCreated: render('runtime-created', ['--set', 'plugins.runtimeInventory.enabled=true', '--set', 'plugins.runtimeInventory.persistence.create=true', '--set', 'plugins.runtimeInventory.persistence.storageClass=reviewed-rwx']),
      dev: render('dev', ['--values', join(chart, 'values-dev.yaml')]),
      observed: render('observed', ['--set', 'serviceMonitor.enabled=true', '--set', 'roles.gateway.autoscaling.enabled=true']),
      gatewayMetrics: render('gateway-metrics', ['--show-only', 'templates/servicemonitor.yaml', '--set', 'serviceMonitor.enabled=true', '--set', 'roles.control.enabled=false']),
      profiling: render('profiling', ['--show-only', 'templates/deployment.yaml', '--set', 'config.runtimeProfiling.enabled=true']),
      archiveCompression: render('archive-compression', ['--show-only', 'templates/deployment.yaml', '--set', 'config.archiveSpoolCompression.enabled=true', '--set', 'config.archiveObjectCompression.enabled=true']),
      failureDomainEnforced: render('failure-domain-enforced', ['--show-only', 'templates/deployment.yaml', '--set', 'config.upstreamHealth.failureDomainEnforcementEnabled=true']),
      digest: render('digest', ['--set-string', 'image.tag=must-not-render', '--set-string', `image.digest=${reviewed}`]),
      retainedWorker: render('retained-worker', [...runtimeFlags, '--set-string', `image.digest=${reviewed}`, '--set-string', 'roles.worker.image.repository=ghcr.io/memeloop-online/memeloop-token-center', '--set-string', `roles.worker.image.digest=${artifact}`]),
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
      archiveReadyBoundary: render('archive-ready-boundary', ['--set', 'config.s3.readinessDeadlineMillis=5001', '--set', 'probes.readiness.timeoutSeconds=8']),
      archiveLive: render('archive-live', ['--set', 'config.s3.readinessDeadlineMillis=30000', '--set', 'probes.readiness.path=/livez']),
      proxyMemory: render('proxy-memory', ['--set', 'config.proxyMemoryBudgetBytes=536870912', '--set', 'roles.gateway.resources.requests.memory=512Mi', '--set', 'roles.gateway.resources.limits.memory=768Mi']),
      fractionalMemory: render('fractional-memory', ['--set', 'roles.gateway.resources.limits.memory=0.75Gi']),
      maximumBodyMemory: render('maximum-body-memory', ['--set', 'config.responsesBodyMaxBytes=67108864', '--set', 'config.proxyMemoryBudgetBytes=1073741824', '--set', 'roles.gateway.resources.requests.memory=1Gi', '--set', 'roles.gateway.resources.limits.memory=1280Mi']),
    };
    const has = (key: string, needle: string): void => assert.ok(output[key]!.includes(needle), `${key} render lacks ${needle}`);
    const lacks = (key: string, pattern: string | RegExp): void => assert.ok(typeof pattern === 'string' ? !output[key]!.includes(pattern) : !pattern.test(output[key]!), `${key} render contains forbidden ${String(pattern)}`);
    const count = (key: string, pattern: string | RegExp, expected: number): void => assert.equal(occurrences(output[key]!, pattern), expected, `${key} count for ${String(pattern)}`);

    count('runtime', 'name: MTC_PLUGIN_INVENTORY_FILE', 3);
    count('runtime', 'name: MTC_PLUGIN_INSTALL_POLICY_FILE', 1);
    count('runtimeReaders', 'name: MTC_PLUGIN_INSTALL_POLICY_FILE', 0);
    lacks('runtimeReaders', 'secretName:');
    lacks('runtime', 'subPath:');
    lacks('runtime', 'MTC_PLUGIN_DIR');
    has('runtimeCreated', 'accessModes: [ReadWriteMany]');
    has('runtimeCreated', 'helm.sh/resource-policy: keep');
    has('runtimeCreated', 'storage: "1Gi"');
    has('runtimeCreated', 'storageClassName: "reviewed-rwx"');
    for (const deployment of parseAllDocuments(output.runtime!).map(document => document.toJSON() as RuntimeDeployment).filter(document => document?.kind === 'Deployment')) {
      const role = deployment.spec.template.metadata.labels['app.kubernetes.io/component'];
      const writer = role === 'control';
      const pod = deployment.spec.template.spec;
      const container = pod.containers[0]!;
      assert.equal(pod.securityContext.runAsUser, 10001);
      assert.equal(pod.securityContext.runAsGroup, 10001);
      assert.equal(pod.securityContext.fsGroup, 10001);
      const mount = container.volumeMounts.find(item => item.name === 'plugin-runtime-inventory')!;
      assert.equal(mount.readOnly, !writer, `${role}: live service write boundary`);
      const volume = pod.volumes.find(item => item.name === 'plugin-runtime-inventory')!;
      assert.equal(volume.persistentVolumeClaim?.claimName, 'shared-plugin-inventories');
      assert.equal(volume.persistentVolumeClaim?.readOnly, !writer);
      const prepare = pod.initContainers.find(item => item.name === 'prepare-plugin-inventory')!;
      assert.equal(prepare.args.includes('--read-only'), !writer, `${role}: readers never initialize`);
      assert.equal(prepare.volumeMounts[0]!.readOnly, !writer);
      assert.equal(pod.volumes.some(item => item.secret?.secretName === 'publisher-trust'), writer);
      assert.equal(pod.volumes.some(item => item.secret?.secretName === 'private-registry-auth'), writer);
      assert.equal(container.env.some(item => item.name === 'MTC_PLUGIN_INSTALL_POLICY_FILE'), writer);
    }

    has('default', 'kind: NetworkPolicy'); has('default', 'kind: PodDisruptionBudget');
    count('default', /name: MTC_PROXY_MEMORY_BUDGET_BYTES\n\s+value: "536870912"/, 3);
    count('proxyMemory', /name: MTC_PROXY_MEMORY_BUDGET_BYTES\n\s+value: "536870912"/, 3);
    const gatewayDeployment = output.default!.split(/^---$/m).find((document) => document.includes('kind: Deployment') && document.includes('app.kubernetes.io/component: gateway'))!;
    assert.match(gatewayDeployment, /limits:\s+cpu: [^\n]+\s+ephemeral-storage: 256Mi\s+memory: 1Gi/);
    assert.match(gatewayDeployment, /requests:\s+cpu: [^\n]+\s+ephemeral-storage: 64Mi\s+memory: 256Mi/);
    has('fractionalMemory', 'memory: 0.75Gi');
    count('default', /name: MTC_RESPONSES_REQUEST_SPOOL_BYTES\n\s+value: "134217728"/, 3);
    count('default', /name: MTC_RESPONSES_REQUEST_SPOOL_PATH\n\s+value: "\/var\/lib\/memeloop-token-center\/request-spool"/, 3);
    const deployments = parseAllDocuments(output.default!).map(document => document.toJSON() as RuntimeDeployment).filter(document => document?.kind === 'Deployment');
    for (const deployment of deployments) {
      const role = deployment.spec.template.metadata.labels['app.kubernetes.io/component'];
      assert.equal(deployment.metadata.annotations['argocd.argoproj.io/sync-wave'], role === 'gateway' || role === 'control' ? '1' : '0');
      const pod = deployment.spec.template.spec;
      const spoolMount = pod.containers[0]!.volumeMounts?.find(item => item.name === 'responses-request-spool');
      const spoolVolume = pod.volumes?.find(item => item.name === 'responses-request-spool');
      if (role === 'gateway' || role === 'all') {
        assert.equal(spoolMount?.readOnly, false, `${role}: request spool mount must be writable`);
        assert.equal(spoolVolume?.emptyDir?.sizeLimit, '160Mi', `${role}: request spool must use bounded emptyDir`);
        assert.equal(spoolVolume?.emptyDir?.medium, undefined, `${role}: request spool must use node disk`);
      } else {
        assert.equal(spoolMount, undefined, `${role}: does not accept Responses bodies`);
        assert.equal(spoolVolume, undefined, `${role}: does not accept Responses bodies`);
      }
    }
    for (const deployment of output.default!.split(/^---$/m).filter((document) => document.includes('kind: Deployment'))) {
      for (const [name, value] of [['CONNECT_TIMEOUT', '5000'], ['REQUEST_TIMEOUT', '30000'], ['READINESS_DEADLINE', '5000']]) {
        assert.match(deployment, new RegExp(`name: MTC_S3_${name}_MILLIS\\s+value: "${value}"`), 'every role must use the same bounded S3 defaults');
      }
    }
    has('observed', 'kind: HorizontalPodAutoscaler'); has('observed', 'kind: ServiceMonitor');
    for (const variant of ['observed', 'gatewayMetrics']) {
      const monitor = output[variant]!.split(/^---$/m).find((document) => document.includes('kind: ServiceMonitor'))!;
      assert.match(monitor, /matchExpressions:\s+- key: app.kubernetes.io\/component\s+operator: In\s+values: \[gateway, control, all\]/);
      assert.match(monitor, /authorization:\s+type: Bearer\s+credentials:/);
      assert.doesNotMatch(monitor, /app.kubernetes.io\/component: control/);
    }
    const migrationVersions = (directory: string): number[] => readdirSync(join(repository, 'migrations', directory)).flatMap((name) => /^([0-9]{4})_.*\.sql$/.exec(name)?.[1] ?? []).map(Number);
    const sqlite = Math.max(...migrationVersions('common'), ...migrationVersions('sqlite'));
    const postgres = Math.max(...migrationVersions('common'), ...migrationVersions('postgres'));
    assert.equal(sqlite, postgres);
    assert.equal(Number(/^  schemaVersion: ([0-9]+)$/m.exec(read('charts/memeloop-token-center/values.yaml'))?.[1]), sqlite);
    const valuesSchema = JSON.parse(read('charts/memeloop-token-center/values.schema.json'));
    assert.equal(valuesSchema.properties.migration.properties.schemaVersion.const, sqlite);
    has('default', `memeloop.io/schema-generation: "v${sqlite}"`);
    count('default', 'image: "ghcr.io/memeloop-online/memeloop-token-center:v0.1.2"', 4);
    count('digest', `image: "ghcr.io/memeloop-online/memeloop-token-center@${reviewed}"`, 4); lacks('digest', 'must-not-render');
    for (const deployment of parseAllDocuments(output.retainedWorker!).map(document => document.toJSON() as RuntimeDeployment).filter(document => document?.kind === 'Deployment')) {
      const role = deployment.spec.template.metadata.labels['app.kubernetes.io/component'];
      const expected = `ghcr.io/memeloop-online/memeloop-token-center@${role === 'worker' ? artifact : reviewed}`;
      assert.equal(deployment.spec.template.spec.containers[0]!.image, expected, `${role}: rollback role image`);
      assert.equal(deployment.spec.template.spec.initContainers.find(item => item.name === 'prepare-plugin-inventory')!.image, expected, `${role}: init/runtime image compatibility`);
    }
    count('default', 'type: RollingUpdate', 3); count('recreate', 'type: Recreate', 3); lacks('recreate', 'rollingUpdate:');
    has('configmap', 'configMap:'); has('pvc', 'persistentVolumeClaim:');
    for (const needle of ['name: install-plugin-0', `image: "ghcr.io/memeloop-online/memeloop-token-center-plugin-installer@${installer}"`, '- --registry-username-file', '- --registry-password-file', '- --cosign-public-key', 'medium: Memory', 'sizeLimit: "16Mi"', 'secretName: plugin-cosign-keys', 'secretName: plugin-registry-auth']) count('oci', needle, 3);
    count('oci', 'readOnlyRootFilesystem: true', 7); count('oci', 'allowPrivilegeEscalation: false', 7);
    assert.ok(occurrences(output.oci!, /seccompProfile:.*RuntimeDefault/g) >= 6); lacks('oci', 'MTC_PLUGIN_REGISTRY_'); lacks('oci', /memeloop-token-center-plugin-installer:[^\s]/);
    count('default', 'name: MTC_RUN_MIGRATIONS_ON_START', 3); has('default', 'args: ["migrate"]'); has('migration', 'restartPolicy: Never'); has('migration', '- name: registry-credentials');
    count('default', 'name: MTC_ARCHIVE_BACKEND', 3); count('default', 'value: "s3"', 3); lacks('default', 'name: MTC_MEMELOOP_CLOUD_WEBHOOK_SECRET'); has('webhook', 'name: memeloop-cloud-integration'); has('webhook', 'key: webhook-secret');
    count('default', 'name: MTC_GATEWAY_BODY_READ_CONCURRENCY', 3); count('default', 'value: "1024"', 3);
    for (const identity of ['MTC_GATEWAY_POD_NAME', 'MTC_GATEWAY_NODE_NAME', 'MTC_GATEWAY_FAILURE_DOMAIN']) count('default', `name: ${identity}`, 3);
    count('default', /name: MTC_UPSTREAM_HEALTH_FAILURE_DOMAIN_ENFORCEMENT_ENABLED\n\s+value: "false"/, 3);
    count('failureDomainEnforced', /name: MTC_UPSTREAM_HEALTH_FAILURE_DOMAIN_ENFORCEMENT_ENABLED\n\s+value: "true"/, 3);
    count('default', 'name: MTC_RESPONSES_BODY_MAX_BYTES', 3); count('default', 'value: "33554432"', 3); count('default', 'name: MTC_RESPONSES_BODY_READ_CONCURRENCY', 3);
    count('default', 'name: MTC_AUDIO_BODY_MAX_BYTES', 3); count('default', 'value: "26214400"', 3);
    count('default', /name: MTC_RUNTIME_PROFILING_ENABLED\n\s+value: "false"/, 1);
    count('profiling', /name: MTC_RUNTIME_PROFILING_ENABLED\n\s+value: "true"/, 1);
    count('default', /name: MTC_ARCHIVE_SPOOL_COMPRESSION_ENABLED\n\s+value: "false"/, 3);
    count('archiveCompression', /name: MTC_ARCHIVE_SPOOL_COMPRESSION_ENABLED\n\s+value: "true"/, 3);
    count('default', /name: MTC_ARCHIVE_OBJECT_COMPRESSION_ENABLED\n\s+value: "false"/, 3);
    count('archiveCompression', /name: MTC_ARCHIVE_OBJECT_COMPRESSION_ENABLED\n\s+value: "true"/, 3);
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
      ['plugins.runtimeInventory.enabled=true'],
      ['plugins.runtimeInventory.enabled=true','plugins.runtimeInventory.persistence.create=true'],
      ['plugins.runtimeInventory.enabled=true','plugins.runtimeInventory.existingClaim=shared','plugins.runtimeInventory.persistence.create=true','plugins.runtimeInventory.persistence.storageClass=rwx'],
      ['plugins.runtimeInventory.enabled=true','plugins.runtimeInventory.existingClaim=shared','plugins.runtimeInventory.installationEnabled=true'],
      ['plugins.runtimeInventory.enabled=true','plugins.runtimeInventory.existingClaim=shared','plugins.enabled=true','plugins.existingConfigMap=legacy'],
      ['plugins.runtimeInventory.enabled=true','plugins.runtimeInventory.existingClaim=shared','roles.control.enabled=false'],
      ['plugins.runtimeInventory.cosignPublicKeysSecret.keys[0]=../escape'],
      ['plugins.runtimeInventory.registrySecrets[0].name=registry','plugins.runtimeInventory.registrySecrets[0].keys[0]=../escape'],
      ['networkPolicy.egress.clusterDependencies.enabled=true'], ['config.archiveBackend=filesystem'], ['config.archiveBackend=memory'], ['image.digest=sha256:abc123'], ['probes.readiness.timeoutSeconds=6'], [`image.digest=sha256:${'A'.repeat(64)}`],
      ['plugins.enabled=true'], ['plugins.enabled=true','plugins.existingConfigMap=x','plugins.existingClaim=x'], ['plugins.ociInstaller.enabled=true'],
      ['roles.gateway.replicaCounnt=2'], ['ingress.gateway.classname=nginx'], ['ingress.enabled=true'], ['ingress.gateway.enabled=true'], ['ingress.control.enabled=true'],
      ['ingress.control.enabled=true','ingress.control.host=x'], ['ingress.control.enabled=true','ingress.control.className=higress-private','ingress.control.host=x','ingress.control.sourceRanges[0]=0.0.0.0/0','ingress.control.tlsSecretName=x'],
      ['roles.control.service.type=NodePort'], ['roles.control.service.type=LoadBalancer'], ['roles.all.service.type=NodePort'], ['roles.all.service.type=LoadBalancer'],
      ['serviceAccount.automount=true'], ['plugins.mountpath=/plugins'], ['hostAliases[0].ip=10.28.0.22'], ['config.databaseMaxConnection=8'], ['config.gatewayBodyReadConcurrency=8193'], ['config.responsesBodyMaxBytes=67108865'], ['config.audioBodyMaxBytes=-1'], ['config.responsesBodyReadConcurrency=9'],
      ['config.runtimeProfiling.enabled=not-a-boolean'], ['config.runtimeProfiling.unknown=true'],
      ['config.archiveSpoolCompression.enabled=not-a-boolean'], ['config.archiveSpoolCompression.unknown=true'],
      ['config.archiveObjectCompression.enabled=not-a-boolean'], ['config.archiveObjectCompression.unknown=true'],
      ['config.proxyMemoryBudgetBytes=268435455'], ['config.proxyMemoryBudgetBytes=2147483649'],
      ['config.responsesRequestSpoolBytes=4194303'], ['config.responsesRequestSpoolBytes=2147483649'], ['requestSpool.mountPath=relative/path'], ['requestSpool.mountPath=/'],
    ];
    for (const [index, values] of invalid.entries()) {
      const args = ['template', `invalid-${index}`, chart, ...values!.flatMap((value) => ['--set-string', value])];
      const result = spawnSync(helm, args, { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, `values schema accepted invalid case ${values!.join(',')}`);
    }
    for (const values of [
      ['config.s3.readinessDeadlineMillis=5001', 'probes.readiness.timeoutSeconds=7'],
      ['config.s3.readinessDeadlineMillis=30000', 'probes.readiness.timeoutSeconds=31'],
      ['config.s3.connectTimeoutMillis=5000', 'config.s3.requestTimeoutMillis=4999'],
    ]) {
      const result = spawnSync(helm, ['template', 'invalid-archive-budget', chart, ...values.flatMap((value) => ['--set', value])], { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, `archive cross-field validation accepted ${values.join(',')}`);
      assert.match(result.stderr, /archive deadline rounded up plus 2 seconds|connectTimeoutMillis must not exceed requestTimeoutMillis/);
    }
    const oldSchema = spawnSync(helm, ['template', 'invalid-old-schema', chart, '--set', 'migration.schemaVersion=58'], { cwd: repository, encoding: 'utf8', shell: false });
    for (const values of [
      ['roles.gateway.resources.limits.memory=767Mi'],
      ['roles.all.enabled=true', 'roles.gateway.enabled=false', 'roles.control.enabled=false', 'roles.worker.enabled=false', 'roles.all.resources.limits.memory=767Mi'],
      ['config.proxyMemoryBudgetBytes=805306369'],
    ]) {
      const result = spawnSync(helm, ['template', 'invalid-workload-budget', chart, ...values.flatMap((value) => ['--set', value])], { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, `memory cross-field gate accepted ${values.join(',')}`);
      assert.match(result.stderr, /memory must cover config.proxyMemoryBudgetBytes plus 256Mi/);
    }
    for (const values of [
      ['config.responsesRequestSpoolBytes=33554431'],
      ['requestSpool.sizeLimit=127Mi'],
      ['requestSpool.mountPath=/var/lib/../request-spool'],
      ['roles.gateway.resources.limits.ephemeral-storage=223Mi'],
      ['roles.all.enabled=true', 'roles.gateway.enabled=false', 'roles.control.enabled=false', 'roles.worker.enabled=false', 'roles.all.resources.limits.ephemeral-storage=223Mi'],
    ]) {
      const result = spawnSync(helm, ['template', 'invalid-spool-budget', chart, ...values.flatMap((value) => ['--set', value])], { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, `request spool cross-field gate accepted ${values.join(',')}`);
      assert.match(result.stderr, /responsesRequestSpoolBytes must cover|requestSpool.sizeLimit must cover|requestSpool.mountPath must be|ephemeral-storage must cover/);
    }
    assert.notEqual(oldSchema.status, 0, 'release values schema accepted migration.schemaVersion=58');
    for (const budget of ['268435456', '805306368']) {
      const result = spawnSync(helm, ['template', 'invalid-body-memory-budget', chart,
        '--set', 'config.responsesBodyMaxBytes=67108864',
        '--set', `config.proxyMemoryBudgetBytes=${budget}`,
        '--set', 'roles.gateway.resources.limits.memory=1280Mi'],
      { cwd: repository, encoding: 'utf8', shell: false });
      assert.notEqual(result.status, 0, '64Mi body accepted insufficient retained-memory headroom');
      assert.match(result.stderr, /responsesBodyMaxBytes times 12 plus 1Mi/);
    }

    if (process.env.KUBECONFORM_BIN) {
      const result = spawnSync(process.env.KUBECONFORM_BIN, ['-strict', '-summary', '-ignore-missing-schemas'], { cwd: repository, input: Object.values(output).join('\n---\n'), encoding: 'utf8', shell: false });
      assert.equal(result.status, 0, result.stderr);
    }
  } finally { rmSync(workspace, { recursive: true, force: true }); }
});
