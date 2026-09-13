import { useId, useLayoutEffect, useRef, type ReactNode } from 'react';
import { Button } from '../design-system';
import { useI18n } from '../i18n';

/** One full-width work surface. Closing keeps the unsaved draft mounted. */
export function CreateJourney({ title, description, children, onOpen, open, onOpenChange, busy = false }: {
  title: string; description: string; children: ReactNode; onOpen?: () => void;
  open: boolean; onOpenChange: (open: boolean) => void; busy?: boolean;
}) {
  const { locale } = useI18n();
  const id = useId();
  const body = useRef<HTMLDivElement>(null);
  const toggle = useRef<HTMLButtonElement>(null);
  const previousOpen = useRef(open);
  const previousBusy = useRef(busy);
  const lastFocused = useRef<HTMLElement | null>(null);
  useLayoutEffect(() => {
    if (open && !previousOpen.current) body.current?.querySelector<HTMLElement>('input:not([type="hidden"]):not([disabled]), button:not([disabled])')?.focus();
    if (!open && previousOpen.current) toggle.current?.focus();
    previousOpen.current = open;
  }, [open]);
  useLayoutEffect(() => {
    if (open && previousBusy.current && !busy && lastFocused.current?.isConnected) lastFocused.current.focus();
    previousBusy.current = busy;
  }, [open, busy]);
  return <article className="create-resource create-journey" data-open={open}>
    <header className="journey-heading">
      {open && <h2 id={`${id}-title`}>{title}</h2>}
      <Button ref={toggle} data-workspace-toggle appearance={open ? 'subtle' : 'primary'} disabled={busy} aria-expanded={open} aria-controls={id} onClick={() => { onOpenChange(!open); if (!open) onOpen?.(); }}>{open ? locale.startsWith('zh') ? '关闭' : 'Close' : title}</Button>
    </header>
    <div ref={body} id={id} hidden={!open} role="region" aria-label={title} onFocusCapture={event => { lastFocused.current = event.target as HTMLElement; }}>
      <p className="create-journey-description">{description}</p>
      <div className="create-resource-body form-panel">{children}</div>
    </div>
  </article>;
}
