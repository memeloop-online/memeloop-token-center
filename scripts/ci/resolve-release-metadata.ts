import { appendFileSync, readFileSync } from 'node:fs';

const SCOPE = 'release metadata resolution';
const [ref = '', output = ''] = process.argv.slice(2);
if (output === '') throw new Error(`${SCOPE}: GitHub output path is required`);
const manifest = readFileSync('Cargo.toml', 'utf8');
const packageStart = manifest.search(/^\[package\]\s*$/mu);
if (packageStart < 0) throw new Error(`${SCOPE}: Cargo package section is missing`);
const packageTail = manifest.slice(packageStart + '[package]'.length);
const nextSection = packageTail.search(/^\[/mu);
const packageBlock = nextSection < 0 ? packageTail : packageTail.slice(0, nextSection);
const version = packageBlock.match(/^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)"\s*$/mu)?.[1];
if (version === undefined) throw new Error(`${SCOPE}: Cargo package version is missing or invalid`);
let versionTag = '';
if (ref.startsWith('refs/tags/')) {
  versionTag = ref.slice('refs/tags/'.length);
  if (versionTag !== `v${version}`) {
    throw new Error(`${SCOPE}: version tag ${JSON.stringify(versionTag)} must exactly equal v${version}`);
  }
} else if (!ref.startsWith('refs/heads/') && !ref.startsWith('refs/pull/')) {
  throw new Error(`${SCOPE}: unsupported GitHub ref ${JSON.stringify(ref)}`);
}
appendFileSync(output, `version=${version}\nversion_tag=${versionTag}\nis_version_tag=${String(versionTag !== '')}\n`, 'utf8');
console.log(JSON.stringify({ version, version_tag: versionTag, is_version_tag: versionTag !== '' }));
