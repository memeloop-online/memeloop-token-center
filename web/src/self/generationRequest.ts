import type { ModelCatalogItem } from '../types.js';

export type GenerationKind = 'image' | 'video';

function generationDriver(model: ModelCatalogItem | undefined): string | undefined {
  const capabilityDriver = model?.capabilities?.generation_driver;
  return typeof model?.driver === 'string'
    ? model.driver
    : typeof model?.generation_driver === 'string'
      ? model.generation_driver
      : typeof capabilityDriver === 'string'
        ? capabilityDriver
        : undefined;
}

/**
 * ComfyUI routes consume a workflow parameter object for both images and videos.
 * A schema is a compatibility signal for older catalogs: currently the server
 * publishes generation_schema only for ComfyUI routes.
 */
export function usesGenerationParameters(model: ModelCatalogItem | undefined): boolean {
  const input = model?.capabilities?.generation_input;
  if (input === 'parameters') return true;
  if (input === 'content') return false;
  const driver = generationDriver(model);
  if (driver === 'volcengine-seedance') return false;
  return driver === 'comfyui' || model?.generation_schema !== undefined;
}

export function generationNeedsDuration(kind: GenerationKind, model: ModelCatalogItem | undefined): boolean {
  return kind === 'video' && !usesGenerationParameters(model);
}

export function buildGenerationInput(
  kind: GenerationKind,
  model: ModelCatalogItem | undefined,
  prompt: string,
  duration: string,
  parameters: Record<string, unknown>,
): Record<string, unknown> {
  const parameterPayload = { ...parameters, prompt };
  if (kind === 'image' || usesGenerationParameters(model)) {
    return { parameters: parameterPayload };
  }
  return {
    duration: Number(duration),
    content: [{ type: 'text', text: prompt }],
  };
}
