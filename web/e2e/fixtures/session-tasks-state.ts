import type { SessionIdentity, SessionTaskScope } from '../../src/types.js';

export interface TaskFixtureCall {
  scope: SessionTaskScope;
  identities: SessionIdentity[];
  timezone: string;
  include_subtrees: boolean;
}

declare global {
  interface Window {
    analyzeCalls: TaskFixtureCall[];
    namePuts: Array<{ title: string; source: string; expected_revision: number }>;
    relationshipWrites: Array<{ method: string; kind: string }>;
    forceNameConflict: boolean;
    lastTitle: string;
    relatedActive: boolean;
    holdAnalysis: boolean;
    releaseAnalysis: () => void;
  }
}
