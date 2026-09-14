// Page orchestration copy lives alongside the composition, not in schema data.
export function formJourneyCopy(locale: string) {
  return locale.startsWith('zh') ? {
    authentication: '认证凭据', authenticationHint: '仅填写所选提供商需要的凭据。网络代理与 API 地址分开配置。',
    identity: '身份与归属', policy: '用量与预算', policyHint: '先选择计费方式；只有需要限制用量时再设置预算。',
    budget: '可选用量限制', budgetHint: '保持原有默认值表示沿用当前策略；展开后可调整具体限制。',
    candidates: '按上游组选择与排除', candidatesHint: '直接选择账号即可开始。需要动态管理候选账号时，再配置上游组；排除优先。',
    priority: '路由优先级', preview: '配置预览', noModel: '尚未选择模型', noUpstream: '尚未选择上游',
    access: '凭据授权', noGrant: '未新增凭据授权；不会自动向所有凭据开放。',
    codexProxy: '此地址仅标识服务协议端点。所有网络连接必须经过此账号独立的 socks5h 代理，并在代理端解析域名；未配置有效代理时无法继续，绝不会回退直连。',
    draftNoRoutes: '尚未授权模型访问；可创建后再配置路由。',
    credentialFlowHint: '填写身份，按需设置用量，再授予模型访问权限。',
    concurrentDraftPreserved: '配置已被其他人修改。你的草稿已保留；请关闭并重新打开编辑，核对最新配置后重试。',
    routeAccess: '3. 模型访问授权', protocolHelp: '协议兼容性说明',
  } : {
    authentication: 'Authentication', authenticationHint: 'Use credentials for the selected provider. Configure the network proxy separately from the API endpoint.',
    identity: 'Identity and ownership', policy: 'Usage and budget', policyHint: 'Choose billing behavior first; add budgets only when you need usage limits.',
    budget: 'Optional usage limits', budgetHint: 'Unchanged values retain the current policy. Expand to adjust individual limits.',
    candidates: 'Include or exclude upstream groups', candidatesHint: 'Select accounts to start. Use groups for a dynamic candidate set; exclusions take precedence.',
    priority: 'Routing priority', preview: 'Configuration preview', noModel: 'No model selected', noUpstream: 'No upstream selected',
    access: 'Credential access', noGrant: 'No new credential grants; this does not automatically grant access to every credential.',
    codexProxy: 'This is the service protocol endpoint. All connections must use this account’s separate socks5h proxy with remote DNS. A valid proxy is required; direct fallback is never allowed.',
    draftNoRoutes: 'No model access selected. You can configure routes after creation.',
    credentialFlowHint: 'Name the credential, adjust usage settings if needed, then grant model access.',
    concurrentDraftPreserved: 'Someone changed this configuration. Your draft is preserved; close and reopen the editor to review the latest version before retrying.',
    routeAccess: '3. Model access', protocolHelp: 'Protocol compatibility details',
  };
}
