import { canExpand, type ObjectFieldTemplateProps } from '@rjsf/utils';
import ObjectFieldTemplate from '@rjsf/core/lib/components/templates/ObjectFieldTemplate.js';
import { schemaFormTemplates } from '../SchemaTemplates';
import { useI18n } from '../i18n';
import { FormSection } from '../design-system';
import { JourneyDisclosure as AdvancedFormSection } from './JourneyDisclosure';
import { formJourneyCopy } from './formJourneyCopy';
import { AdditionalPropertyTemplate, AddPropertyButton } from './AdditionalPropertyTemplate';
import './operatorFormSurfaces.css';
import './formJourney.css';

function UpstreamObjectTemplate(props: ObjectFieldTemplateProps) {
  const { t, locale } = useI18n();
  const copy = formJourneyCopy(locale);
  const networkNames = ['network_scope', 'timeout_seconds', 'transport_policy'];
  const capabilityNames = ['image_api_mode', 'image_main_model', 'video_api', 'video_models', 'result_origins', 'input_token_overhead_ceiling', 'stream_usage_contract'];
  if (props.fieldPathId.path.length === 0 && props.schema.properties?.name && props.schema.properties?.config) {
    const identity = props.properties.filter((field) => field.name !== 'config' && field.name !== 'credential');
    return <div className="upstream-form-sections">
      <FormSection title={props.registry.formContext?.providerIdentityTitle ?? t('connection.identitySection')}>{identity.map((field) => field.content)}</FormSection>
      {props.registry.formContext?.providerAuthentication}
      {props.properties.find((field) => field.name === 'config')?.content}
      {props.registry.formContext?.providerRouting}
      {props.properties.some(field => field.name === 'credential') && <FormSection title={copy.authentication} description={copy.authenticationHint}>{props.properties.find(field => field.name === 'credential')?.content}</FormSection>}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'config') {
    if (props.registry.formContext?.providerEdit) {
      const primaryNames = ['base_url'];
      const retryNames = ['transport_policy', 'timeout_seconds'];
      const primary = props.properties.filter(field => !field.hidden && primaryNames.includes(field.name));
      const retries = props.properties.filter(field => !field.hidden && retryNames.includes(field.name));
      const advanced = props.properties.filter(field => !field.hidden && !primaryNames.includes(field.name) && !retryNames.includes(field.name));
      const zh = locale.startsWith('zh');
      return <div className="upstream-form-sections">
        {props.properties.filter(field => field.hidden).map(field => field.content)}
        <FormSection title={props.registry.formContext.providerConnectionTitle ?? t('connection.endpointSection')}>
          {primary.map(field => field.content)}
          {props.registry.formContext.providerConnection}
        </FormSection>
        {retries.length > 0 && <AdvancedFormSection action title={zh ? '配置超时与重试' : 'Configure timeouts and retries'} description={zh ? '调整连接、读取和故障切换时限；日常修改代理无需调整。' : 'Connection, read and failover limits. Changing a proxy does not require adjusting these.'} invalid={retries.some(field => Boolean(props.errorSchema?.[field.name]))}>{retries.map(field => field.content)}</AdvancedFormSection>}
        {(advanced.length > 0 || canExpand(props.schema, props.uiSchema, props.formData)) && <AdvancedFormSection title={zh ? '高级配置与模型预留' : 'Advanced settings and model reservations'} description={zh ? '网络范围、模型预留及扩展配置。现有字段完整保留；仅在明确了解影响时修改。' : 'Network scope, model reservations and extensions. Existing fields are preserved; change only when you understand their effects.'} invalid={advanced.some(field => Boolean(props.errorSchema?.[field.name]))}><ObjectFieldTemplate {...props} title="" description={undefined} properties={advanced} /></AdvancedFormSection>}
      </div>;
    }
    // Unknown plugin fields stay visible. Required capability fields also stay
    // visible: disclosure must not hide an adapter's minimum configuration.
    const optionalNetwork = networkNames.filter((name) => !props.schema.required?.includes(name));
    const optionalCapabilities = capabilityNames.filter((name) => !props.schema.required?.includes(name));
    const network = props.properties.filter((field) => optionalNetwork.includes(field.name));
    const capabilities = props.properties.filter((field) => optionalCapabilities.includes(field.name));
    const main = props.properties.filter((field) => !field.hidden && !optionalNetwork.includes(field.name) && !optionalCapabilities.includes(field.name));
    return <div className="upstream-form-sections">
      {props.properties.filter(field => field.hidden).map(field => field.content)}
      {(main.length > 0 || props.registry.formContext?.providerConnection) && <FormSection title={props.registry.formContext?.providerConnectionTitle ?? t('connection.endpointSection')}>
        {main.length > 0 && <ObjectFieldTemplate {...props} title="" properties={main} />}
        {props.registry.formContext?.providerConnection}
      </FormSection>}
      {network.length > 0 && <AdvancedFormSection title={t('connection.advancedSection')} description={t('connection.advancedHint')} invalid={network.some((field) => Boolean(props.errorSchema?.[field.name]))}>{network.map((field) => field.content)}</AdvancedFormSection>}
      {capabilities.length > 0 && <AdvancedFormSection title={t('connection.capabilitiesSection')} description={t('connection.capabilitiesHint')} invalid={capabilities.some((field) => Boolean(props.errorSchema?.[field.name]))}>{capabilities.map((field) => field.content)}</AdvancedFormSection>}
    </div>;
  }
  return <ObjectFieldTemplate {...props} />;
}

export const upstreamFormTemplates = { ...schemaFormTemplates, ObjectFieldTemplate: UpstreamObjectTemplate,
  WrapIfAdditionalTemplate: AdditionalPropertyTemplate, ButtonTemplates: { AddButton: AddPropertyButton } };
