import assert from 'node:assert/strict';
import test from 'node:test';
import type { ModelPriceView } from '../src/types.js';
import {
  loadModelPricePages,
  MODEL_PRICE_PAGE_SIZE,
  modelPricePagePath,
} from '../src/operator/pricingLoading.js';

function price(model: string): ModelPriceView {
  return {
    model,
    currency: 'USD',
    input_per_million: '1',
    output_per_million: '2',
    source: 'manual',
    updated_at: 1,
    tiers: [],
  };
}

test('model price path carries a bounded encoded keyset cursor', () => {
  assert.equal(
    modelPricePagePath('CNY', 'vendor/model + preview'),
    `/internal/v1/model-prices?currency=CNY&limit=${MODEL_PRICE_PAGE_SIZE}&after_model=vendor%2Fmodel+%2B+preview`,
  );
});

test('large model catalogs publish the first page before fetching the remainder', async () => {
  const first = Array.from(
    { length: MODEL_PRICE_PAGE_SIZE },
    (_, index) => price(`model-${String(index).padStart(4, '0')}`),
  );
  const remainder = [price('model-0200'), price('model-0201')];
  const paths: string[] = [];
  const published: number[] = [];
  const result = await loadModelPricePages(
    'USD',
    new AbortController().signal,
    async (path) => {
      paths.push(path);
      return paths.length === 1 ? first : remainder;
    },
    (prices) => published.push(prices.length),
  );

  assert.deepEqual(published, [MODEL_PRICE_PAGE_SIZE, MODEL_PRICE_PAGE_SIZE + 2]);
  assert.equal(result.length, MODEL_PRICE_PAGE_SIZE + 2);
  assert.match(paths[1] ?? '', /after_model=model-0199/);
});

test('a non-advancing page fails closed instead of spinning', async () => {
  let calls = 0;
  await assert.rejects(
    loadModelPricePages(
      'USD',
      new AbortController().signal,
      async () => {
        calls += 1;
        return Array.from({ length: MODEL_PRICE_PAGE_SIZE }, () => price('model-a'));
      },
      () => undefined,
    ),
    /did not advance its cursor/,
  );
  assert.equal(calls, 2);
});
