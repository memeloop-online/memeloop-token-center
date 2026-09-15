import type { BrandVariants } from '@fluentui/react-components';

/** The shell, Fluent controls and analytical surfaces use the same teal family. */
export const brandRamp: BrandVariants = {
  10: '#041310', 20: '#09251f', 30: '#0d362d', 40: '#10483c',
  50: '#105c4e', 60: '#0b6b5d', 70: '#087466', 80: '#087d70',
  90: '#269587', 100: '#4bae9f', 110: '#72c7ba', 120: '#96d9ce',
  130: '#b4e5dc', 140: '#cef0e8', 150: '#e3f7f1', 160: '#f3fcf8',
};

/** Shared semantic colors for CSS metrics and canvas charts. No chart-library defaults. */
export const dataThemes = {
  light: { primary: brandRamp[80], secondary: '#467f91', negative: '#b52f45', muted: '#536e79', surface: '#f3f7f8', border: '#d0dfe2' },
  dark: { primary: brandRamp[110], secondary: '#85b5c4', negative: '#ed9caa', muted: '#a6bac0', surface: '#12282b', border: '#30474e' },
} as const;
