import type { ObjectFieldTemplateProps } from '@rjsf/utils';
import ObjectFieldTemplate from '@rjsf/core/lib/components/templates/ObjectFieldTemplate.js';
import { schemaFormTemplates } from '../SchemaTemplates';
import { useI18n } from '../i18n';
import { AdvancedFormSection, FormSection } from './FormSection';

function UpstreamObjectTemplate(props: ObjectFieldTemplateProps) {
  const { t } = useI18n();
  const advancedNames = ['network_scope', 'timeout_seconds', 'transport_policy'];
  const hasAdvancedError = advancedNames.some((name) => Boolean(props.errorSchema?.[name]));
  if (props.fieldPathId.path.length === 0 && props.schema.properties?.name && props.schema.properties?.config) {
    return <div className="upstream-form-sections">
      <FormSection title={t('connection.identitySection')}>{props.properties.filter((field) => field.name !== 'config').map((field) => field.content)}</FormSection>
      {props.properties.find((field) => field.name === 'config')?.content}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'config') {
    const fields = props.properties.filter((field) => advancedNames.includes(field.name));
    return <div className="upstream-form-sections">
      <FormSection title={t('connection.endpointSection')}>
        <ObjectFieldTemplate {...props} title="" properties={props.properties.filter((field) => !advancedNames.includes(field.name))} />
      </FormSection>
      {fields.length > 0 && <AdvancedFormSection title={t('connection.advancedSection')} description={t('connection.advancedHint')} invalid={hasAdvancedError}>{fields.map((field) => field.content)}</AdvancedFormSection>}
    </div>;
  }
  return <ObjectFieldTemplate {...props} />;
}

export const upstreamFormTemplates = { ...schemaFormTemplates, ObjectFieldTemplate: UpstreamObjectTemplate };
