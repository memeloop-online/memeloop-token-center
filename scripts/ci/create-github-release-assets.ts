import { createHash } from 'node:crypto';
import {
  chmodSync, constants, copyFileSync, cpSync, lstatSync, mkdirSync, mkdtempSync,
  readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fail, parseObject, requireCanonicalDirectory, requireRevision } from './release-evidence.ts';

const SCOPE = 'GitHub release asset creation';
const [inputValue = '', evidenceValue = '', manifestValue = '', outputValue = '', revisionValue = '', version = ''] = process.argv.slice(2);
if ([inputValue, evidenceValue, manifestValue, outputValue, revisionValue, version].some((value) => value === '')) {
  fail(SCOPE, 'release input, evidence, manifest, output, revision, and version are required');
}
if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) fail(SCOPE, 'version is invalid');
const revision = requireRevision(revisionValue, SCOPE);
const input = requireCanonicalDirectory(resolve(inputValue), SCOPE, 'release input directory');
const evidence = requireCanonicalDirectory(resolve(evidenceValue), SCOPE, 'release evidence directory');
const manifestPath = resolve(manifestValue);
const manifestMetadata = lstatSync(manifestPath);
if (!manifestMetadata.isFile() || manifestMetadata.isSymbolicLink() || realpathSync(manifestPath) !== manifestPath) {
  fail(SCOPE, 'GHCR release manifest must be a canonical regular file');
}
let releaseManifest: unknown;
try { releaseManifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as unknown; } catch { fail(SCOPE, 'GHCR release manifest is invalid JSON'); }
if (!Array.isArray(releaseManifest) || releaseManifest.length !== 2 || releaseManifest.some((entry) => {
  if (entry === null || Array.isArray(entry) || typeof entry !== 'object') return true;
  const record = entry as Record<string, unknown>;
  return record.revision !== revision || typeof record.reference !== 'string';
})) fail(SCOPE, 'GHCR release manifest identity is invalid');
const output = resolve(outputValue);
mkdirSync(output, { recursive: false, mode: 0o700 });

const copy = (source: string, name: string, mode = 0o644): void => {
  const path = resolve(source);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(path) !== path || statSync(path).size === 0) {
    fail(SCOPE, `${basename(source)} must be a canonical non-empty regular file`);
  }
  const destination = join(output, name);
  copyFileSync(path, destination, constants.COPYFILE_EXCL);
  chmodSync(destination, mode);
};

copy(join(input, 'memeloop-token-center'), 'memeloop-token-center-linux-amd64', 0o755);
copy(join(input, 'install-plugin-oci'), 'install-plugin-oci-linux-amd64', 0o755);
copy(join(input, 'release-service-input.json'), 'release-input-linux-amd64.json');
copy(manifestPath, 'ghcr-release-manifest.json');

const statement = (scope: string, predicate: string, destination: string): void => {
  const directory = requireCanonicalDirectory(join(evidence, `${scope}-attestations`), SCOPE, `${scope} attestation directory`);
  const matches = readdirSync(directory).filter((name) => name.startsWith('in-toto-') && name.endsWith('.json')).filter((name) => {
    const payload = parseObject(readFileSync(join(directory, name), 'utf8'), SCOPE, `${scope} statement`);
    return payload.predicateType === predicate;
  });
  if (matches.length !== 1) fail(SCOPE, `${scope} must contain exactly one ${predicate} statement`);
  copy(join(directory, matches[0]!), destination);
};
for (const [scope, name] of [['service', 'memeloop-token-center'], ['plugin-installer', 'memeloop-token-center-plugin-installer']] as const) {
  statement(scope, 'https://spdx.dev/Document', `${name}-image-linux-amd64.spdx.json`);
  const directory = requireCanonicalDirectory(join(evidence, `${scope}-attestations`), SCOPE, `${scope} attestation directory`);
  const slsa = readdirSync(directory).filter((entry) => entry.startsWith('in-toto-') && entry.endsWith('.json')).filter((entry) => {
    const payload = parseObject(readFileSync(join(directory, entry), 'utf8'), SCOPE, `${scope} statement`);
    return payload.predicateType === 'https://slsa.dev/provenance/v1' || payload.predicateType === 'https://slsa.dev/provenance/v0.2';
  });
  if (slsa.length !== 1) fail(SCOPE, `${scope} must contain exactly one SLSA provenance statement`);
  copy(join(directory, slsa[0]!), `${name}-image-linux-amd64.provenance.json`);
}

