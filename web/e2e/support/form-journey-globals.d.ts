/** Instrumentation exposed only by the in-memory form journey fixture. */
export {};

declare global {
  interface Window {
    formJourneyReads: string[];
    formJourneyWrites: number;
    formJourneyLastProviderWrite?: { name: string; config?: Record<string, unknown>; tenant_external_id: string; expected_updated_at: number };
    formJourneyLastProviderCreate?: { name: string; driver: string; config: Record<string, unknown>; credential: Record<string, unknown>; tenant_external_id: string };
    formJourneyLastNotesWrite?: { tenant_external_id: string; notes: string | null; expected_updated_at: number };
    failNextFormNotesWrite: boolean;
    conflictNextFormNotesWrite: boolean;
    deferNextFormNotesWrite: boolean;
    releaseFormNotesWrite: () => void;
    failNextFormWrite: boolean;
    deferNextFormProviderCreate: boolean;
    releaseFormProviderCreate: (status: number) => void;
    failNextFormAccountRead: boolean;
    changeFormProviderScope: (field: 'token' | 'tenant' | 'writeTenant') => void;
    deferNextFormQuotaRead: boolean;
    releaseFormQuotaRead: () => void;
    deferNextFormProxyRead: boolean;
    releaseFormProxyRead: () => void;
  }
}
