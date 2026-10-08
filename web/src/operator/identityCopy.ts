export function identityCopy(locale: string) {
  return locale.startsWith('zh') ? {
    title: '身份管理', description: '管理租户及访问管理接口的服务凭据。客户端凭据用于调用模型，在独立页面管理。',
    tenants: '租户', services: '服务凭据', leaveDraft: '身份管理中的表单修改尚未保存。仍要离开当前页面吗？',
  } : {
    title: 'Identity management', description: 'Manage tenants and service credentials for management API access. Client credentials for model requests have their own page.',
    tenants: 'Tenants', services: 'Service credentials', leaveDraft: 'Your identity management form changes are not saved. Leave the current page anyway?',
  };
}
