import { useCallback, useEffect, useMemo, useState } from 'react';
import { useI18n } from '../i18n';

export type ResourceListStatusSelection = 'normal' | 'all';

export interface ResourceListStatusFilter<T> {
  selection: ResourceListStatusSelection;
  values: T[];
  totalCount: number;
  visibleCount: number;
  normalCount: number;
  inactiveCount: number;
  showInactive: boolean;
  setSelection: (selection: ResourceListStatusSelection) => void;
}

const storagePrefix = 'mtc.operator.resource-list-status.';

/**
 * Keep the persisted choice scoped to a resource kind and tenant, never to a
 * credential. This deliberately only owns the simple status dimension: the
 * typed filter builder can compose its AST result before it is passed here.
 */
export function resourceListStatusStorageKey(resource: string, scope: string) {
  return `${storagePrefix}${resource}.${scope || 'all-tenants'}`;
}

export function filterResourceListByStatus<T>(values: readonly T[], selection: ResourceListStatusSelection, isNormal: (value: T) => boolean) {
  return selection === 'all' ? [...values] : values.filter(isNormal);
}

function readSelection(key: string): ResourceListStatusSelection {
  if (typeof window === 'undefined') return 'normal';
  try {
    return window.localStorage.getItem(key) === 'all' ? 'all' : 'normal';
  } catch {
    return 'normal';
  }
}

export function useResourceListStatusFilter<T>(resource: string, scope: string, values: readonly T[], isNormal: (value: T) => boolean): ResourceListStatusFilter<T> {
  const storageKey = resourceListStatusStorageKey(resource, scope);
  const [selection, setStoredSelection] = useState<ResourceListStatusSelection>(() => readSelection(storageKey));

  useEffect(() => {
    setStoredSelection(readSelection(storageKey));
  }, [storageKey]);

  const setSelection = useCallback((next: ResourceListStatusSelection) => {
    setStoredSelection(next);
    if (typeof window === 'undefined') return;
    try {
      window.localStorage.setItem(storageKey, next);
    } catch {
      // Persistence is a convenience; storage restrictions must not block filtering.
    }
  }, [storageKey]);

  return useMemo(() => {
    const normalValues = values.filter(isNormal);
    const visibleValues = filterResourceListByStatus(values, selection, isNormal);
    return {
      selection,
      values: visibleValues,
      totalCount: values.length,
      visibleCount: visibleValues.length,
      normalCount: normalValues.length,
      inactiveCount: values.length - normalValues.length,
      showInactive: selection === 'all',
      setSelection,
    };
  }, [values, selection, isNormal, setSelection]);
}

export function ResourceListStatusFilterControl<T>({ filter, inactiveLabel }: { filter: ResourceListStatusFilter<T>; inactiveLabel: string }) {
  const { locale, t } = useI18n();
  return <div className="resource-list-status-filter" data-resource-list-status-filter>
    <span>{t('resourceList.visibleCount', { visible: filter.visibleCount.toLocaleString(locale), total: filter.totalCount.toLocaleString(locale) })}</span>
    {filter.inactiveCount > 0 && <button
      type="button"
      className="secondary"
      aria-pressed={filter.showInactive}
      onClick={() => filter.setSelection(filter.showInactive ? 'normal' : 'all')}
    >{filter.showInactive
      ? t('resourceList.hideInactive', { status: inactiveLabel })
      : t('resourceList.showInactive', { count: filter.inactiveCount.toLocaleString(locale), status: inactiveLabel })}
    </button>}
  </div>;
}

export function ResourceListStatusEmpty({ totalCount, normalLabel, empty }: { totalCount: number; normalLabel: string; empty: string }) {
  const { t } = useI18n();
  return <div className="empty">{totalCount === 0 ? empty : t('resourceList.noNormal', { status: normalLabel })}</div>;
}
