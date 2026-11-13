import { createInterface } from 'node:readline';

// Static JSON Lines protocol peer for the subprocess transport regression.
// It does not invoke Codex or any provider.
const lines = createInterface({ input: process.stdin });
lines.on('line', line => {
  const message = JSON.parse(line);
  if (message.method === 'initialize') {
    process.stdout.write(JSON.stringify({ id: message.id, result: { ok: true } }) + '\n');
  }
  if (message.method === 'ask') {
    process.stdout.write(JSON.stringify({ id: 99, method: 'item/tool/call', params: { task: 'plain' } }) + '\n');
  }
  if (message.id === 99 && message.result) {
    process.stdout.write(JSON.stringify({ method: 'peer/done', params: message.result }) + '\n');
  }
});
