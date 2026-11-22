/** UI identity is an occurrence in a manifest, never the content-addressed atom.
 * Identical text/tool results in distinct turns must remain distinct messages.
 */
export interface SemanticInstanceIdentity {
  instance_id: string;
  request_id: string;
  branch_id: string | null;
  ordinal: number;
  atom_hash: string;
  node_hash: string;
}

export function semanticMessageId(revision: string, instance: SemanticInstanceIdentity): string {
  if (!revision || !instance.instance_id || !Number.isSafeInteger(instance.ordinal) || instance.ordinal < 0) {
    throw new Error('Invalid semantic message identity');
  }
  return JSON.stringify([revision, instance.request_id, instance.branch_id, instance.instance_id]);
}

/** A cursor belongs to one immutable manifest revision. Never append a page
 * from a newer revision to an older visible window, including refresh races.
 */
export function assertSemanticPageRevision(expected: string, received: string): void {
  if (!expected || received !== expected) throw new Error('Semantic conversation revision changed');
}
