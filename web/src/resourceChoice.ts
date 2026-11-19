import type { RJSFSchema } from '@rjsf/utils';

export interface ResourceChoiceHint { source: string; bindings: Record<string, string>; autoSelectSingle: boolean }
export interface ResourceChoiceOption { value: string; label: string; details?: string; data: Record<string, string> }

export function resourceChoiceHint(schema: RJSFSchema): ResourceChoiceHint | undefined {
  const hint = schema['x-resource-choice'];
  if (!hint || typeof hint !== 'object' || typeof hint.source !== 'string' || !hint.bindings || typeof hint.bindings !== 'object') return undefined;
  const bindings = Object.entries(hint.bindings);
  if (!bindings.length || bindings.some(([field, attribute]) => typeof attribute !== 'string' || !Object.hasOwn(schema.properties ?? {}, field))) return undefined;
  return { source: hint.source, bindings: hint.bindings, autoSelectSingle: hint.autoSelectSingle === true };
}
export function matchesResourceChoice(hint: ResourceChoiceHint, option: ResourceChoiceOption, data: Record<string, unknown>) {
  return Object.entries(hint.bindings).every(([field, attribute]) => option.data[attribute] !== undefined && data[field] === option.data[attribute]);
}
export function applyResourceChoice(hint: ResourceChoiceHint, option: ResourceChoiceOption, data: Record<string, unknown>) {
  return { ...data, ...Object.fromEntries(Object.entries(hint.bindings).map(([field, attribute]) => [field, option.data[attribute]])) };
}
export function emptyResourceChoice(hint: ResourceChoiceHint, data: Record<string, unknown>) {
  return Object.keys(hint.bindings).every(field => data[field] === '' || data[field] === undefined);
}
