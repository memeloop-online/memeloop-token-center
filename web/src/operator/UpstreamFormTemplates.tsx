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
  const networkNames = ['network_scope', 'timeout_seconds', 'transport_policy', 'reservation_token_bounds', 'input_token_overhead_ceiling', 'quota_read_policy'];
  const capabilityNames = ['image_api_mode', 'image_main_model', 'video_api', 'video_models', 'result_origins', 'stream_usage_contract', 'responses_transport', 'responses_compact_v2_bridge', 'responses_via_chat_compaction', 'provider_asset_reads_repeatable'];
  if (props.fieldPathId.path.length === 0 && !props.registry.formContext?.providerConfigRoot && props.schema.properties?.name && props.schema.properties?.config) {
    const identity = props.properties.filter((field) => field.name !== 'config' && field.name !== 'credential');
    return <div className="upstream-form-sections">
      <FormSection title={props.registry.formContext?.providerIdentityTitle ?? t('connection.identitySection')}>{identity.map((field) => field.content)}{props.registry.formContext?.providerIdentity}</FormSection>
      {props.registry.formContext?.providerAuthentication}
      {props.properties.find((field) => field.name === 'config')?.content}
      {props.registry.formContext?.providerRouting}
      {props.properties.some(field => field.name === 'credential') && <FormSection title={copy.authentication} description={copy.authenticationHint}>{props.properties.find(field => field.name === 'credential')?.content}</FormSection>}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'credential') {
    const advancedNames = ['header', 'prefix'].filter(name => !props.schema.required?.includes(name));
    const fixedNames = ['type', 'proxy_network_scope'].filter(name => {
      const schema = props.schema.properties?.[name];
      return schema && typeof schema === 'object' && schema.const !== undefined && !props.errorSchema?.[name];
    });
    const advanced = props.properties.filter(field => !field.hidden && advancedNames.includes(field.name));
    const main = props.properties.filter(field => !fixedNames.includes(field.name) && (field.hidden || !advancedNames.includes(field.name)));
    return <div className="upstream-form-sections">
      <div hidden>{props.properties.filter(field => fixedNames.includes(field.name)).map(field => field.content)}</div>
      <ObjectFieldTemplate {...props} title="" description={undefined} properties={main} />
      {advanced.length > 0 && <AdvancedFormSection title={copy.advancedAuthentication} description={copy.advancedAuthenticationHint} invalid={advanced.some(field => Boolean(props.errorSchema?.[field.name]))}>{advanced.map(field => field.content)}</AdvancedFormSection>}
    </div>;
  }
  if (props.fieldPathId.path.at(-1) === 'config' || (props.fieldPathId.path.length === 0 && props.registry.formContext?.providerConfigRoot)) {
    if (props.registry.formContext?.providerEdit) {
      const primaryNames = ['base_url'];
      const retryNames = ['transport_policy', 'timeout_seconds'];
      const primary = props.properties.filter(field => !field.hidden && primaryNames.includes(field.name));
      const retries = props.properties.filter(field => !field.hidden && retryNames.includes(field.name));
      const advanced = props.properties.filter(field => !field.hidden && !primaryNames.includes(field.name) && !retryNames.includes(field.name));
      return <div className="upstream-form-sections">
        {props.properties.filter(field => field.hidden).map(field => field.content)}
        <FormSection title={props.registry.formContext.providerConnectionTitle ?? t('connection.endpointSection')}>
          {primary.map(field => field.content)}
          {props.registry.formContext.providerConnection}
        </FormSection>
        {retries.length > 0 && <AdvancedFormSection action title={copy.timeouts} description={copy.timeoutsHint} invalid={retries.some(field => Boolean(props.errorSchema?.[field.name]))}>{retries.map(field => field.content)}</AdvancedFormSection>}
        {(advanced.length > 0 || canExpand(props.schema, props.uiSchema, props.formData)) && <AdvancedFormSection title={copy.advancedConnection} description={copy.advancedConnectionHint} invalid={advanced.some(field => Boolean(props.errorSchema?.[field.name]))}><ObjectFieldTemplate {...props} title="" description={undefined} properties={advanced} /></AdvancedFormSection>}
      </div>;
    }
    // Unknown plugin fields stay visible. Required capability fields also stay
    // visible: disclosure must not hide an adapter's minimum configuration.
    const optionalNetwork = networkNames.filter((name) => !props.schema.required?.includes(name));
    const optionalCapabilities = capabilityNames.filter((name) => !props.schema.required?.includes(name));
    const network = props.properties.filter((field) => !field.hidden && optionalNetwork.includes(field.name));
    const capabilities = props.properties.filter((field) => !field.hidden && optionalCapabilities.includes(field.name));
    const main = props.properties.filter((field) => !field.hidden && !optionalNetwork.includes(field.name) && !optionalCapabilities.includes(field.name));
    return <div className="upstream-form-sections">
      {props.properties.filter(field => field.hidden).map(field => field.content)}
      {(main.length > 0 || canExpand(props.schema, props.uiSchema, props.formData) || props.registry.formContext?.providerConnection) && <FormSection title={props.registry.formContext?.providerConnectionTitle ?? t('connection.endpointSection')}>
        <ObjectFieldTemplate {...props} title="" description={undefined} properties={main} />
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
