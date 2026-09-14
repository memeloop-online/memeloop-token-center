import RjsfForm from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { localizeSchema, useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { safeValidator as validator } from '../safeValidator';
import type { PluginConfiguration, PluginManifest } from '../types';

/** Schema rendering is heavy; request ownership remains in the lightweight disclosure. */
export function PluginConfigurationForm({ plugin, configuration, saving, writeTenant, onSubmit }: {
  plugin: PluginManifest;
  configuration?: PluginConfiguration;
  saving: boolean;
  writeTenant: string;
  onSubmit: (value: unknown) => Promise<void>;
}) {
  const { locale, t } = useI18n();
  const contribution = plugin.contributions.configuration;
  if (!contribution || !configuration) return null;
  return <RjsfForm
    key={`${plugin.id}-${configuration.scope_version}-${locale}`}
    schema={localizeSchema(contribution.schema as RJSFSchema, locale)}
    formData={configuration.value}
    validator={validator}
    templates={schemaFormTemplates}
    noHtml5Validate
    onError={() => { /* RJSF renders bounded validation errors inline. */ }}
    onSubmit={({ formData }) => { void onSubmit(formData); }}
  ><button type="submit" disabled={!writeTenant || saving}>{saving ? t('common.loading') : t('common.save')}</button></RjsfForm>;
}
