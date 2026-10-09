import { deepEquals, type ValidatorType } from '@rjsf/utils';
import type { UpstreamAccount } from '../types.js';

export function providerSettingsConfigChanged(initialConfig: unknown, formData: Record<string, unknown>) {
  return !deepEquals(initialConfig, formData.config);
}

export function providerSettingsValidator(base: ValidatorType, initialConfig: unknown): ValidatorType {
  return {
    rawValidation: base.rawValidation.bind(base),
    isValid: base.isValid.bind(base),
    validateFormData(formData, schema, customValidate, transformErrors, uiSchema) {
      const validateConfig = !formData || typeof formData !== 'object'
        || providerSettingsConfigChanged(initialConfig, formData as Record<string, unknown>);
      const validationSchema = validateConfig ? schema : {
        ...schema, properties: { ...schema.properties, config: true },
      };
      return base.validateFormData(formData, validationSchema, customValidate, transformErrors, uiSchema);
    },
  };
}

export function providerSettingsUpdate(formData: Record<string, unknown>, initialConfig: unknown, tenant: string, revision: number) {
  return {
    name: formData.name,
    tenant_external_id: tenant,
    expected_updated_at: revision,
    ...(providerSettingsConfigChanged(initialConfig, formData) ? { config: formData.config } : {}),
  };
}

export function mergeProviderRenameReceipt(account: UpstreamAccount, response: unknown, tenant: string, requestedName: unknown): UpstreamAccount | undefined {
  if (!response || typeof response !== 'object' || Array.isArray(response) || typeof requestedName !== 'string') return undefined;
  const receipt = response as Record<string, unknown>;
  if (receipt.id !== account.id || typeof receipt.tenant_id !== 'string' || receipt.tenant_id !== account.tenant_id
    || receipt.tenant_external_id !== tenant || (account.tenant_external_id && account.tenant_external_id !== tenant)
    || typeof receipt.name !== 'string' || !receipt.name.trim() || receipt.name !== requestedName.trim()
    || typeof receipt.updated_at !== 'number' || !Number.isSafeInteger(receipt.updated_at) || receipt.updated_at < account.updated_at) return undefined;
  return { ...account, name: receipt.name, updated_at: receipt.updated_at };
}