const staging = mkdtempSync(join(tmpdir(), 'mtc-release-assets-'));
try {
  const service = join(staging, `memeloop-token-center-${version}-linux-amd64`);
  mkdirSync(join(service, 'bin'), { recursive: true });
  mkdirSync(join(service, 'lib'), { recursive: true });
  mkdirSync(join(service, 'share'), { recursive: true });
  copyFileSync(join(input, 'memeloop-token-center'), join(service, 'bin/memeloop-token-center.bin'));
  copyFileSync(join(input, 'install-plugin-oci'), join(service, 'bin/install-plugin-oci'));
  copyFileSync(join(input, 'cosign'), join(service, 'bin/cosign'));
  for (const name of ['memeloop-token-center.bin', 'install-plugin-oci', 'cosign']) chmodSync(join(service, 'bin', name), 0o755);
  copyFileSync(join(input, 'libgcc_s.so.1'), join(service, 'lib/libgcc_s.so.1'));
  copyFileSync(join(input, 'libstdc++.so.6'), join(service, 'lib/libstdc++.so.6'));
  cpSync(join(input, 'web'), join(service, 'share/web'), { recursive: true, errorOnExist: true });
  copyFileSync(join(input, 'LICENSE'), join(service, 'LICENSE'));
  copyFileSync(join(input, 'THIRD_PARTY_NOTICES.md'), join(service, 'THIRD_PARTY_NOTICES.md'));
  cpSync(join(input, 'third-party-licenses'), join(service, 'third-party-licenses'), { recursive: true, errorOnExist: true });
  writeFileSync(join(service, 'bin/memeloop-token-center'), [
    '#!/bin/sh',
    'set -eu',
    'bin_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)',
    'root_dir=$(CDPATH= cd -- "$bin_dir/.." && pwd)',
    '[ -f "$root_dir/share/web/index.html" ] || { echo "bundled web root is missing" >&2; exit 1; }',
    'export LD_LIBRARY_PATH="$root_dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"',
    'export GLIBC_TUNABLES="${GLIBC_TUNABLES:-glibc.malloc.mmap_threshold=65536}"',
    'export MTC_WEB_ROOT="${MTC_WEB_ROOT:-$root_dir/share/web}"',
    'export PATH="$bin_dir:$PATH"',
    'exec "$bin_dir/memeloop-token-center.bin" "$@"',
    '',
  ].join('\n'), { mode: 0o755, flag: 'wx' });
  writeFileSync(join(service, 'README.txt'), [
    'MemeLoop Token Center complete Linux amd64 runtime',
    '',
    'Run: ./bin/memeloop-token-center --help',
    'The launcher resolves the bundled shared libraries and web root relative to this extracted directory.',
    'The .bin file is the exact ELF also published as a raw release asset; invoke the launcher for a portable extracted-package layout.',
    '',
  ].join('\n'), { mode: 0o644, flag: 'wx' });

  const installer = join(staging, `install-plugin-oci-${version}-linux-amd64`);
  mkdirSync(join(installer, 'bin'), { recursive: true });
  mkdirSync(join(installer, 'lib'), { recursive: true });
  copyFileSync(join(input, 'install-plugin-oci'), join(installer, 'bin/install-plugin-oci.bin'));
  copyFileSync(join(input, 'cosign'), join(installer, 'bin/cosign'));
  chmodSync(join(installer, 'bin/install-plugin-oci.bin'), 0o755);
  chmodSync(join(installer, 'bin/cosign'), 0o755);
  copyFileSync(join(input, 'libgcc_s.so.1'), join(installer, 'lib/libgcc_s.so.1'));
  copyFileSync(join(input, 'LICENSE'), join(installer, 'LICENSE'));
  copyFileSync(join(input, 'THIRD_PARTY_NOTICES.md'), join(installer, 'THIRD_PARTY_NOTICES.md'));
  cpSync(join(input, 'third-party-licenses'), join(installer, 'third-party-licenses'), { recursive: true, errorOnExist: true });
  writeFileSync(join(installer, 'bin/install-plugin-oci'), [
    '#!/bin/sh',
    'set -eu',
    'bin_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)',
    'root_dir=$(CDPATH= cd -- "$bin_dir/.." && pwd)',
    '[ -x "$bin_dir/cosign" ] || { echo "bundled cosign is missing" >&2; exit 1; }',
    'export LD_LIBRARY_PATH="$root_dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"',
    'export PATH="$bin_dir:$PATH"',
    'exec "$bin_dir/install-plugin-oci.bin" "$@"',
    '',
  ].join('\n'), { mode: 0o755, flag: 'wx' });
  writeFileSync(join(installer, 'README.txt'), [
    'MemeLoop Token Center OCI plugin installer for Linux amd64',
    '',
    'Run: ./bin/install-plugin-oci --help',
    'The launcher resolves the bundled shared library and pinned Cosign companion relative to this extracted directory.',
    'The .bin file is the exact ELF also published as a raw release asset; invoke the launcher for a portable extracted-package layout.',
    '',
  ].join('\n'), { mode: 0o644, flag: 'wx' });

  for (const directory of [service, installer]) {
    const archive = join(output, `${basename(directory)}.tar.gz`);
    const result = spawnSync('tar', [
      '--sort=name', '--mtime=@0', '--owner=0', '--group=0', '--numeric-owner',
      '-czf', archive, '-C', staging, basename(directory),
    ], { encoding: 'utf8', shell: false });
    if (result.status !== 0) {
      const detail = result.error?.message ?? (result.stderr.trim() || `tar exited with status ${String(result.status)}`);
      fail(SCOPE, `unable to package ${basename(directory)}: ${detail}`);
    }
  }
} finally {
  rmSync(staging, { recursive: true, force: true });
}

