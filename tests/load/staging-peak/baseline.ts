import { createHash } from "node:crypto";

export type PeakBaseline = {
  schema_version: 1;
  evidence: { source: string; observed_from: string; observed_until: string; query_sha256: string };
  windows: { seconds: number; requests: number; max_in_flight: number; input_bytes: number; output_bytes: number }[];
  protocol_requests: Record<string, number>;
};

/** No guessed rates: incomplete historical evidence cannot qualify a run. */
export function validateBaseline(value: unknown): PeakBaseline {
  const data = value as PeakBaseline;
  const positive = (n: unknown): boolean => Number.isSafeInteger(n) && Number(n) > 0;
  if (data?.schema_version !== 1 || !data.evidence?.source || !/^[a-f0-9]{64}$/.test(data.evidence.query_sha256)) throw new Error("historical evidence identity is required");
  const start = Date.parse(data.evidence.observed_from), end = Date.parse(data.evidence.observed_until);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) throw new Error("historical observation window is invalid");
  if (!Array.isArray(data.windows) || !data.windows.length) throw new Error("measured peak windows are required");
  for (const window of data.windows) if (![window.seconds, window.requests, window.max_in_flight, window.input_bytes, window.output_bytes].every(positive)) throw new Error("peak rate, concurrency and byte evidence must be measured positive integers");
  if (!data.protocol_requests || !Object.keys(data.protocol_requests).length || !Object.values(data.protocol_requests).every(positive)) throw new Error("measured protocol mix is required");
  return data;
}

export function baselineIdentity(value: PeakBaseline): string {
  return createHash("sha256").update(JSON.stringify(validateBaseline(value))).digest("hex");
}
