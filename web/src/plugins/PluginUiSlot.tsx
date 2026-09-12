import { useEffect, useRef, useState } from 'react';
import { parsePluginUiProjection, type PluginUiPolicy, type PluginUiProjection } from './uiProjection';
import './pluginUiSlot.css';

export interface PluginUiSlotProps extends PluginUiPolicy {
  /** Changes on tenant, principal, credential/authorization revision, or plugin revision. */
  scopeKey: string;
  title: string;
  messages: { loading: string; unavailable: string; empty: string; states: Record<'ok' | 'warning' | 'error' | 'unknown', string> };
  /** Core-owned authenticated loader. Never a URL, script, or callback from plugin data. */
  load: (signal: AbortSignal) => Promise<unknown>;
}

/** Keyed inner boundary synchronously discards old-scope data before effects run. */
export function PluginUiSlot(props: PluginUiSlotProps) {
  return <ScopedSlot key={JSON.stringify([props.scopeKey, props.pluginId, props.slotId, props.allowedLinkOrigins])} {...props} />;
}

function ScopedSlot({ title, messages, load, ...policy }: PluginUiSlotProps) {
  const container = useRef<HTMLElement>(null);
  const [visible, setVisible] = useState(false);
  const [result, setResult] = useState<PluginUiProjection | 'error' | null>(null);
  const loader = useRef(load);
  loader.current = load;
  const policyRef = useRef(policy);
  useEffect(() => {
    if (!container.current) return;
    if (typeof IntersectionObserver === 'undefined') { setVisible(true); return; }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) { setVisible(true); observer.disconnect(); }
    });
    observer.observe(container.current);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!visible) return;
    const controller = new AbortController();
    let active = true;
    const timeout = window.setTimeout(() => {
      if (active) { active = false; setResult('error'); controller.abort(); }
    }, 10_000);
    Promise.resolve().then(() => loader.current(controller.signal)).then((value) => {
      if (active) setResult(parsePluginUiProjection(value, policyRef.current) ?? 'error');
    }).catch(() => { if (active) setResult('error'); }).finally(() => window.clearTimeout(timeout));
    return () => { active = false; controller.abort(); window.clearTimeout(timeout); };
  }, [visible]);
  return <section ref={container} className="panel plugin-ui-slot" aria-label={title} aria-busy={visible && result === null}>
    <h3>{title}</h3>
    {result === null ? <p role="status">{messages.loading}</p>
      : result === 'error' ? <p role="status">{messages.unavailable}</p>
        : result.components.length === 0 ? <p>{messages.empty}</p>
          : <div className="plugin-ui-components">{result.components.map((component, index) => {
            switch (component.kind) {
              case 'text': return <p key={index}>{component.text}</p>;
              case 'metric': return <dl key={index}><dt>{component.label}</dt><dd>{component.value}</dd></dl>;
              case 'status': return <p key={index}>{component.label}: <span className="plugin-ui-state">{messages.states[component.state]}</span></p>;
              case 'link': return <a key={index} href={component.href} target="_blank" rel="noopener noreferrer" referrerPolicy="no-referrer">{component.label}</a>;
            }
          })}</div>}
  </section>;
}
