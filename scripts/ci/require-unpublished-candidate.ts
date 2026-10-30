import { spawnSync } from 'node:child_process';
const [image = '', revision = ''] = process.argv.slice(2);
if (!/^ghcr\.io\/memeloop-online\/memeloop-token-center(?:-plugin-installer)?$/.test(image) || !/^[0-9a-f]{40}$/.test(revision)) {
  throw new Error('candidate tag preflight: invalid image identity');
}
const result = spawnSync('crane', ['manifest', `${image}:sha-${revision}`], { encoding: 'utf8', shell: false });
if (result.status === 0) throw new Error('candidate tag preflight: immutable tag already exists; refusing overwrite');
// A registry authorization/network/server failure must not be interpreted as
// proof of absence. Only an explicit registry MANIFEST_UNKNOWN is acceptable.
if (!/MANIFEST_UNKNOWN|manifest unknown/.test(result.stderr)) {
  throw new Error('candidate tag preflight: registry did not prove the tag absent');
}
console.log('Registry confirmed that the immutable candidate tag is absent');
