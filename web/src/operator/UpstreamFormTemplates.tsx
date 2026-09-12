import type { ObjectFieldTemplateProps } from '@rjsf/utils';
import ObjectFieldTemplate from '@rjsf/core/lib/components/templates/ObjectFieldTemplate.js';
import { useEffect, useRef } from 'react';
import { schemaFormTemplates } from '../SchemaTemplates';
import { useI18n } from '../i18n';

function UpstreamObjectTemplate(props: ObjectFieldTemplateProps) {
  const { t } = useI18n();
  const advanced = useRef<HTMLDetailsElement>(null);
  const advancedNames = ['network_scope', 'timeout_seconds', 'transport_policy'];
  const hasAdvancedError = advancedNames.some((name) => Boolean(props.errorSchema?.[name]));
  useEffect(() => { if (hasAdvancedError && advanced.current) advanced.current.open = true; }, [hasAdvancedError]);
  if (props.fieldPathId.path.length === 0 && props.schema.properties?.name && props.schema.properties?.config) {
    return <div className="upstream-form-sections">
      <fieldset><legend>{t('connection.identitySection')}</legend>{props.properties.filter((field) => field.name !== 'config').map((field) => field.content)}</fieldset>
      {props.properties.find((field) => field.name === 'config')?.content}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'config') {
    const fields = props.properties.filter((field) => advancedNames.includes(field.name));
    return <div className="upstream-form-sections">
      <ObjectFieldTemplate {...props} title={t('connection.endpointSection')} properties={props.properties.filter((field) => !advancedNames.includes(field.name))} />
      {fields.length > 0 && <details ref={advanced} className="upstream-advanced"><summary>{t('connection.advancedSection')}</summary><p className="field-hint">{t('connection.advancedHint')}</p>{fields.map((field) => field.content)}</details>}
    </div>;
  }
  return <ObjectFieldTemplate {...props} />;
}

export const upstreamFormTemplates = { ...schemaFormTemplates, ObjectFieldTemplate: UpstreamObjectTemplate };
