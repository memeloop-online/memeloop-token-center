import { isOperatorLoginCycleCurrent } from '../credentialStorage.js';
import type { TypedFilterAst } from '../types.js';
import { decodeTypedFilterAst } from './typedFilterContract.js';

const storageKey = 'mtc.operator.request-filter.reload.v1';
export interface RequestFilterReload {
  cycle: string;
  tenant: string;
  ast: TypedFilterAst;
}

export function clearRequestFilterReload() {
  try { window.sessionStorage.removeItem(storageKey); } catch { /* Filtering also works without storage. */ }
}

/** Only a reload may consume this tab's last applied filter. */
export function readRequestFilterReload(): RequestFilterReload | undefined {
  try {
    if ((performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming | undefined)?.type !== 'reload') {
      clearRequestFilterReload();
      return undefined;
    }
    const value: unknown = JSON.parse(window.sessionStorage.getItem(storageKey) ?? 'null');
    if (value && typeof value === 'object' && 'v' in value && value.v === 1
      && 'cycle' in value && typeof value.cycle === 'string' && value.cycle
      && 'tenant' in value && typeof value.tenant === 'string' && value.tenant && 'ast' in value) {
      const ast = decodeTypedFilterAst(value.ast);
      if (ast) return { cycle: value.cycle, tenant: value.tenant, ast };
    }
    clearRequestFilterReload();
  } catch { clearRequestFilterReload(); }
  return undefined;
}

export function rememberRequestFilterReload(snapshot: RequestFilterReload) {
  if (!isOperatorLoginCycleCurrent(snapshot.cycle)) return;
  if (snapshot.ast.conditions.length === 0) { clearRequestFilterReload(); return; }
  try { window.sessionStorage.setItem(storageKey, JSON.stringify({ v: 1, ...snapshot })); }
  catch { clearRequestFilterReload(); }
}
