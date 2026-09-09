import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import { useI18n } from './i18n';
import './confirm-dialog.css';

type Confirmation = {
  message: string;
  revision: number;
  resolve: (accepted: boolean) => void;
  trigger: HTMLElement | null;
};

/** Confirmation is invalidated when its authority or resource scope changes. */
export function useConfirmDialog(scope: readonly unknown[]) {
  const { t } = useI18n();
  const id = useId();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const proceedRef = useRef<HTMLButtonElement>(null);
  const current = useRef<Confirmation | undefined>(undefined);
  const mounted = useRef(true);
  const scopeRef = useRef(scope);
  const revision = useRef(0);
  const [confirmation, setConfirmation] = useState<Confirmation>();
  if (scope.length !== scopeRef.current.length || scope.some((value, index) => value !== scopeRef.current[index])) {
    scopeRef.current = scope;
    revision.current += 1;
  }
  const renderedRevision = revision.current;
  const finish = (accepted: boolean) => {
    const pending = current.current;
    if (!pending) return;
    current.current = undefined;
    dialogRef.current?.close();
    setConfirmation(undefined);
    const sameScope = mounted.current && pending.revision === revision.current;
    pending.resolve(accepted && sameScope);
    if (sameScope && pending.trigger?.isConnected) pending.trigger.focus();
  };
  useLayoutEffect(() => {
    if (current.current && current.current.revision !== revision.current) finish(false);
    if (confirmation && current.current === confirmation && !dialogRef.current?.open) {
      dialogRef.current?.showModal();
      cancelRef.current?.focus();
    }
  });
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      current.current?.resolve(false);
      current.current = undefined;
    };
  }, []);
  const confirm = async (message: string) => {
    // Keep the first request; repeated activation must not replace its action.
    if (!mounted.current || current.current || renderedRevision !== revision.current) return false;
    const requestedRevision = renderedRevision;
    const accepted = await new Promise<boolean>((resolve) => {
      const pending = {
        message, revision: requestedRevision, resolve,
        trigger: document.activeElement instanceof HTMLElement ? document.activeElement : null,
      };
      current.current = pending;
      setConfirmation(pending);
    });
    return accepted && mounted.current && requestedRevision === revision.current;
  };
  const confirmationDialog = confirmation ? <dialog ref={dialogRef} className="app-confirm-dialog"
    aria-modal="true" aria-labelledby={`${id}-title`} aria-describedby={`${id}-description`}
    onKeyDown={(event) => {
      if (event.key !== 'Tab') return;
      // showModal makes the background inert, but browsers may still move
      // boundary Tab focus to browser chrome. Keep this two-action dialog's
      // keyboard sequence closed in both directions.
      const first = cancelRef.current;
      const last = proceedRef.current;
      if (!first || !last) return;
      if (event.shiftKey && (document.activeElement === first || document.activeElement === event.currentTarget)) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === event.currentTarget)) {
        event.preventDefault();
        first.focus();
      }
    }}
    onCancel={(event) => { event.preventDefault(); finish(false); }}
    onClose={() => finish(false)}>
    <h2 id={`${id}-title`}>{t('confirmation.title')}</h2>
    <p id={`${id}-description`}>{confirmation.message}</p>
    <div className="button-row">
      <button ref={cancelRef} type="button" className="secondary" onClick={() => finish(false)}>{t('common.cancel')}</button>
      <button ref={proceedRef} type="button" className="danger" onClick={() => finish(true)}>{t('confirmation.proceed')}</button>
    </div>
  </dialog> : null;
  return { confirm, confirmationDialog };
}
