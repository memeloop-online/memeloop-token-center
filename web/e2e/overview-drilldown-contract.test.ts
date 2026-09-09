import assert from 'node:assert/strict';
import test from 'node:test';

import { requestDrilldownForOverviewBucket } from '../src/operator/overviewDrilldown.js';

test('Overview request drilldowns preserve only the selected API time bucket', () => {
  assert.deepEqual(requestDrilldownForOverviewBucket(1_700_000_000_000, 'hour'), {
    logical_operator: 'and',
    conditions: [{
      field: 'created_at', operator: 'between', value: { type: 'timestamp', value: 1_700_000_000_000 },
      upper: { type: 'timestamp', value: 1_700_003_599_999 },
    }],
  });
  assert.deepEqual(requestDrilldownForOverviewBucket(1_700_000_000_000, 'day'), {
    logical_operator: 'and',
    conditions: [{
      field: 'created_at', operator: 'between', value: { type: 'timestamp', value: 1_700_000_000_000 },
      upper: { type: 'timestamp', value: 1_700_086_399_999 },
    }],
  });
});

test('Overview does not turn malformed bucket values into a request filter', () => {
  assert.equal(requestDrilldownForOverviewBucket(Number.NaN, 'hour'), undefined);
  assert.equal(requestDrilldownForOverviewBucket(-1, 'hour'), undefined);
  assert.equal(requestDrilldownForOverviewBucket(Number.MAX_SAFE_INTEGER, 'day'), undefined);
});
