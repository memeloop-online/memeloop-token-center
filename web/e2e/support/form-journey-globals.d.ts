/** Instrumentation exposed only by the in-memory form journey fixture. */
export {};

declare global {
  interface Window {
    formJourneyReads: string[];
    formJourneyWrites: number;
    failNextFormWrite: boolean;
    deferNextFormQuotaRead: boolean;
    releaseFormQuotaRead: () => void;
  }
}
