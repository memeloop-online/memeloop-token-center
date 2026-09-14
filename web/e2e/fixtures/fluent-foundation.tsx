import React, { useEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { MtcFluentProvider, DataSurface, DetailTooltip, Disclosure, FormSection, Field, Input, ActionButton } from '../../src/design-system';
import '../../src/styles.css';
import '../../src/theme.css';

function ControlledForm() {
  const [open, setOpen] = useState(false);
  const [invalid, setInvalid] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!invalid || !open) return;
    // Accordion's context subscribers commit their visibility after the caller.
    const frame = requestAnimationFrame(() => input.current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [invalid, open]);
  return <>
    <ActionButton label="Validate advanced settings" onClick={() => { setInvalid(true); setOpen(true); }} />
    <Disclosure title="Controlled advanced settings" open={open} onOpenChange={setOpen}>
      <Field label="Draft proxy" validationState={invalid ? 'error' : 'none'} validationMessage={invalid ? 'Check proxy address' : undefined}>
        <Input input={{ ref: input }} defaultValue="draft proxy" />
      </Field>
    </Disclosure>
  </>;
}

createRoot(document.getElementById('root')!).render(<MtcFluentProvider>
  <DataSurface aria-label="Test surface">
    <FormSection title="Connection" description="Configure the account connection">
      <Field label="Account name" required><Input /></Field>
      <DetailTooltip content="Route ID: 0123456789"><button type="button" className="mtc-detail-trigger">Route details</button></DetailTooltip>
      <Disclosure title="高级设置"><p>网络代理</p></Disclosure>
      <ControlledForm />
      <ActionButton appearance="primary" label="Save" />
    </FormSection>
  </DataSurface>
</MtcFluentProvider>);
