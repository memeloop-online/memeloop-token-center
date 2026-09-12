import { useLayoutEffect, useRef } from 'react';

/** Return to the originating row only after close and any pending save settle. */
export function useInlineEditorFocus(activeId: string | undefined, busy: boolean, scope: string) {
  const container = useRef<HTMLElement>(null);
  const origin = useRef<{ id: string; button: HTMLButtonElement } | undefined>(undefined);
  const previousId = useRef<string | undefined>(undefined);
  const previousScope = useRef(scope);
  const rememberTrigger = (id: string, button: HTMLButtonElement) => { origin.current = { id, button }; };

  useLayoutEffect(() => {
    if (previousScope.current !== scope) {
      previousScope.current = scope; previousId.current = undefined; origin.current = undefined;
      return;
    }
    if (activeId) {
      if (previousId.current !== activeId) {
        const editor = container.current?.querySelector<HTMLElement>('.inline-editor');
        editor?.scrollIntoView({ block: 'start' });
        editor?.querySelector<HTMLInputElement>('input:not([type="hidden"]):not([disabled])')?.focus({ preventScroll: true });
      }
      previousId.current = activeId;
      return;
    }
    if (busy || !origin.current) return;
    const { id, button } = origin.current;
    // A list refresh can replace the DOM node. Resolve by stable resource ID,
    // never by row index or a translated label shared by other edit buttons.
    const trigger = button.isConnected ? button : container.current?.querySelector<HTMLButtonElement>(`[data-inline-edit-trigger="${CSS.escape(id)}"]`);
    if (trigger && !trigger.disabled) {
      trigger.closest('details')?.setAttribute('open', '');
      trigger.focus();
    }
    origin.current = undefined; previousId.current = undefined;
  }, [activeId, busy, scope]);
  return { container, rememberTrigger };
}
