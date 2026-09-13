import type { ObjectFieldTemplateProps } from '@rjsf/utils';
import ObjectFieldTemplate from '@rjsf/core/lib/components/templates/ObjectFieldTemplate.js';
import { schemaFormTemplates } from '../SchemaTemplates';
import { useI18n } from '../i18n';
import { AdvancedFormSection, FormSection } from './FormSection';

function UpstreamObjectTemplate(props: ObjectFieldTemplateProps) {
  const { t } = useI18n();
  const networkNames = ['network_scope', 'timeout_seconds', 'transport_policy'];
  const capabilityNames = ['image_api_mode', 'image_main_model', 'video_api', 'video_models', 'result_origins', 'input_token_overhead_ceiling', 'stream_usage_contract'];
  if (props.fieldPathId.path.length === 0 && props.schema.properties?.name && props.schema.properties?.config) {
    return <div className="upstream-form-sections">
      <FormSection title={t('connection.identitySection')}>{props.properties.filter((field) => field.name !== 'config').map((field) => field.content)}</FormSection>
      {props.properties.find((field) => field.name === 'config')?.content}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'config') {
    // Unknown plugin fields stay visible. Required capability fields also stay
    // visible: disclosure must not hide an adapter's minimum configuration.
    const optionalNetwork = networkNames.filter((name) => !props.schema.required?.includes(name));
    const optionalCapabilities = capabilityNames.filter((name) => !props.schema.required?.includes(name));
    const network = props.properties.filter((field) => optionalNetwork.includes(field.name));
    const capabilities = props.properties.filter((field) => optionalCapabilities.includes(field.name));
    return <div className="upstream-form-sections">
      <FormSection title={t('connection.endpointSection')}>
        <ObjectFieldTemplate {...props} title="" properties={props.properties.filter((field) => !optionalNetwork.includes(field.name) && !optionalCapabilities.includes(field.name))} />
      </FormSection>
      {network.length > 0 && <AdvancedFormSection title={t('connection.advancedSection')} description={t('connection.advancedHint')} invalid={network.some((field) => Boolean(props.errorSchema?.[field.name]))}>{network.map((field) => field.content)}</AdvancedFormSection>}
      {capabilities.length > 0 && <AdvancedFormSection title={t('connection.capabilitiesSection')} description={t('connection.capabilitiesHint')} invalid={capabilities.some((field) => Boolean(props.errorSchema?.[field.name]))}>{capabilities.map((field) => field.content)}</AdvancedFormSection>}
    </div>;
  }
  return <ObjectFieldTemplate {...props} />;
}

export const upstreamFormTemplates = { ...schemaFormTemplates, ObjectFieldTemplate: UpstreamObjectTemplate };
