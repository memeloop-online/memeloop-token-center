import assert from 'node:assert/strict';
import test from 'node:test';
import { contains, excludes, occurrences, read } from './contract-helpers.ts';

test('session archive import Job is digest-pinned, least-privilege, and dry-run', () => {
  const jobPath = 'ops/kubernetes/session-archive-import-job.yaml';
  const job = read(jobPath);
  for (const needle of [
    'name: memeloop-token-center-session-archive-import',
    'namespace: REPLACE_TARGET_NAMESPACE',
    'image: REPLACE_PRIVATE_REGISTRY/memeloop-token-center@sha256:REPLACE_DIGEST',
    'command: ["/usr/local/bin/import-cpa-session-archive"]',
    'automountServiceAccountToken: false',
    'name: REPLACE_IMAGE_PULL_SECRET',
    'readOnlyRootFilesystem: true', 'allowPrivilegeEscalation: false', 'runAsNonRoot: true',
    'runAsUser: 10001', 'type: RuntimeDefault', 'drop: ["ALL"]',
    '- --plan-directory', '- /plan', '- --max-plan-bytes', '- "1073741824"',
    'sizeLimit: 1200Mi', 'mountPath: /source', 'secretKeyRef:',
    'name: memeloop-token-center-session-archive-import-default-deny',
    'name: memeloop-token-center-session-archive-import-egress',
    'kubernetes.io/metadata.name: kube-system', 'k8s-app: kube-dns',
    'port: 53', 'port: 5432', 'port: 9000', 'cnpg.io/cluster: REPLACE_TARGET_CNPG_CLUSTER',
    'app.kubernetes.io/name: minio',
    'value: REPLACE_TARGET_S3_ENDPOINT', 'value: REPLACE_TARGET_S3_BUCKET',
    'REPLACE_TENANT_EXTERNAL_ID', 'REPLACE_CPAMP_IMPORT_SOURCE', 'REPLACE_ARCHIVE_IMPORT_SOURCE',
    'name: MTC_S3_ALLOW_HTTP', 'value: "true"', '- Ingress', '- Egress',
  ]) contains(jobPath, needle);
  assert.equal(occurrences(job, 'namespace: REPLACE_TARGET_NAMESPACE'), 3);
  assert.equal(occurrences(job, 'kubernetes.io/metadata.name: REPLACE_TARGET_NAMESPACE'), 2);
  assert.equal(occurrences(job, /^kind: NetworkPolicy$/gm), 2);
  assert.ok(occurrences(job, 'readOnly: true') >= 2);
  excludes(jobPath, /name: MTC_(?:KEY_PEPPER|SERVICE_TOKEN)/);
  excludes(jobPath, '0.0.0.0/0');
  excludes(jobPath, /memeloop-token-center-dogfood|cpa-session-archive-v[0-9]+/);
  excludes(jobPath, /(?:mts_|mtc_|postgres:\/\/[^R]|password:\s*[^R])/);
  excludes(jobPath, /^\s+- --apply\s*$/m);

  contains('src/bin/import-cpa-session-archive.rs', 'Config::from_session_archive_import_env()');
  contains('src/bin/import-cpa-session-archive.rs', 'ensure_session_archive_import_schema()');
  excludes('src/bin/import-cpa-session-archive.rs', '.migrate()');
  contains('ops/import-cpa-session-archive.ts', 'SESSION_ARCHIVE_MAX_LINE_BYTES');
  contains('ops/import-cpa-session-archive.ts', '16 MiB importer hard limit');
});
