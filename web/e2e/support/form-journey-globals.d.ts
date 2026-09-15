/** Instrumentation exposed only by the in-memory form journey fixture. */
export {};

declare global {
  interface Window {
    formJourneyReads: string[];
    formJourneyWrites: number;
    formJourneyLastProviderWrite?: { name: string; config: Record<string, unknown>; tenant_external_id: string; expected_updated_at: number };
    failNextFormWrite: boolean;
    deferNextFormQuotaRead: boolean;
    releaseFormQuotaRead: () => void;
    deferNextFormProxyRead: boolean;
    releaseFormProxyRead: () => void;
  }
}
