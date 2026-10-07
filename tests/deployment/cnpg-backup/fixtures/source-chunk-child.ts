import assert from 'node:assert/strict';
import { copyChunk } from '../copy.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true');
const mode = process.argv[2];
if (mode === 'complete') {
  let acknowledgement = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', value => { acknowledgement += value; });
  process.stdin.on('end', () => { process.exitCode = acknowledgement === 'MTC_COPY_CHUNK_RECEIVED\n' ? 0 : 12; });
  process.stdout.write(Buffer.alloc(copyChunk, 71));
} else if (mode === 'short') {
  process.stdout.write(Buffer.alloc(8192, 71));
} else if (mode === 'overflow') {
  process.stdout.write(Buffer.alloc(8193));
} else if (mode === 'failed') {
  process.stderr.write('private-archive-secret');
  process.exitCode = 7;
} else if (mode === 'failed-after-data') {
  process.stdin.resume();
  process.stdin.on('end', () => { process.exitCode = 9; });
  process.stdout.write(Buffer.alloc(8192));
} else if (mode === 'timeout') {
  setInterval(() => {}, 1000);
} else {
  throw new Error('Unknown source chunk fixture mode');
}
