import { useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { Disclosure } from '../design-system';

/** Disclosure is a labelled action, not a modal or an icon-only affordance.
 * Contents remain mounted so closing never discards a draft. */
export function JourneyDisclosure({ title, description, children, invalid = false }: {
  title: string; description?: string; children: ReactNode; invalid?: boolean;
}) {
  const [open, setOpen] = useState(invalid);
  const section = useRef<HTMLElement>(null);
  const focusRequested = useRef(false);
  useLayoutEffect(() => {
    if (invalid) { focusRequested.current = true; setOpen(true); }
  }, [invalid]);
  useLayoutEffect(() => {
    if (!open || !focusRequested.current) return;
    const field = section.current?.querySelector<HTMLElement>('input[aria-invalid="true"], select[aria-invalid="true"], textarea[aria-invalid="true"], input:invalid, select:invalid, textarea:invalid');
    if (field) { field.focus(); focusRequested.current = false; }
  }, [open, invalid]);
  return <section ref={section} className="form-journey-disclosure" onInvalidCapture={() => { focusRequested.current = true; setOpen(true); }}>
    <Disclosure title={title} open={open} onOpenChange={setOpen}>
      {description && <p className="field-hint">{description}</p>}
      <div className="operator-form-section-fields">{children}</div>
    </Disclosure>
  </section>;
}
