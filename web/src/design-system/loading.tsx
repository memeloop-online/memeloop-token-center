import { createContext, useCallback, useContext, useEffect, useId, useMemo, useState, type ReactNode } from 'react';
import { ProgressBar, Skeleton, SkeletonItem } from '@fluentui/react-components';
import './loading.css';

export type LoadingLevel = 'page' | 'section';
export type LoadingVariant = 'list' | 'detail' | 'compact' | 'inline';

const PageLoadingContext = createContext<{
  register: (identity: string) => () => void;
} | undefined>(undefined);

function usePageLoading(active: boolean, level: LoadingLevel) {
  const context = useContext(PageLoadingContext);
  const identity = useId();
  useEffect(() => {
    if (active && level === 'page') return context?.register(identity);
  }, [active, context, identity, level]);
  return context;
}

function PageLoadingRoot({ label, busy, children, className }: {
  label: string; busy: boolean; children: ReactNode; className: string;
}) {
  const [pending, setPending] = useState<ReadonlySet<string>>(() => new Set());
  const register = useCallback((identity: string) => {
    setPending((current) => new Set(current).add(identity));
    return () => setPending((current) => {
      const next = new Set(current);
      next.delete(identity);
      return next;
    });
  }, []);
  const context = useMemo(() => ({ register }), [register]);
  const active = busy || pending.size > 0;
  return <PageLoadingContext.Provider value={context}>
    <div className={`mtc-page-loading-region ${className}`}>
      <div className="mtc-page-loading-announcement" data-page-loading-announcement role="status" aria-live="polite" aria-atomic="true">
        <span>{active ? label : ''}</span>
        <div className="mtc-loading-track" aria-hidden="true">{active && <ProgressBar />}</div>
      </div>
      {children}
    </div>
  </PageLoadingContext.Provider>;
}

function NestedLoadingRegion({ busy, children, className }: {
  busy: boolean; children: ReactNode; className: string;
}) {
  usePageLoading(busy, 'page');
  return <div className={`mtc-page-loading-region ${className}`}>{children}</div>;
}

export function PageLoadingRegion({ scopeKey, label, busy = false, children, className = '' }: {
  scopeKey: string; label: string; busy?: boolean; children: ReactNode; className?: string;
}) {
  const parent = useContext(PageLoadingContext);
  return parent
    ? <NestedLoadingRegion key={scopeKey} busy={busy} className={className}>{children}</NestedLoadingRegion>
    : <PageLoadingRoot key={scopeKey} label={label} busy={busy} className={className}>{children}</PageLoadingRoot>;
}

export function LoadingProgress({ active, label, level = 'section' }: {
  active: boolean; label: string; level?: LoadingLevel;
}) {
  const context = usePageLoading(active, level);
  return <div className="mtc-loading-progress" data-loading-active={active || undefined}>
    {!context && level === 'page' && <div className="mtc-loading-standalone-announcement" role="status" aria-live="polite" aria-atomic="true">{active ? label : ''}</div>}
    <div className="mtc-loading-track" aria-hidden="true">{active && (!context || level === 'section') && <ProgressBar />}</div>
  </div>;
}

export function LoadingState({ label, level = 'section', variant = 'list', className = '' }: {
  label: string; level?: LoadingLevel; variant?: LoadingVariant; className?: string;
}) {
  return <div className={`mtc-loading-state mtc-loading-${variant} ${className}`} role="group" aria-busy="true" aria-label={label}>
    {level === 'page' && <LoadingProgress active label={label} level={level} />}
    <Skeleton animation="pulse" aria-hidden="true" className="mtc-loading-skeleton">
      {variant !== 'inline' && <SkeletonItem shape="rectangle" size={16} className="mtc-loading-heading" />}
      {Array.from({ length: variant === 'inline' ? 1 : variant === 'compact' ? 2 : 4 }, (_, index) => <SkeletonItem key={index} shape="rectangle" size={16} className="mtc-loading-row" />)}
    </Skeleton>
  </div>;
}
