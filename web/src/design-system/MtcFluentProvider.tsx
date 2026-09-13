import { useSyncExternalStore, type ReactNode } from 'react';
import { FluentProvider, webDarkTheme, webLightTheme, type Theme } from '@fluentui/react-components';
import './foundation.css';

const themes: Record<'light' | 'dark', Theme> = {
  light: { ...webLightTheme, fontFamilyBase: 'Inter, ui-sans-serif, system-ui, sans-serif' },
  dark: { ...webDarkTheme, fontFamilyBase: 'Inter, ui-sans-serif, system-ui, sans-serif' },
};

// Subscribe to the existing shell's source of truth, including external preference changes.
// No document access during render on the server; no second persisted theme setting.
function subscribe(onChange: () => void) {
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  return () => observer.disconnect();
}
function snapshot() {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}

export function MtcFluentProvider({ children }: { children: ReactNode }) {
  const theme = useSyncExternalStore<'light' | 'dark'>(subscribe, snapshot, () => 'dark');
  return <FluentProvider theme={themes[theme]} className="mtc-fluent-root">
    {children}
  </FluentProvider>;
}
