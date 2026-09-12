import { useEffect, useLayoutEffect, useRef, useState, type InputHTMLAttributes } from 'react';
import { useI18n } from './i18n';
import './secretInput.css';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'defaultValue' | 'value'> & { label: string; value?: string };

/** Never copies, persists, logs or renders the value outside its input. */
export function SecretInput({ label, value = '', autoComplete = 'new-password', ...props }: Props) {
  const { t } = useI18n();
  const [visible, setVisible] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  // Keep the editable value in the live input property, never a serialized
  // value/defaultValue, title, data-* or accessibility attribute.
  useLayoutEffect(() => {
    if (input.current && input.current.value !== value) input.current.value = value;
  }, [value]);
  useEffect(() => { if (props.disabled || props.readOnly) setVisible(false); }, [props.disabled, props.readOnly]);
  useEffect(() => {
    const hide = () => setVisible(false);
    window.addEventListener('blur', hide);
    document.addEventListener('visibilitychange', hide);
    return () => { window.removeEventListener('blur', hide); document.removeEventListener('visibilitychange', hide); };
  }, []);
  return <div className="secret-input" onBlur={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget)) setVisible(false);
  }} onKeyDown={(event) => {
    if (event.key === 'Escape' && visible) { setVisible(false); event.stopPropagation(); }
  }}>
    <input {...props} ref={input} type={visible ? 'text' : 'password'} autoComplete={autoComplete} autoCapitalize="none" spellCheck={false} />
    <button type="button" className="secondary secret-input-toggle" aria-controls={props.id} aria-label={t(visible ? 'secret.hideField' : 'secret.showField', { field: label })} aria-pressed={visible} disabled={props.disabled || props.readOnly} onClick={() => setVisible((value) => !value)}>{t(visible ? 'secret.hide' : 'secret.show')}</button>
  </div>;
}
