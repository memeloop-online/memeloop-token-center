import type { ObjectFieldTemplateProps } from '@rjsf/utils';
import ObjectFieldTemplate from '@rjsf/core/lib/components/templates/ObjectFieldTemplate.js';
import { schemaFormTemplates } from '../SchemaTemplates';
import { useI18n } from '../i18n';
import { FormSection } from '../design-system';
import { JourneyDisclosure as AdvancedFormSection } from './JourneyDisclosure';
import { formJourneyCopy } from './formJourneyCopy';

function CredentialObjectTemplate(props: ObjectFieldTemplateProps) {
  const { locale } = useI18n();
  const copy = formJourneyCopy(locale);
  if (props.fieldPathId.path.length === 0 && (props.schema.properties?.policy || props.registry.formContext?.authorizationFields)) {
    const funding = props.properties.filter(field => ['currency', 'initial_balance'].includes(field.name) && !props.schema.required?.includes(field.name));
    return <div className="credential-form-journey">
      <FormSection title={copy.identity}>{props.properties.filter(field => field.name !== 'policy' && !funding.includes(field)).map(field => field.content)}</FormSection>
      {props.registry.formContext?.authorizationFields}
      {(props.schema.properties?.policy || funding.length > 0) && <AdvancedFormSection title={copy.policy} description={copy.policyHint} invalid={Boolean(props.errorSchema?.policy) || funding.some(field => Boolean(props.errorSchema?.[field.name]))}>
        {funding.map(field => field.content)}
        {props.properties.find(field => field.name === 'policy')?.content}
      </AdvancedFormSection>}
    </div>;
  }
  if (props.schema.properties?.enforcement_mode) {
    // Keep required and unknown plugin fields visible. Only known optional
    // limits can be collapsed, and server validation always reveals them.
    const limitNames = ['daily_budget', 'weekly_budget', 'lifetime_budget'];
    const optional = props.properties.filter(field => limitNames.includes(field.name) && !props.schema.required?.includes(field.name));
    return <div className="credential-policy-fields">
      <ObjectFieldTemplate {...props} title="" properties={props.properties.filter(field => !optional.includes(field))} />
      {optional.length > 0 && <AdvancedFormSection title={copy.budget} description={copy.budgetHint} invalid={optional.some(field => Boolean(props.errorSchema?.[field.name]))}>
        {optional.map(field => field.content)}
      </AdvancedFormSection>}
    </div>;
  }
  return <ObjectFieldTemplate {...props} />;
}

export const credentialFormTemplates = { ...schemaFormTemplates, ObjectFieldTemplate: CredentialObjectTemplate };
