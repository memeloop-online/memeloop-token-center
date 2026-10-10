export const upstreamNotesMaxScalars = 16384;
export const upstreamNotesMaxUtf8Bytes = 65536;

export type UpstreamNotesIssue = 'tooLong' | 'controlChars';

// Allows TAB/LF/CR only; mirrors the backend rejection set.
const DISALLOWED_CONTROL = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/;

const utf8 = new TextEncoder();

export function upstreamNotesScalarCount(source: string): number {
  return [...source].length;
}

export function validateUpstreamNotes(source: string): UpstreamNotesIssue | undefined {
  if (DISALLOWED_CONTROL.test(source)) return 'controlChars';
  if (upstreamNotesScalarCount(source) > upstreamNotesMaxScalars || utf8.encode(source).length > upstreamNotesMaxUtf8Bytes) return 'tooLong';
  return undefined;
}

/** Whitespace-only content clears the note; anything else is preserved verbatim. */
export function normalizeUpstreamNotes(source: string): string | null {
  return source.trim() ? source : null;
}

export function upstreamNotesDirty(saved: string | null | undefined, draft: string): boolean {
  return normalizeUpstreamNotes(draft) !== (saved ?? null);
}
