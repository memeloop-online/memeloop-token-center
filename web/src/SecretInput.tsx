import { useLayoutEffect, useRef, type InputHTMLAttributes } from 'react';
import './secretInput.css';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'defaultValue' | 'value'> & { label: string; value?: string; fluent?: boolean };

/** Authorized configuration values stay visible and directly editable. */
export function SecretInput({ label, value = '', autoComplete = 'off', fluent: _fluent, ...props }: Props) {
  const input = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => {
    if (input.current && input.current.value !== value) input.current.value = value;
  }, [value]);
  return <div className="secret-input">
    <input {...props} ref={input} type="text" aria-label={props['aria-label'] ?? label} autoComplete={autoComplete} autoCapitalize="none" spellCheck={false} />
  </div>;
}
