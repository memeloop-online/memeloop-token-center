import { cloneElement, useId, useState, type HTMLAttributes, type ReactElement, type ReactNode } from 'react';
import { Accordion, AccordionHeader, AccordionItem, AccordionPanel, Button, Tooltip, type ButtonProps } from '@fluentui/react-components';

/** Layout only: interactive controls remain upstream Fluent components. */
export function DataSurface({ children, className = '', ...props }: HTMLAttributes<HTMLElement>) {
  return <section {...props} className={`mtc-data-surface ${className}`}>{children}</section>;
}

/** Supplemental noninteractive content only. Focus and tap expose the same detail as hover. */
export function DetailTooltip({ children, content }: {
  children: ReactElement<HTMLAttributes<HTMLElement>>; content: ReactNode;
}) {
  const [visible, setVisible] = useState(false);
  return <Tooltip content={content} relationship="description" withArrow
    visible={visible} onVisibleChange={(_, data) => setVisible(data.visible)}>
    {cloneElement(children, {
      onClick: (event) => {
        children.props.onClick?.(event);
        if (!event.defaultPrevented) setVisible(true);
      },
    })}
  </Tooltip>;
}

export function FormSection({ title, description, children }: {
  title: string; description?: string; children: ReactNode;
}) {
  const id = useId();
  return <fieldset className="mtc-form-section" aria-describedby={description ? id : undefined}>
    <legend>{title}</legend>
    {description && <p id={id} className="mtc-secondary-text">{description}</p>}
    <div className="mtc-form-fields">{children}</div>
  </fieldset>;
}

/** Localized visible text is mandatory; do not turn ordinary actions into icon puzzles. */
export function ActionButton({ label, ...props }: Omit<Extract<ButtonProps, { as?: 'button' }>, 'children' | 'as'> & { label: string }) {
  return <Button {...props} as="button">{label}</Button>;
}

/** Standard advanced-settings disclosure; no native details/summary triangle styling. */
export function Disclosure({ title, children, defaultOpen = false, open, onOpenChange }: {
  title: string; children: ReactNode; defaultOpen?: boolean;
  open?: boolean; onOpenChange?: (open: boolean) => void;
}) {
  return <Accordion collapsible defaultOpenItems={open === undefined ? (defaultOpen ? ['content'] : []) : undefined}
    openItems={open === undefined ? undefined : open ? ['content'] : []}
    onToggle={(_, data) => onOpenChange?.(data.openItems.includes('content'))}>
    <AccordionItem value="content">
      <AccordionHeader>{title}</AccordionHeader>
      {/* Fluent's default motion unmounts on exit and defers visibility to an
          animation frame. Keep the panel mounted and synchronously hide it so
          validation focus need not wait for an animation duration. */}
      <AccordionPanel collapseMotion={{
        children: (_, props) => cloneElement(props.children, { hidden: !props.visible }),
      }}>{children}</AccordionPanel>
    </AccordionItem>
  </Accordion>;
}
