export function providerConnectionCopy(locale: string) {
  return locale.startsWith('zh') ? {
    identity: '账号', network: '网络与连接', authentication: '认证', routing: '路由设置',
    readFailed: '无法读取当前代理地址。请重新打开编辑；若仍失败，请确认管理权限。',
    noProxy: '未配置网络代理', copyProxy: '复制代理地址', copyEndpoint: '复制服务地址',
    viewProxy: '查看代理地址', hideProxy: '隐藏代理地址',
    readOnlyProxy: '此账号暂不支持单独修改代理地址。',
    routingHint: '模型路由在模型管理中配置；这里的连接设置对该账号的所有路由生效。',
    openRoutes: '管理模型路由', discard: '当前修改尚未保存。放弃修改并继续？',
  } : {
    identity: 'Account', network: 'Network and connection', authentication: 'Authentication', routing: 'Routing settings',
    readFailed: 'Cannot read the current proxy address. Reopen the editor; if this persists, check management permissions.',
    noProxy: 'No network proxy configured', copyProxy: 'Copy proxy address', copyEndpoint: 'Copy service address',
    viewProxy: 'Show proxy address', hideProxy: 'Hide proxy address',
    readOnlyProxy: 'This account does not yet support separate proxy updates.',
    routingHint: 'Configure model routes in Models. These connection settings apply to every route using this account.',
    openRoutes: 'Manage model routes', discard: 'Your changes have not been saved. Discard them and continue?',
  };
}
