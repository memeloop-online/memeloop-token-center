import { pathToFileURL } from 'node:url';

type ReviewOptions = {
  baseUrl: string;
  apiKey: string;
  model: string;
  task: string;
};

/** Plain Chat requests use the normal gateway routing, archive and billing path. */
export async function review(options: ReviewOptions, send: typeof fetch = fetch) {
  const response = await send(`${options.baseUrl.replace(/\/$/, '')}/chat/completions`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${options.apiKey}`,
      'Content-Type': 'application/json',
      'X-Mtc-Client-Name': 'kimi-plaintext-review',
    },
    body: JSON.stringify({
      model: options.model,
      messages: [{ role: 'user', content: options.task }],
      stream: false,
    }),
  });
  const data = await response.json() as {
    request_id?: string;
    model?: string;
    usage?: unknown;
    error?: { code?: string; type?: string; message?: string };
    choices?: { finish_reason?: string; message?: { content?: string } }[];
  };
  // Return the actual review and finish reason, including an empty or truncated
  // result. Do not substitute reasoning, opaque content or a success summary.
  return {
    status: response.status,
    request_id: response.headers.get('x-request-id') ?? data.request_id,
    model: data.model,
    usage: data.usage,
    finish_reason: data.choices?.[0]?.finish_reason,
    review: data.choices?.[0]?.message?.content,
    error: data.error && {
      code: data.error.code,
      type: data.error.type,
      message: data.error.message?.replaceAll(options.apiKey, '[redacted]'),
    },
  };
}

async function main() {
  if (process.argv.includes('--help')) {
    console.log('Read a complete review task from stdin. Set MTC_API_KEY; optionally MTC_BASE_URL (including /v1) and MTC_REVIEW_MODEL. Outputs the actual review, usage and request ID as JSON.');
    return;
  }
  const apiKey = process.env.MTC_API_KEY;
  if (!apiKey) throw new Error('Set MTC_API_KEY to an authorized downstream API key.');
  let task = '';
  process.stdin.setEncoding('utf8');
  for await (const chunk of process.stdin) task += chunk;
  const result = await review({
    baseUrl: process.env.MTC_BASE_URL ?? 'https://token.k3s.onetwo.website/v1',
    apiKey,
    model: process.env.MTC_REVIEW_MODEL ?? 'k3.1-pro',
    task,
  });
  console.log(JSON.stringify(result, null, 2));
  if (result.status < 200 || result.status >= 300) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    console.error('Review request could not complete. No automatic retry was made.');
    process.exitCode = 1;
  });
}
