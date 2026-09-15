import type { RequestDetail } from './types.js';

/** P1: A bounded, display-neutral reconstruction of archived conversation turns. */

export const SESSION_REPLAY_PROJECTION_PRIORITY = 'P1';
export const SESSION_REPLAY_MAX_REQUESTS = 100;
export const SESSION_REPLAY_MAX_ITEMS = 300;
type ProjectionLimit = { truncated: boolean; itemOffset: number; seen: number; unknownKeys: Set<string> };

export type ReplayBody = 'request' | 'response';
export type ReplayUnknownReason =
  | 'archive_unavailable'
  | 'redacted'
  | 'unsupported_body'
  | 'missing_text'
  | 'outside_session'
  | 'input_limit';

export interface ReplayMessage {
  kind: 'message';
  requestId: string;
  body: ReplayBody;
  role: 'user' | 'assistant' | 'agent';
  /** Null means the archive named the role but did not retain readable text. */
  text: string | null;
  unknown: Extract<ReplayUnknownReason, 'redacted' | 'missing_text'> | null;
  truncated: boolean;
}

export interface ReplayToolCall {
  kind: 'tool_call';
  requestId: string;
  body: ReplayBody;
  callId: string | null;
  name: string | null;
  arguments: string | null;
  unknownFields: Array<'call_id' | 'name' | 'arguments'>;
  pairedResultRequestId: string | null;
  pairing: 'paired' | 'unpaired' | 'unknown';
  truncated: boolean;
}

export interface ReplayToolResult {
  kind: 'tool_result';
  requestId: string;
  body: ReplayBody;
  callId: string | null;
  name: string | null;
  output: string | null;
  unknownFields: Array<'call_id' | 'output'>;
  pairedCallRequestId: string | null;
  pairing: 'paired' | 'unpaired' | 'unknown';
  truncated: boolean;
}

export interface ReplayUnknown {
  kind: 'unknown';
  requestId: string;
  body: ReplayBody;
  reason: ReplayUnknownReason;
}

export type SessionReplayItem = ReplayMessage | ReplayToolCall | ReplayToolResult | ReplayUnknown;

export interface SessionReplayProjection {
  sessionId: string;
  items: SessionReplayItem[];
  /** True when this projection intentionally omitted retained data at a safe UI bound. */
  truncated: boolean;
  totalItems: number;
  nextItemOffset: number | null;
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as JsonObject
    : undefined;
}

function array(value: unknown): unknown[] | undefined {
  return Array.isArray(value) ? value : undefined;
}

