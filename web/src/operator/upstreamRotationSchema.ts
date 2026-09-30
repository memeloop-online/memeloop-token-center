import type { RJSFSchema, UiSchema } from '@rjsf/utils';
import { localizeSchema, type Locale } from '../i18n';
import { rotationFieldCopy, upstreamRotationCopy } from './upstreamRotationCopy';

export function upstreamRotationSchema(schema: RJSFSchema, locale: Locale): RJSFSchema {
  const copy = upstreamRotationCopy(locale);
  const titles: Record<string, string> = {
    'No authentication': copy.types.none, '无需认证': copy.types.none, 'API key': copy.types.api_key, 'API credential': copy.types.api_key, 'API 凭据': copy.types.api_key,
    'API key through an account proxy': copy.types.api_key_proxy, OAuth: copy.types.oauth,
  };
  const visit = (node: RJSFSchema): RJSFSchema => {
    const result = { ...node };
    if (result.title && titles[result.title]) result.title = titles[result.title];
    if (result.properties) result.properties = Object.fromEntries(Object.entries(result.properties).map(([name, definition]) => {
      if (typeof definition === 'boolean') return [name, definition];
      const child = visit(definition);
      const field = rotationFieldCopy[name];
      if (field) {
        child.title = field[locale === 'zh-CN' ? 0 : 1];
        child.description = field[locale === 'zh-CN' ? 2 : 3];
      }
      return [name, child];
    }));
    for (const keyword of ['oneOf', 'anyOf', 'allOf'] as const) {
      if (result[keyword]) result[keyword] = result[keyword].map(child => typeof child === 'boolean' ? child : visit(child));
    }
    for (const keyword of ['$defs', 'definitions'] as const) {
      if (result[keyword]) result[keyword] = Object.fromEntries(Object.entries(result[keyword]).map(([name, child]) => [name, typeof child === 'boolean' ? child : visit(child)]));
    }
    return result;
  };
  return visit(localizeSchema(schema, locale));
}

export function upstreamRotationUiSchema(locale: Locale): UiSchema {
  return Object.fromEntries(Object.entries(rotationFieldCopy).map(([name, field]) => [name, {
    'ui:help': field[locale === 'zh-CN' ? 2 : 3],
    ...(name === 'type' ? { 'ui:widget': 'hidden' } : {}),
  }]));
}
