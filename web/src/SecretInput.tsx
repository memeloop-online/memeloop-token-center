import { useCallback, useEffect, useLayoutEffect, useRef, useState, type InputHTMLAttributes } from 'react';
import { useI18n } from './i18n';
import './secretInput.css';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'defaultValue' | 'value'> & { label: string; value?: string };

/** Never copies, persists, logs or renders the value outside its input. */
export function SecretInput({ label, value = '', autoComplete = 'new-password', ...props }: Props) {
  const { t } = useI18n();
  const [visible, setVisible] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const hide = useCallback(() => {
    // Privacy events must mask immediately, before React's next scheduled
    // render (for example before the browser captures a backgrounded tab).
    if (input.current) input.current.type = 'password';
    setVisible(false);
  }, []);
  // Keep the editable value in the live input property, never a serialized
  // value/defaultValue, title, data-* or accessibility attribute.
  useLayoutEffect(() => {
    if (input.current && input.current.value !== value) input.current.value = value;
  }, [value]);
  useEffect(() => { if (props.disabled || props.readOnly) hide(); }, [props.disabled, props.readOnly, hide]);
  useEffect(() => {
    window.addEventListener('blur', hide);
    document.addEventListener('visibilitychange', hide);
    return () => { window.removeEventListener('blur', hide); document.removeEventListener('visibilitychange', hide); };
  }, [hide]);
  return <div className="secret-input" onBlur={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget)) hide();
  }} onKeyDown={(event) => {
    if (event.key === 'Escape' && visible) { hide(); event.stopPropagation(); }
  }}>
    <input {...props} ref={input} type={visible && !props.disabled && !props.readOnly ? 'text' : 'password'} autoComplete={autoComplete} autoCapitalize="none" spellCheck={false} />
    <button type="button" className="secondary secret-input-toggle" aria-controls={props.id} aria-label={t(visible ? 'secret.hideField' : 'secret.showField', { field: label })} aria-pressed={visible} disabled={props.disabled || props.readOnly} onClick={() => setVisible((value) => !value)}>{t(visible ? 'secret.hide' : 'secret.show')}</button>
  </div>;
}
