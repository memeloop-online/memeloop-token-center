import type { Plugin } from 'vite';

export const prefixTraceInit = `(() => {
  const events = [];
  let posts = 0;
  window.formPrefixTrace = events;
  window.recordFormPrefix = (stage, value, hasOwn, emptyValue, hasEmptyValue) => {
    events.push({ stage, type: typeof value, empty: value === '', hasOwn,
      options: { emptyValue: { type: typeof emptyValue, empty: emptyValue === '', hasOwn: hasEmptyValue } } });
    if (events.length > 48) events.splice(0, events.length - 48);
  };
  window.recordFormPrefixPost = credential => {
    posts++;
    window.recordFormPrefix('POST-' + posts, credential?.prefix,
      credential != null && Object.hasOwn(credential, 'prefix'), undefined, false);
  };
})();`;

export function prefixTracePlugin(): Plugin {
  return {
    name: 'fixture-prefix-trace',
    enforce: 'pre',
    transform(code, id) {
      const path = id.split('?')[0];
      const record = 'window.recordFormPrefix?.';
      const replace = (needle: string, replacement: string) => {
        if (!code.includes(needle)) throw new Error(`Prefix trace boundary missing: ${path}`);
        code = code.replace(needle, replacement);
      };
      if (path.endsWith('/src/operator/FluentFormWidgets.tsx')) {
        replace('function TextWidget({', 'function TextWidget(props: WidgetProps) { const {');
        replace('autofocus }: WidgetProps) {', 'autofocus, options } = props;');
        replace("  const text =", `  if (id === 'root_credential_prefix') ${record}('widget-render', value, Object.hasOwn(props, 'value'), options.emptyValue, Object.hasOwn(options, 'emptyValue'));\n  const text =`);
        replace('onChange(data.value)', `(id === 'root_credential_prefix' && ${record}('widget-change', data.value, Object.hasOwn(data, 'value'), options.emptyValue, Object.hasOwn(options, 'emptyValue')), onChange(data.value))`);
      } else if (path.endsWith('/@rjsf/core/lib/components/fields/StringField.js')) {
        replace('const Widget = getWidget', `if (fieldPathId.$id === 'root_credential_prefix') ${record}('StringField-render', formData, Object.hasOwn(props, 'formData'), options.emptyValue, Object.hasOwn(options, 'emptyValue'));\n    const Widget = getWidget`);
        replace('=> onChange(value, fieldPathId.path, errorSchema, id)', `=> (fieldPathId.$id === 'root_credential_prefix' && ${record}('StringField-change', value, true, options.emptyValue, Object.hasOwn(options, 'emptyValue')), onChange(value, fieldPathId.path, errorSchema, id))`);
      } else if (path.endsWith('/src/operator/pages/ManagementPages.tsx')) {
        replace('onChange={({ formData }) => { if (providerCreateScope', `onChange={({ formData }) => { ${record}('form-change', formData?.credential?.prefix, formData?.credential != null && Object.hasOwn(formData.credential, 'prefix'), undefined, false); if (providerCreateScope`);
        replace('onSubmit={({ formData }) => void createProvider(formData)}', `onSubmit={({ formData }) => { ${record}('form-submit', formData?.credential?.prefix, formData?.credential != null && Object.hasOwn(formData.credential, 'prefix'), undefined, false); void createProvider(formData); }}`);
      } else if (path.endsWith('/e2e/fixtures/form-journey.tsx')) {
        replace("window.formJourneyLastProviderCreate = JSON.parse(String(init?.body ?? '{}'));", "window.formJourneyLastProviderCreate = JSON.parse(String(init?.body ?? '{}')); window.recordFormPrefixPost?.(window.formJourneyLastProviderCreate?.credential);");
      } else return;
      return { code, map: null };
    },
  };
}
