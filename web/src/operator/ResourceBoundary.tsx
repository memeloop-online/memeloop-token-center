import type { ReactNode } from 'react';
import { Button, LoadingProgress, LoadingState, type LoadingLevel, type LoadingVariant } from '../design-system';
import { useI18n } from '../i18n';
import type { ResourceState } from './hooks/useOperatorResource';

export function ResourceBoundary<T>({ resource, scopeKey, children, level = 'page', variant = 'list', onRetry, refreshErrorPresentation = 'boundary' }: {
  resource: ResourceState<T>;
  scopeKey: string;
  children: (value: T) => ReactNode;
  level?: LoadingLevel;
  variant?: LoadingVariant;
  onRetry?: () => void;
  refreshErrorPresentation?: 'boundary' | 'caller';
}) {
  const { t } = useI18n();
  const matchesScope = resource.scopeKey === scopeKey;
  const ready = matchesScope && resource.kind === 'ready';
  const failed = matchesScope && resource.kind === 'failed';
  const inactive = matchesScope && resource.kind === 'idle' && resource.disabled === true;
  const pending = !ready && !failed && !inactive;
  const refreshing = ready && resource.refreshing === true;
  return <div className={`mtc-resource-boundary mtc-resource-${variant}`} aria-busy={pending || refreshing}>
    <LoadingProgress active={pending || refreshing} label={t('common.loading')} level={level} />
    {ready
      ? <>{refreshErrorPresentation === 'boundary' && resource.refreshError && <div className="notice error" role="alert">{resource.refreshError}{onRetry && <Button appearance="secondary" onClick={onRetry}>{t('common.retry')}</Button>}</div>}{children(resource.value)}</>
      : failed
        ? <div className="notice error" role="alert">{resource.message}{onRetry && <Button appearance="secondary" onClick={onRetry}>{t('common.retry')}</Button>}</div>
        : inactive ? null : <LoadingState label={t('common.loading')} variant={variant} />}
  </div>;
}
