import assert from "node:assert/strict";
import test from "node:test";
import { sequentialScanEvidence } from "../load/conversation_explain.ts";

test("empty and tiny PostgreSQL partition scans remain bounded evidence", () => {
  const evidence = sequentialScanEvidence({
    "Node Type": "Append",
    Plans: [
      { "Node Type": "Seq Scan", "Relation Name": "request_records_20260830", "Actual Rows": 0, "Rows Removed by Filter": 0, "Shared Hit Blocks": 0, "Shared Read Blocks": 0 },
      { "Node Type": "Seq Scan", "Relation Name": "request_records_20260829", "Actual Rows": 12, "Rows Removed by Filter": 7, "Shared Hit Blocks": 2, "Shared Read Blocks": 1 },
      { "Node Type": "Seq Scan", "Relation Name": "unrelated_table", "Actual Rows": 10_000, "Rows Removed by Filter": 0, "Shared Hit Blocks": 1_000, "Shared Read Blocks": 0 },
    ],
  });
  assert.deepEqual(evidence.forbidden, []);
  assert.deepEqual(evidence.bounded.map((scan) => scan.relation), ["request_records_20260830", "request_records_20260829"]);
});

test("a sequential scan doing bulk row or block work fails closed", () => {
  const evidence = sequentialScanEvidence({
    "Node Type": "Append",
    Plans: [
      { "Node Type": "Seq Scan", "Relation Name": "request_records_default", "Actual Rows": 1, "Rows Removed by Filter": 256, "Shared Hit Blocks": 3, "Shared Read Blocks": 0 },
      { "Node Type": "Seq Scan", "Relation Name": "conversation_observations", "Actual Rows": 1, "Rows Removed by Filter": 0, "Shared Hit Blocks": 64, "Shared Read Blocks": 1 },
    ],
  });
  assert.deepEqual(evidence.bounded, []);
  assert.deepEqual(evidence.forbidden.map((scan) => scan.relation), ["request_records_default", "conversation_observations"]);
});
