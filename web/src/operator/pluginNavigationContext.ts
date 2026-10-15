/** In-memory navigation input is valid only in the originating authenticated scope. */
export interface PluginNavigationContext {
  route: string;
  credentialGeneration: number;
  tenant: string;
  value: Record<string, unknown>;
}

export function resolvePluginNavigationContext(
  context: PluginNavigationContext | undefined,
  route: string,
  credentialGeneration: number,
  tenant: string,
): Record<string, unknown> | undefined {
  return context?.route === route
    && context.credentialGeneration === credentialGeneration
    && context.tenant === tenant ? context.value : undefined;
}
