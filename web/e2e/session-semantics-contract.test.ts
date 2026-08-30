import assert from 'node:assert/strict';
import test from 'node:test';
import { deriveSemanticExecution } from '../src/sessionSemantics.js';
import type { ConversationRequest, LogicalSessionDetail } from '../src/types.js';

function request(request_id: string, execution?: ConversationRequest['execution']): ConversationRequest {
  return {
    request_id,
    created_at: Number(request_id.slice(1)),
    protocol: 'openai',
    model: 'fixture-model',
    status_code: 200,
    duration_ms: 10,
    input_tokens: 1,
    output_tokens: 1,
    cost: '0',
    error_code: null,
    source: 'live',
    provenance: 'native',
    unlinked: false,
    execution,
  };
}

function execution(values: Partial<NonNullable<ConversationRequest['execution']>>): NonNullable<ConversationRequest['execution']> {
  return {
    session_name: null,
    trace_id: null,
    span_id: null,
    parent_span_id: null,
    agent_id: null,
    parent_agent_id: null,
    task_kind: null,
    labels: {},
    source: 'declared',
    ...values,
  };
}

test('span ancestry wins, then agent and confirmed edges fall back', () => {
  const detail = {
    session_id: 'session', cluster_id: null, unlinked: false, has_more: false,
    next_cursor: null, edges_truncated: false,
    requests: [
      request('r1', execution({ trace_id: 'trace', span_id: 'root', agent_id: 'root-agent', labels: { environment: 'prod' } })),
      request('r2', execution({ agent_id: 'child-agent', parent_agent_id: 'root-agent', labels: { environment: 'staging' } })),
      request('r3'),
      request('r4', execution({ trace_id: 'trace', span_id: 'child', parent_span_id: 'root', agent_id: 'other', parent_agent_id: 'root-agent' })),
    ],
    edges: [
      { from_request_id: 'r2', to_request_id: 'r3', relation: 'continues', confidence: 1, evidence: {} },
      { from_request_id: 'r1', to_request_id: 'r4', relation: 'continues', confidence: 1, evidence: {} },
      { from_request_id: 'r3', to_request_id: 'r1', relation: 'candidate', confidence: 0.2, evidence: {} },
    ],
  } as LogicalSessionDetail;

  const derived = deriveSemanticExecution(detail);
  const nodes = new Map(derived.nodes.map((node) => [node.requestId, node]));
  assert.deepEqual(nodes.get('r2'), { requestId: 'r2', parentRequestId: 'r1', parentSource: 'agent', depth: 1 });
  assert.deepEqual(nodes.get('r3'), { requestId: 'r3', parentRequestId: 'r2', parentSource: 'edge', depth: 2 });
  assert.deepEqual(nodes.get('r4'), { requestId: 'r4', parentRequestId: 'r1', parentSource: 'span', depth: 1 });
  assert.deepEqual(derived.labels, [{ key: 'environment', values: ['prod', 'staging'], conflict: true }]);
});

test('conflicting parents and cycles are explicitly flattened', () => {
  const detail = {
    session_id: 'session', cluster_id: null, unlinked: false, has_more: false,
    next_cursor: null, edges_truncated: false,
    requests: [
      request('r1', execution({ trace_id: 'trace', span_id: 'span-a', agent_id: 'agent-a' })),
      request('r2', execution({ agent_id: 'agent-b' })),
      request('r3', execution({ trace_id: 'trace', parent_span_id: 'span-a', parent_agent_id: 'agent-b' })),
      request('r4'), request('r5'),
    ],
    edges: [
      { from_request_id: 'r5', to_request_id: 'r4', relation: 'continues', confidence: 1, evidence: {} },
      { from_request_id: 'r4', to_request_id: 'r5', relation: 'continues', confidence: 1, evidence: {} },
    ],
  } as LogicalSessionDetail;

  const nodes = new Map(deriveSemanticExecution(detail).nodes.map((node) => [node.requestId, node]));
  assert.equal(nodes.get('r3')?.parentSource, 'conflict');
  assert.equal(nodes.get('r3')?.parentRequestId, null);
  assert.equal(nodes.get('r4')?.parentSource, 'cycle');
  assert.equal(nodes.get('r5')?.parentSource, 'cycle');
  assert.equal(nodes.get('r4')?.depth, 0);
  assert.equal(nodes.get('r5')?.depth, 0);
});