function string(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function redacted(value: unknown): boolean {
  const record = object(value);
  return record?.redacted === true || record?.archive_redacted === true || record?.status === 'redacted';
}

function clipped(value: string): { value: string; truncated: boolean } {
  // The API already bounds archived bodies. Long retained text is expanded
  // on demand by ArchiveText instead of being irreversibly sliced here.
  return { value, truncated: false };
}

function textFromContent(value: unknown): { text: string | null; unknown: ReplayMessage['unknown']; truncated: boolean } {
  if (redacted(value)) return { text: null, unknown: 'redacted', truncated: false };
  const direct = string(value);
  if (direct !== undefined) {
    const clippedValue = clipped(direct);
    return { text: clippedValue.value, unknown: null, truncated: clippedValue.truncated };
  }
  const parts = array(value);
  if (!parts) return { text: null, unknown: 'missing_text', truncated: false };

  let text = '';
  let foundText = false;
  const truncated = false;
  for (const part of parts) {
    if (redacted(part)) return { text: null, unknown: 'redacted', truncated };
    const record = object(part);
    if (!record) continue;
    const candidate = string(record.text) ?? string(record.input_text) ?? string(record.output_text);
    if (candidate === undefined) continue;
    foundText = true;
    text += candidate;
  }
  if (!foundText) return { text: null, unknown: 'missing_text', truncated };
  return { text, unknown: null, truncated };
}

function push(items: SessionReplayItem[], item: SessionReplayItem, limit: ProjectionLimit): boolean {
  const position = limit.seen++;
  if (position < limit.itemOffset || items.length >= SESSION_REPLAY_MAX_ITEMS) return true;
  items.push(item);
  return true;
}

function unknown(items: SessionReplayItem[], requestId: string, body: ReplayBody, reason: ReplayUnknownReason, limit: ProjectionLimit) {
  // A body may contain hundreds of opaque reasoning/compaction records. One
  // honest gap per reason is enough; repeated warnings must not crowd out text.
  const key = JSON.stringify([requestId, body, reason]);
  if (limit.unknownKeys.has(key)) return;
  limit.unknownKeys.add(key);
  push(items, { kind: 'unknown', requestId, body, reason }, limit);
}

function message(items: SessionReplayItem[], requestId: string, body: ReplayBody, role: ReplayMessage['role'], content: unknown, limit: ProjectionLimit) {
  const extracted = textFromContent(content);
  if (extracted.truncated) limit.truncated = true;
  push(items, {
    kind: 'message', requestId, body, role,
    text: extracted.text, unknown: extracted.unknown, truncated: extracted.truncated,
  }, limit);
}

function toolCall(items: SessionReplayItem[], requestId: string, body: ReplayBody, value: unknown, limit: ProjectionLimit) {
  const record = object(value);
  if (!record || redacted(record)) {
    unknown(items, requestId, body, redacted(record) ? 'redacted' : 'unsupported_body', limit);
    return;
  }
  const functionValue = object(record.function) ?? record;
  const callId = string(record.call_id) ?? string(record.id);
  const name = string(functionValue.name);
  const rawArguments = string(functionValue.arguments);
  const argumentsValue = rawArguments === undefined ? undefined : clipped(rawArguments);
  if (argumentsValue?.truncated) limit.truncated = true;
  const unknownFields: ReplayToolCall['unknownFields'] = [];
  if (callId === undefined) unknownFields.push('call_id');
  if (name === undefined) unknownFields.push('name');
  if (argumentsValue === undefined) unknownFields.push('arguments');
  push(items, {
    kind: 'tool_call', requestId, body,
    callId: callId ?? null, name: name ?? null, arguments: argumentsValue?.value ?? null,
    unknownFields, pairedResultRequestId: null,
    pairing: callId === undefined ? 'unknown' : 'unpaired', truncated: argumentsValue?.truncated ?? false,
  }, limit);
}

function toolResult(items: SessionReplayItem[], requestId: string, body: ReplayBody, value: unknown, limit: ProjectionLimit) {
  const record = object(value);
  if (!record || redacted(record)) {
    unknown(items, requestId, body, redacted(record) ? 'redacted' : 'unsupported_body', limit);
    return;
  }
  const callId = string(record.call_id) ?? string(record.tool_call_id);
  const name = string(record.name);
  const source = record.output ?? record.content;
  const extracted = textFromContent(source);
  if (extracted.truncated) limit.truncated = true;
  const unknownFields: ReplayToolResult['unknownFields'] = [];
  if (callId === undefined) unknownFields.push('call_id');
  if (extracted.text === null) unknownFields.push('output');
  push(items, {
    kind: 'tool_result', requestId, body,
    callId: callId ?? null, name: name ?? null, output: extracted.text,
    unknownFields, pairedCallRequestId: null,
    pairing: callId === undefined ? 'unknown' : 'unpaired', truncated: extracted.truncated,
  }, limit);
}

function chatMessage(items: SessionReplayItem[], requestId: string, body: ReplayBody, value: unknown, limit: ProjectionLimit) {
  const record = object(value);
  if (!record || redacted(record)) {
    unknown(items, requestId, body, redacted(record) ? 'redacted' : 'unsupported_body', limit);
    return;
  }
  const role = string(record.role);
  // Instructions are valid protocol input, not conversation turns.
  if (role === 'system' || role === 'developer') return;
  if (role === 'user' || role === 'assistant') message(items, requestId, body, role, record.content, limit);
  const calls = array(record.tool_calls);
  if (calls) {
    for (const call of calls) toolCall(items, requestId, body, call, limit);
  }
  if (record.function_call !== undefined) toolCall(items, requestId, body, record.function_call, limit);
  if (role === 'tool' || role === 'function') toolResult(items, requestId, body, record, limit);
  if (role !== 'user' && role !== 'assistant' && role !== 'tool' && role !== 'function') {
    unknown(items, requestId, body, 'unsupported_body', limit);
  }
}

function responsesItem(items: SessionReplayItem[], requestId: string, body: ReplayBody, value: unknown, limit: ProjectionLimit) {
  const record = object(value);
  if (!record || redacted(record)) {
    unknown(items, requestId, body, redacted(record) ? 'redacted' : 'unsupported_body', limit);
    return;
  }
  const type = string(record.type);
  if (type === 'additional_tools' || record.role === 'system' || record.role === 'developer') return;
  if (type === 'agent_message') { message(items, requestId, body, 'agent', record.content, limit); return; }
  if (type === 'custom_tool_call') { toolCall(items, requestId, body, { ...record, arguments: record.input }, limit); return; }
  if (type === 'custom_tool_call_output') { toolResult(items, requestId, body, record, limit); return; }
  if (type === 'function_call') { toolCall(items, requestId, body, record, limit); return; }
  if (type === 'function_call_output') { toolResult(items, requestId, body, record, limit); return; }
  if (type === 'message') {
    const role = string(record.role);
    if (role === 'user' || role === 'assistant') message(items, requestId, body, role, record.content, limit);
    else unknown(items, requestId, body, 'unsupported_body', limit);
    return;
  }
  if (string(record.role) === 'user' || string(record.role) === 'assistant') {
    message(items, requestId, body, string(record.role) as 'user' | 'assistant', record.content, limit);
    return;
  }
  unknown(items, requestId, body, 'unsupported_body', limit);
}

function responsesRequest(items: SessionReplayItem[], requestId: string, body: ReplayBody, input: unknown, limit: ProjectionLimit) {
  if (typeof input === 'string') { message(items, requestId, body, 'user', input, limit); return; }
  const entries = array(input);
  if (!entries) { unknown(items, requestId, body, 'unsupported_body', limit); return; }
  for (const entry of entries) {
    if (typeof entry === 'string') message(items, requestId, body, 'user', entry, limit);
    else responsesItem(items, requestId, body, entry, limit);
  }
}

function responsesResponse(items: SessionReplayItem[], requestId: string, body: ReplayBody, output: unknown, limit: ProjectionLimit) {
  const entries = array(output);
  if (!entries) { unknown(items, requestId, body, 'unsupported_body', limit); return; }
  for (const entry of entries) responsesItem(items, requestId, body, entry, limit);
}

function projectRequestBody(items: SessionReplayItem[], detail: RequestDetail, limit: ProjectionLimit) {
  const body = detail.request_body;
  if (body === null || body === undefined) {
    unknown(items, detail.request_id, 'request', 'archive_unavailable', limit);
    return;
  }
  if (redacted(body)) { unknown(items, detail.request_id, 'request', 'redacted', limit); return; }
  const record = object(body);
  if (!record) { unknown(items, detail.request_id, 'request', 'unsupported_body', limit); return; }
  const messages = array(record.messages);
  if (messages) {
    for (const value of messages) chatMessage(items, detail.request_id, 'request', value, limit);
    return;
  }
  if ('input' in record) { responsesRequest(items, detail.request_id, 'request', record.input, limit); return; }
  unknown(items, detail.request_id, 'request', 'unsupported_body', limit);
}

/** Archive storage retains streaming responses as SSE text, not a JSON response.
 * Read every returned event; the archive API owns body-size validation. */
function archivedSseResponse(value: unknown): JsonObject | undefined {
  if (typeof value !== 'string' || !/^data:/m.test(value)) return undefined;
  const completedItems = new Map<number, JsonObject>();
  let terminal: JsonObject | undefined;
  const blocks = value.replaceAll('\r\n', '\n').split('\n\n');
  for (const block of blocks) {
    const data = block.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
    if (!data || data === '[DONE]') continue;
    let event: JsonObject | undefined;
    try { event = object(JSON.parse(data)); } catch { continue; }
    if (!event) continue;
    if (event.type === 'response.output_item.done' && Number.isInteger(event.output_index)
      && (event.output_index as number) >= 0 && object(event.item)) {
      completedItems.set(event.output_index as number, object(event.item)!);
    }
    if (event.type === 'response.completed' || event.type === 'response.incomplete' || event.type === 'response.failed') {
      terminal = object(event.response);
      break;
    }
  }
  if (array(terminal?.output)?.length) return terminal;
  if (completedItems.size) return { ...terminal, output: [...completedItems].sort(([a], [b]) => a - b).map(([, item]) => item), replay_partial: !terminal };
  return terminal;
}

function projectResponseBody(items: SessionReplayItem[], detail: RequestDetail, limit: ProjectionLimit) {
  const body = archivedSseResponse(detail.response_body) ?? detail.response_body;
  if (body === null || body === undefined) {
    unknown(items, detail.request_id, 'response', 'archive_unavailable', limit);
    return;
  }
  if (redacted(body)) { unknown(items, detail.request_id, 'response', 'redacted', limit); return; }
  const direct = object(body);
  const record = direct && !array(direct.choices) && !array(direct.output) ? object(direct.response) ?? direct : direct;
  if (!record) { unknown(items, detail.request_id, 'response', 'unsupported_body', limit); return; }
  const choices = array(record.choices);
  if (choices) {
    for (const choice of choices) chatMessage(items, detail.request_id, 'response', object(choice)?.message, limit);
    return;
  }
  if ('output' in record) {
    responsesResponse(items, detail.request_id, 'response', record.output, limit);
    if (record.replay_partial) unknown(items, detail.request_id, 'response', 'archive_unavailable', limit);
    return;
  }
  const outputText = string(record.output_text);
  if (outputText !== undefined) { message(items, detail.request_id, 'response', 'assistant', outputText, limit); return; }
  unknown(items, detail.request_id, 'response', 'unsupported_body', limit);
}

function pairToolCalls(items: SessionReplayItem[]) {
  const calls = new Map<string, ReplayToolCall[]>();
  const results = new Map<string, ReplayToolResult[]>();
  for (const item of items) {
    if (item.kind === 'tool_call' && item.callId) calls.set(item.callId, [...(calls.get(item.callId) ?? []), item]);
    if (item.kind === 'tool_result' && item.callId) results.set(item.callId, [...(results.get(item.callId) ?? []), item]);
  }
  for (const [callId, callEntries] of calls) {
    const resultEntries = results.get(callId) ?? [];
    if (callEntries.length !== 1 || resultEntries.length !== 1) {
      if (callEntries.length > 1 || resultEntries.length > 1) {
        for (const call of callEntries) call.pairing = 'unknown';
        for (const result of resultEntries) result.pairing = 'unknown';
      }
      continue;
    }
    const [call] = callEntries;
    const [result] = resultEntries;
    call.pairedResultRequestId = result.requestId;
    call.pairing = 'paired';
    result.pairedCallRequestId = call.requestId;
    result.pairing = 'paired';
  }
}

/**
 * Projects only details explicitly belonging to `sessionId`. It never guesses
 * a session, pairs a tool result by name/order, parses HTML, or evaluates data.
 */
export function projectSessionReplay(sessionId: string, details: readonly RequestDetail[], itemOffset = 0): SessionReplayProjection {
  const items: SessionReplayItem[] = [];
  const limit: ProjectionLimit = { truncated: details.length > SESSION_REPLAY_MAX_REQUESTS, itemOffset: Math.max(0, itemOffset), seen: 0, unknownKeys: new Set() };
  const requests = details.slice(0, SESSION_REPLAY_MAX_REQUESTS)
    .sort((left, right) => left.created_at - right.created_at || left.request_id.localeCompare(right.request_id));
  let previousHistory: string[] = [];
  let previousFormat = '';

  for (const detail of requests) {
    if (detail.session_context?.session_id !== sessionId) {
      unknown(items, detail.request_id, 'request', 'outside_session', limit);
      unknown(items, detail.request_id, 'response', 'outside_session', limit);
      previousHistory = [];
      continue;
    }
    const body = object(detail.request_body);
    const format = array(body?.input) ? 'input' : array(body?.messages) ? 'messages' : '';
    const history = format ? array(body?.[format]) ?? [] : [];
    const identities = history.map(item => JSON.stringify(item));
    let repeated = 0;
    // Only an exact ordered prefix is carried history. Never globally dedupe
    // equal text, which would erase a user's deliberate repeated turn.
    if (format && format === previousFormat) {
      while (repeated < identities.length && repeated < previousHistory.length && identities[repeated] === previousHistory[repeated]) repeated += 1;
    }
    projectRequestBody(items, repeated && body ? { ...detail, request_body: { ...body, [format]: history.slice(repeated) } } : detail, limit);
    const response = archivedSseResponse(detail.response_body) ?? object(detail.response_body);
    projectResponseBody(items, response ? { ...detail, response_body: response } : detail, limit);
    const output = format === 'input' ? array(response?.output) : array(response?.choices)?.flatMap(choice => object(choice)?.message ? [object(choice)?.message] : []);
    previousHistory = [...identities, ...(output ?? []).map(item => JSON.stringify(item))];
    previousFormat = format;
  }
  if (details.length > SESSION_REPLAY_MAX_REQUESTS) {
    unknown(items, '', 'request', 'input_limit', limit);
  }
  pairToolCalls(items);
  return { sessionId, items, truncated: limit.truncated, totalItems: limit.seen, nextItemOffset: limit.seen > limit.itemOffset + items.length ? limit.itemOffset + items.length : null };
}
