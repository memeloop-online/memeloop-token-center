import type { LogicalSessionDetail } from './types.js';

export type SemanticParentSource = 'span' | 'agent' | 'edge' | 'none' | 'conflict' | 'cycle';

export interface SemanticNode {
  requestId: string;
  parentRequestId: string | null;
  parentSource: SemanticParentSource;
  depth: number;
}

export interface SemanticLabel {
  key: string;
  values: string[];
  conflict: boolean;
}

export interface SemanticExecution {
  nodes: SemanticNode[];
  labels: SemanticLabel[];
}

function unique(values: Array<string | null | undefined>) {
  return [...new Set(values.filter((value): value is string => Boolean(value)))];
}

export function deriveSemanticExecution(detail: LogicalSessionDetail): SemanticExecution {
  const requests = [...detail.requests]
    .sort((left, right) => left.created_at - right.created_at || left.request_id.localeCompare(right.request_id));
  const requestIds = new Set(requests.map((request) => request.request_id));
  const spanOwners = new Map<string, string[]>();
  const agentOwners = new Map<string, string[]>();
  const edgeParents = new Map<string, string[]>();
  const labelValues = new Map<string, Set<string>>();

  for (const request of requests) {
    const execution = request.execution;
    if (execution?.trace_id && execution.span_id) {
      const key = `${execution.trace_id}\0${execution.span_id}`;
      spanOwners.set(key, [...(spanOwners.get(key) ?? []), request.request_id]);
    }
    if (execution?.agent_id) {
      agentOwners.set(execution.agent_id, [...(agentOwners.get(execution.agent_id) ?? []), request.request_id]);
    }
    for (const [key, value] of Object.entries(execution?.labels ?? {})) {
      const values = labelValues.get(key) ?? new Set<string>();
      values.add(value);
      labelValues.set(key, values);
    }
  }
  for (const edge of detail.edges) {
    if (edge.relation === 'candidate' || !edge.from_request_id || !requestIds.has(edge.from_request_id)) continue;
    edgeParents.set(edge.to_request_id, [...(edgeParents.get(edge.to_request_id) ?? []), edge.from_request_id]);
  }

  const nodes = requests.map((request): SemanticNode => {
    const execution = request.execution;
    const span = execution?.trace_id && execution.parent_span_id
      ? spanOwners.get(`${execution.trace_id}\0${execution.parent_span_id}`) ?? []
      : [];
    const agent = execution?.parent_agent_id ? agentOwners.get(execution.parent_agent_id) ?? [] : [];
    const edge = edgeParents.get(request.request_id) ?? [];
    const sources = [
      ['span', unique(span)] as const,
      ['agent', unique(agent)] as const,
      ['edge', unique(edge)] as const,
    ].filter(([, candidates]) => candidates.length > 0);
    if (!sources.length) return { requestId: request.request_id, parentRequestId: null, parentSource: 'none', depth: 0 };
    const [preferredSource, preferred] = sources[0];
    const conflicting = preferred.length !== 1
      || preferred[0] === request.request_id
      || sources.slice(1).some(([, candidates]) => candidates.length !== 1 || candidates[0] !== preferred[0]);
    if (conflicting) return { requestId: request.request_id, parentRequestId: null, parentSource: 'conflict', depth: 0 };
    return { requestId: request.request_id, parentRequestId: preferred[0], parentSource: preferredSource, depth: 0 };
  });

  const byId = new Map(nodes.map((node) => [node.requestId, node]));
  for (const start of nodes) {
    const path: SemanticNode[] = [];
    const positions = new Map<string, number>();
    let current: SemanticNode | undefined = start;
    while (current?.parentRequestId) {
      const repeatedAt = positions.get(current.requestId);
      if (repeatedAt !== undefined) {
        for (const cyclic of path.slice(repeatedAt)) {
          cyclic.parentRequestId = null;
          cyclic.parentSource = 'cycle';
          cyclic.depth = 0;
        }
        break;
      }
      positions.set(current.requestId, path.length);
      path.push(current);
      current = byId.get(current.parentRequestId);
    }
  }
  for (const node of nodes) {
    if (node.parentSource === 'conflict' || node.parentSource === 'cycle') continue;
    let depth = 0;
    let parentId = node.parentRequestId;
    const seen = new Set([node.requestId]);
    while (parentId && !seen.has(parentId) && depth < 12) {
      seen.add(parentId);
      depth += 1;
      parentId = byId.get(parentId)?.parentRequestId ?? null;
    }
    node.depth = depth;
  }

  return {
    nodes,
    labels: [...labelValues]
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, values]) => {
        const stable = [...values].sort();
        return { key, values: stable, conflict: stable.length > 1 };
      }),
  };
}
