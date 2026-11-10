import { pathToFileURL } from 'node:url';

type ReviewOptions = {
  baseUrl: string;
  apiKey: string;
  model: string;
  task: string;
};

type ReviewData = {
  request_id?: string;
  model?: string;
  usage?: unknown;
  error?: { code?: string; type?: string; message?: string };
  choices?: { finish_reason?: string | null; message?: { content?: string }; delta?: { content?: string } }[];
};

async function collectStream(response: Response) {
  const data: ReviewData = {};
  let text = '';
  let buffer = '';
  let done = false;
  let streamError: string | undefined;
  const decoder = new TextDecoder();
  const consume = (frame: string) => {
    const payload = frame.split(/\r?\n/).filter(line => line.startsWith('data:'))
      .map(line => line.slice(5).replace(/^ /, '')).join('\n');
    if (!payload) return;
    if (payload === '[DONE]') { done = true; return; }
    try {
      const chunk = JSON.parse(payload) as ReviewData;
      if (chunk.model) data.model = chunk.model;
      if (chunk.request_id) data.request_id = chunk.request_id;
      if (chunk.usage !== undefined) data.usage = chunk.usage;
      if (chunk.error) data.error = chunk.error;
      const choice = chunk.choices?.[0];
      if (typeof choice?.delta?.content === 'string') text += choice.delta.content;
      if (choice?.finish_reason != null) data.choices = [{ finish_reason: choice.finish_reason }];
      // Provider reasoning and opaque fields are never included in the result.
    } catch { streamError = 'invalid_json'; }
  };
  const drain = () => {
    let boundary;
    while ((boundary = /\r?\n\r?\n/.exec(buffer))) {
      consume(buffer.slice(0, boundary.index));
      buffer = buffer.slice(boundary.index + boundary[0].length);
    }
  };
  try {
    if (response.body) {
      for await (const bytes of response.body) {
        buffer += decoder.decode(bytes, { stream: true });
        drain();
      }
    }
    buffer += decoder.decode();
    drain();
    if (buffer.trim()) consume(buffer);
  } catch { streamError = 'transport_read_error'; }
  return { data, text, stream_complete: done && !streamError && !data.error, stream_error: streamError };
}

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
      stream: true,
      stream_options: { include_usage: true },
    }),
  });
  const streaming = response.headers.get('content-type')?.split(';')[0]?.trim() === 'text/event-stream';
  const collected = streaming ? await collectStream(response) : undefined;
  let data: ReviewData = collected?.data ?? {};
  if (!collected) {
    const raw = await response.text();
    try { data = JSON.parse(raw) as ReviewData; }
    catch { data = { error: { type: 'non_json_response', message: raw } }; }
  }
  // Return the actual review and finish reason, including an empty or truncated
  // result. Do not substitute reasoning, opaque content or a success summary.
  return {
    status: response.status,
    request_id: response.headers.get('x-request-id') ?? data.request_id,
    model: data.model,
    usage: data.usage,
    finish_reason: data.choices?.[0]?.finish_reason,
    review: collected?.text ?? data.choices?.[0]?.message?.content,
    stream_complete: collected?.stream_complete,
    stream_error: collected?.stream_error,
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
  if (result.status < 200 || result.status >= 300 || result.error || result.stream_complete === false) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    console.error('Review request could not complete. No automatic retry was made.');
    process.exitCode = 1;
  });
}
