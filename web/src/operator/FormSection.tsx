import { useEffect, useId, useRef, type ReactNode } from 'react';
import './operatorFormSurfaces.css';

interface SectionProps { title: string; description?: string; children: ReactNode }

/** Native grouping keeps labels, required fields and browser validation intact. */
export function FormSection({ title, description, children }: SectionProps) {
  const id = useId();
  return <fieldset className="operator-form-section" aria-describedby={description ? `${id}-description` : undefined}>
    <legend>{title}</legend>
    {description && <p id={`${id}-description`} className="field-hint">{description}</p>}
    <div className="operator-form-section-fields">{children}</div>
  </fieldset>;
}

/** Validation reveals advanced fields without discarding the user's disclosure state. */
export function AdvancedFormSection({ title, description, children, invalid = false }: SectionProps & { invalid?: boolean }) {
  const section = useRef<HTMLDetailsElement>(null);
  const id = useId();
  useEffect(() => { if (invalid && section.current) section.current.open = true; }, [invalid]);
  return <details ref={section} className="upstream-advanced operator-form-advanced"
    onInvalidCapture={() => { if (section.current) section.current.open = true; }}>
    <summary aria-describedby={description ? `${id}-description` : undefined}>{title}</summary>
    {description && <p id={`${id}-description`} className="field-hint">{description}</p>}
    <div className="operator-form-section-fields">{children}</div>
  </details>;
}
