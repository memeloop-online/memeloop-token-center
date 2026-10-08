import { useEffect, useId, useLayoutEffect, useRef, type ReactNode, type SyntheticEvent } from 'react';
import { Tab, TabList } from '../../design-system';
import { useI18n } from '../../i18n';
import { useConfirmDialog } from '../../useConfirmDialog';
import { useGuardedNavigation, useNavigationGuard } from '../../app/NavigationGuard';
import { identityCopy } from '../identityCopy';
import './identityWorkspace.css';

export type IdentityTab = 'tenants' | 'service-credentials';

export function IdentityPage({ tab, onNavigate, children }: {
  tab: IdentityTab;
  onNavigate: (tab: IdentityTab) => void;
  children: ReactNode;
}) {
  const { locale } = useI18n();
  const copy = identityCopy(locale);
  const prefix = useId();
  const navigate = useGuardedNavigation();
  const workspace = useRef<HTMLElement>(null);
  const drafts = useRef(new WeakMap<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement, string>());
  const { confirm, confirmationDialog } = useConfirmDialog([]);
  const fieldValue = (field: HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement) => field instanceof HTMLInputElement && (field.type === 'checkbox' || field.type === 'radio') ? String(field.checked) : field.value;
  const trackDraft = (event: SyntheticEvent) => {
    if (!(event.target instanceof Element)) return;
    const form = event.target.closest('form');
    const fields = form ? Array.from(form.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>('input, select, textarea'))
      : event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement || event.target instanceof HTMLTextAreaElement ? [event.target] : [];
    for (const field of fields) {
      if ((field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) && field.readOnly) continue;
      if (!drafts.current.has(field)) drafts.current.set(field, fieldValue(field));
    }
  };
  const dirty = () => Array.from(workspace.current?.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>('input,select,textarea') ?? [])
    .some(field => drafts.current.has(field) && fieldValue(field) !== drafts.current.get(field));
  useNavigationGuard(async () => !dirty() || await confirm(copy.leaveDraft));
  useLayoutEffect(() => {
    drafts.current = new WeakMap();
    return () => { drafts.current = new WeakMap(); };
  }, [tab]);
  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (!dirty()) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, []);
  return <section ref={workspace} className="identity-workspace" aria-label={copy.title} onFocusCapture={trackDraft} onPointerDownCapture={trackDraft} onKeyDownCapture={trackDraft}>
    {confirmationDialog}
    <header className="panel-title"><div><h2>{copy.title}</h2><p className="muted">{copy.description}</p></div></header>
    <TabList selectedValue={tab} aria-label={copy.title} onTabSelect={(_, data) => {
      const next = data.value;
      if (next !== tab && (next === 'tenants' || next === 'service-credentials')) void navigate(() => onNavigate(next));
    }}>
      <Tab id={`${prefix}-tenants`} value="tenants" aria-controls={`${prefix}-tenants-panel`}>{copy.tenants}</Tab>
      <Tab id={`${prefix}-service-credentials`} value="service-credentials" aria-controls={`${prefix}-service-credentials-panel`}>{copy.services}</Tab>
    </TabList>
    <div key={tab} id={`${prefix}-${tab}-panel`} role="tabpanel" aria-labelledby={`${prefix}-${tab}`} tabIndex={0}>{children}</div>
  </section>;
}
