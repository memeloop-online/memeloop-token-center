import { useSyncExternalStore, type CSSProperties, type ReactNode } from 'react';
import { createDarkTheme, createLightTheme, FluentProvider, type Theme } from '@fluentui/react-components';
import './foundation.css';
import { brandRamp, dataThemes } from './dataTheme';

const themes: Record<'light' | 'dark', Theme> = {
  light: { ...createLightTheme(brandRamp), fontFamilyBase: 'Inter, ui-sans-serif, system-ui, sans-serif' },
  dark: { ...createDarkTheme(brandRamp), fontFamilyBase: 'Inter, ui-sans-serif, system-ui, sans-serif' },
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
  const colors = dataThemes[theme];
  const dataTokens = Object.fromEntries(Object.entries(colors).map(([name, value]) => [`--mtc-data-${name}`, value])) as CSSProperties;
  return <FluentProvider theme={themes[theme]} style={dataTokens} className="mtc-fluent-root">
    {children}
  </FluentProvider>;
}
