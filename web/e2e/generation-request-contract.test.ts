import assert from 'node:assert/strict';
import test from 'node:test';

import { buildGenerationInput, generationNeedsDuration, usesGenerationParameters } from '../src/self/generationRequest.js';
import type { ModelCatalogItem } from '../src/types.js';

function model(overrides: Partial<ModelCatalogItem>): ModelCatalogItem {
  return { id: 'test-model', object: 'model', owned_by: 'memeloop', modalities: ['video'], ...overrides };
}

test('Seedance video keeps its content and duration request contract', () => {
  const seedance = model({
    driver: 'volcengine-seedance',
    generation_schema: { type: 'object' },
  });
  assert.equal(usesGenerationParameters(seedance), false);
  assert.equal(generationNeedsDuration('video', seedance), true);
  assert.deepEqual(buildGenerationInput('video', seedance, 'a fox runs', '5', {}), {
    duration: 5,
    content: [{ type: 'text', text: 'a fox runs' }],
  });
});

test('ComfyUI video sends parameters only and does not ask for Seedance duration', () => {
  const comfy = model({
    driver: 'comfyui',
    generation_schema: { type: 'object', properties: { prompt: { type: 'string' }, seed: { type: 'integer' } } },
  });
  assert.equal(usesGenerationParameters(comfy), true);
  assert.equal(generationNeedsDuration('video', comfy), false);
  assert.deepEqual(buildGenerationInput('video', comfy, 'a fox runs', '60', { seed: 42 }), {
    parameters: { seed: 42, prompt: 'a fox runs' },
  });
});

test('capability input style is authoritative and old schema catalogs remain compatible', () => {
  const capabilityModel = model({ capabilities: { generation_driver: 'custom-workflow', generation_input: 'parameters' } });
  assert.deepEqual(buildGenerationInput('video', capabilityModel, 'workflow prompt', '9', { steps: 12 }), {
    parameters: { steps: 12, prompt: 'workflow prompt' },
  });

  const legacyComfy = model({ generation_schema: { type: 'object' } });
  assert.deepEqual(buildGenerationInput('video', legacyComfy, 'legacy prompt', '9', {}), {
    parameters: { prompt: 'legacy prompt' },
  });
});

test('root generation_driver catalogs identify ComfyUI video transport', () => {
  const comfy = model({ generation_driver: 'comfyui' });
  assert.equal(generationNeedsDuration('video', comfy), false);
  assert.deepEqual(buildGenerationInput('video', comfy, 'root metadata', '12', { seed: 7 }), {
    parameters: { seed: 7, prompt: 'root metadata' },
  });
});