const assets = readdirSync(output).sort((left, right) => left.localeCompare(right, 'en'));
const sums = assets.map((name) => {
  const digest = createHash('sha256').update(readFileSync(join(output, name))).digest('hex');
  return `${digest}  ${name}`;
});
writeFileSync(join(output, 'SHA256SUMS'), `${sums.join('\n')}\n`, { encoding: 'utf8', flag: 'wx', mode: 0o644 });
writeFileSync(join(output, 'RELEASE.md'), [
  `Automated immutable release for MemeLoop Token Center ${version}.`,
  '',
  `Source revision: \`${revision}\``,
  '',
  'The raw Linux ELF executables are exact build outputs intended for packaging and inspection; they still require their normal dynamic libraries and runtime paths. The complete runtime archives include launchers that configure those paths, and were produced from the same sealed CI build input used to assemble the GHCR images. The service archive includes the web bundle, plugin installer, pinned Cosign companion, and required GCC/C++ runtime libraries; the plugin-installer archive includes Cosign and its runtime library.',
  '',
  'This release is licensed under Apache License 2.0. Third-party notices and licenses are preserved in the complete archives and represented in the attached SPDX/SLSA evidence.',
].join('\n'), { encoding: 'utf8', flag: 'wx', mode: 0o644 });
console.log(`Created ${assets.length} checksummed release assets plus SHA256SUMS for ${revision}`);
