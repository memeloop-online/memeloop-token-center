import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';

export type Locale = 'zh-CN' | 'en';
type Variables = Record<string, string | number>;

const zh: Record<string, string> = {
  'language.zh': '中文', 'language.en': 'English',
  'theme.light': '切换到亮色主题', 'theme.dark': '切换到暗色主题',
  'common.connect': '连接', 'common.load': '载入', 'common.none': '无', 'common.running': '运行中',
  'common.select': '请选择', 'common.back': '返回', 'common.openAuthorization': '打开授权页',
  'common.checkAuthorization': '检查授权结果', 'common.startLogin': '开始登录', 'common.save': '保存',
  'common.noData': '暂无数据', 'common.noRequests': '暂无请求', 'common.oneTime': '仅显示一次',
  'common.requestFailed': '请求失败', 'common.connectionFailed': '管理 API 请求失败',
  'common.scopeWarning': '当前凭据有 {{count}} 项管理资源无读取权限；其余只读数据已加载。',
  'nav.traffic': '实时请求', 'nav.providers': '上游提供商', 'nav.routes': '模型路由',
  'nav.pricing': '模型计费', 'nav.credentials': '创建凭据', 'nav.services': '服务凭据', 'nav.plugins': '插件',
  'operator.subtitle': '统一管理上游提供商、接入方式、路由、计费与流量诊断。',
  'operator.tokenPlaceholder': 'mts_… 服务凭据', 'operator.tenant': '租户范围', 'operator.allTenants': '全部租户',
  'traffic.total': '总请求', 'traffic.success': '成功', 'traffic.failure': '失败', 'traffic.cost': '总费用',
  'traffic.models': '模型分布', 'traffic.days': '每日趋势', 'traffic.errors': '错误分布',
  'traffic.live': '实时请求尾流', 'traffic.liveHint': 'SSE 亚秒级尾查 · 点击加载归档',
  'traffic.streamDisconnected': '实时请求流已断开', 'traffic.detailFailed': '无法读取请求归档',
  'request.time': '时间', 'request.model': '模型', 'request.protocol': '协议', 'request.status': '状态',
  'request.duration': '耗时', 'request.tokens': 'Tokens', 'request.cost': '费用', 'request.count': '{{count}} 次',
  'request.archiveComplete': '归档完整', 'request.archiveIncomplete': '存在归档缺口',
  'request.error': '错误', 'request.request': '请求', 'request.response': '响应',
  'providers.title': '统一上游', 'providers.description': 'API 凭据、OAuth 和订阅桥接只是同一上游提供商的不同接入方式；创建后统一出现在左侧。',
  'providers.empty': '暂无上游提供商', 'providers.add': '新增上游', 'providers.method': '接入方式',
  'providers.direct': '直接凭据', 'providers.oauth': 'OAuth / 订阅授权', 'providers.provider': '提供商',
  'providers.name': '上游名称', 'providers.create': '添加上游', 'providers.schemaMissing': '连接管理 API 后加载配置 Schema',
  'providers.authKind': '认证', 'providers.generation': '凭据代次', 'providers.subscription': 'CPA Subscription Bridge',
  'providers.cursorDirect': 'Cursor 直接 PKCE', 'providers.pluginAdapter': '插件 OAuth Adapter',
  'providers.subscriptionProvider': '订阅提供商', 'providers.bridgeSecret': 'Bridge Secret（可选）',
  'providers.waiting': '仍在等待授权', 'providers.ready': '上游 {{id}} 已就绪',
  'providers.noAdapter': '当前没有插件贡献 OAuth Adapter。',
  'providers.oauthSecurity': '授权状态和接入凭据加密保存，并在同一稳定上游身份下按代次轮换。',
  'routes.title': '创建模型路由', 'routes.publicModel': '公开模型', 'routes.upstream': '上游提供商',
  'routes.upstreamModel': '上游模型', 'routes.protocol': '协议', 'routes.generation': '异步多模态生成',
  'routes.create': '创建路由', 'routes.created': '路由已创建',
  'pricing.title': '模型计费', 'pricing.description': '按 CPA 的规则从 models.dev、LiteLLM、OpenRouter 依次同步；仅唯一强匹配会自动保存，手动价格作为高级覆盖保留。',
  'pricing.sync': '一键同步价格', 'pricing.syncing': '正在同步…', 'pricing.synced': '已同步 {{count}} 个模型',
  'pricing.usedModels': '{{count}} 个已使用模型', 'pricing.saved': '{{count}} 个已有价格',
  'pricing.sourceOrder': '优先顺序', 'pricing.sourceHealthy': '{{count}} 个模型', 'pricing.sourceFailed': '本次不可用，保留旧价格',
  'pricing.model': '模型', 'pricing.calls': '请求数', 'pricing.input': '输入 / 1M', 'pricing.output': '输出 / 1M',
  'pricing.source': '来源', 'pricing.updated': '更新时间', 'pricing.missing': '待同步', 'pricing.noPrices': '尚无已保存价格',
  'pricing.result': '同步结果', 'pricing.imported': '导入 {{count}}', 'pricing.candidates': '待确认 {{count}}',
  'pricing.unmatched': '未匹配 {{count}}', 'pricing.preserved': '保留 {{count}}', 'pricing.manual': '手动覆盖 / 多模态价格',
  'pricing.manualHint': '仅用于公开数据源无法覆盖的内部模型，以及图片、视频、工作流的按任务计费。',
  'pricing.type': '类型', 'pricing.tokenModel': 'Token 模型', 'pricing.generationModel': '多模态生成',
  'pricing.currency': '币种', 'pricing.save': '保存手动价格', 'pricing.savedMessage': '价格已保存',
  'credentials.title': '创建下游凭据', 'credentials.description': '凭据可轮换；稳定主键、权限、额度与历史记录不会迁移或丢失。',
  'credentials.create': '创建凭据', 'credentials.created': '新凭据仅显示一次，请立即保存。',
  'services.title': '创建服务凭据', 'services.description': '为 memeloop web 等内部调用者分配最小权限；绑定租户后不能跨租户。',
  'services.create': '创建服务凭据',
  'plugins.title': '已加载插件', 'plugins.empty': '当前未挂载插件',
  'self.title': '请求与用量', 'self.subtitle': '凭当前客户端凭据只读查看稳定身份、历史、错误和逻辑会话。',
  'self.placeholder': 'CPA 原凭据或 mtc_…', 'self.balance': '可用余额 ({{currency}})',
  'self.stableCredential': '稳定凭据', 'self.concurrency': '并发', 'self.allowedModels': '模型',
  'self.recent': '最近请求', 'self.conversations': '逻辑对话', 'self.conversationHint': 'Merkle 前缀 · 压缩/重试/分支',
  'self.noConversations': '暂无可关联的逻辑对话', 'self.inferred': '推断会话', 'self.sequence': '请求序列',
  'self.edges': '关系边', 'self.singleObservation': '当前仅有一个观察点',
  'self.generations': '多模态生成任务', 'self.integration': '接入', 'self.units': '计费单位',
  'self.noGenerations': '暂无生成任务', 'self.billing': '计费', 'self.resultArchive': '结果与归档',
  'self.detailFailed': '无法读取请求详情',
  'schema.Downstream credential': '下游凭据', 'schema.Tenant': '租户', 'schema.Principal': '用户主体',
  'schema.Credential alias': '凭据别名', 'schema.Initial credit': '初始额度', 'schema.Policy': '权限与限流策略',
  'schema.Allowed models': '允许模型', 'schema.Credential policy': '凭据策略', 'schema.Requests per minute': '每分钟请求数',
  'schema.Tokens per minute': '每分钟 Token 数', 'schema.Maximum concurrency': '最大并发数',
  'schema.Token model price': 'Token 模型价格', 'schema.Generation price': '多模态生成价格',
  'schema.Service credential': '服务凭据', 'schema.Restrict to tenant': '限制到租户',
  'schema.Leave empty only for a global operator integration.': '仅全局管理集成可以留空。',
};

const en: Record<string, string> = {
  ...Object.fromEntries(Object.keys(zh).map((key) => [key, key])),
  'language.zh': '中文', 'language.en': 'English', 'theme.light': 'Switch to light theme', 'theme.dark': 'Switch to dark theme',
  'common.connect': 'Connect', 'common.load': 'Load', 'common.none': 'None', 'common.running': 'Running', 'common.select': 'Select',
  'common.back': 'Back', 'common.openAuthorization': 'Open authorization', 'common.checkAuthorization': 'Check authorization',
  'common.startLogin': 'Start login', 'common.save': 'Save', 'common.noData': 'No data', 'common.noRequests': 'No requests',
  'common.oneTime': 'Shown once', 'common.requestFailed': 'Request failed', 'common.connectionFailed': 'Management API request failed',
  'common.scopeWarning': '{{count}} management resources are unavailable to this credential; other readable data was loaded.',
  'nav.traffic': 'Live traffic', 'nav.providers': 'Upstream providers', 'nav.routes': 'Model routes', 'nav.pricing': 'Model pricing',
  'nav.credentials': 'Create credential', 'nav.services': 'Service credentials', 'nav.plugins': 'Plugins',
  'operator.subtitle': 'Manage upstream providers, connection methods, routing, pricing, and traffic diagnostics in one place.',
  'operator.tokenPlaceholder': 'mts_… service credential', 'operator.tenant': 'Tenant scope', 'operator.allTenants': 'All tenants',
  'traffic.total': 'Total requests', 'traffic.success': 'Successful', 'traffic.failure': 'Failed', 'traffic.cost': 'Total cost',
  'traffic.models': 'Models', 'traffic.days': 'Daily trend', 'traffic.errors': 'Errors', 'traffic.live': 'Live request tail',
  'traffic.liveHint': 'Sub-second SSE tail · select a row to load its archive', 'traffic.streamDisconnected': 'Live request stream disconnected',
  'traffic.detailFailed': 'Could not load the request archive',
  'request.time': 'Time', 'request.model': 'Model', 'request.protocol': 'Protocol', 'request.status': 'Status', 'request.duration': 'Duration',
  'request.tokens': 'Tokens', 'request.cost': 'Cost', 'request.count': '{{count}} requests', 'request.archiveComplete': 'Archive complete',
  'request.archiveIncomplete': 'Archive incomplete', 'request.error': 'Error', 'request.request': 'Request', 'request.response': 'Response',
  'providers.title': 'Unified upstreams', 'providers.description': 'API credentials, OAuth, and subscription bridges are connection methods for the same upstream provider. Every result is managed in one list.',
  'providers.empty': 'No upstream providers', 'providers.add': 'Add upstream', 'providers.method': 'Connection method',
  'providers.direct': 'Direct credential', 'providers.oauth': 'OAuth / subscription authorization', 'providers.provider': 'Provider',
  'providers.name': 'Upstream name', 'providers.create': 'Add upstream', 'providers.schemaMissing': 'Connect to load configuration schemas',
  'providers.authKind': 'Authentication', 'providers.generation': 'Credential generation', 'providers.subscription': 'CPA Subscription Bridge',
  'providers.cursorDirect': 'Cursor direct PKCE', 'providers.pluginAdapter': 'Plugin OAuth adapter',
  'providers.subscriptionProvider': 'Subscription provider', 'providers.bridgeSecret': 'Bridge secret (optional)',
  'providers.waiting': 'Waiting for authorization', 'providers.ready': 'Upstream {{id}} is ready', 'providers.noAdapter': 'No plugin OAuth adapter is loaded.',
  'providers.oauthSecurity': 'Authorization state and credentials are encrypted and rotate by generation under one stable upstream identity.',
  'routes.title': 'Create model route', 'routes.publicModel': 'Public model', 'routes.upstream': 'Upstream provider',
  'routes.upstreamModel': 'Upstream model', 'routes.protocol': 'Protocol', 'routes.generation': 'Asynchronous multimodal generation',
  'routes.create': 'Create route', 'routes.created': 'Route created',
  'pricing.title': 'Model pricing', 'pricing.description': 'Syncs models.dev, LiteLLM, then OpenRouter like CPA. Only unique strong matches are saved; manual prices remain advanced overrides.',
  'pricing.sync': 'Sync prices', 'pricing.syncing': 'Syncing…', 'pricing.synced': 'Synced {{count}} models',
  'pricing.usedModels': '{{count}} used models', 'pricing.saved': '{{count}} saved prices', 'pricing.sourceOrder': 'Priority order',
  'pricing.sourceHealthy': '{{count}} models', 'pricing.sourceFailed': 'Unavailable; retained last-known prices',
  'pricing.model': 'Model', 'pricing.calls': 'Requests', 'pricing.input': 'Input / 1M', 'pricing.output': 'Output / 1M',
  'pricing.source': 'Source', 'pricing.updated': 'Updated', 'pricing.missing': 'Missing', 'pricing.noPrices': 'No saved prices',
  'pricing.result': 'Sync result', 'pricing.imported': 'Imported {{count}}', 'pricing.candidates': 'Candidates {{count}}',
  'pricing.unmatched': 'Unmatched {{count}}', 'pricing.preserved': 'Preserved {{count}}', 'pricing.manual': 'Manual override / multimodal pricing',
  'pricing.manualHint': 'Use this only for internal models absent from public sources and per-job image, video, or workflow pricing.',
  'pricing.type': 'Type', 'pricing.tokenModel': 'Token model', 'pricing.generationModel': 'Multimodal generation',
  'pricing.currency': 'Currency', 'pricing.save': 'Save manual price', 'pricing.savedMessage': 'Price saved',
  'credentials.title': 'Create client credential', 'credentials.description': 'Credentials rotate without migrating or losing the stable ID, policy, quota, or history.',
  'credentials.create': 'Create credential', 'credentials.created': 'The new credential is shown once. Save it now.',
  'services.title': 'Create service credential', 'services.description': 'Grant least privilege to memeloop web and other internal callers. Tenant binding prevents cross-tenant access.',
  'services.create': 'Create service credential', 'plugins.title': 'Loaded plugins', 'plugins.empty': 'No plugins loaded',
  'self.title': 'Requests and usage', 'self.subtitle': 'Use a client credential for read-only access to its stable identity, history, errors, and logical conversations.',
  'self.placeholder': 'Legacy CPA credential or mtc_…', 'self.balance': 'Available balance ({{currency}})', 'self.stableCredential': 'Stable credential',
  'self.concurrency': 'Concurrency', 'self.allowedModels': 'Models', 'self.recent': 'Recent requests', 'self.conversations': 'Logical conversations',
  'self.conversationHint': 'Merkle prefix · compaction/retry/branch', 'self.noConversations': 'No linked logical conversations',
  'self.inferred': 'Inferred conversation', 'self.sequence': 'Request sequence', 'self.edges': 'Relationship edges',
  'self.singleObservation': 'Only one observation is available', 'self.generations': 'Multimodal generation jobs', 'self.integration': 'Integration',
  'self.units': 'Billing units', 'self.noGenerations': 'No generation jobs', 'self.billing': 'Billing', 'self.resultArchive': 'Result and archive',
  'self.detailFailed': 'Could not load request details',
  'schema.Downstream credential': 'Client credential', 'schema.Tenant': 'Tenant', 'schema.Principal': 'Principal',
  'schema.Credential alias': 'Credential alias', 'schema.Initial credit': 'Initial credit', 'schema.Policy': 'Policy and rate limits',
  'schema.Allowed models': 'Allowed models', 'schema.Credential policy': 'Credential policy', 'schema.Requests per minute': 'Requests per minute',
  'schema.Tokens per minute': 'Tokens per minute', 'schema.Maximum concurrency': 'Maximum concurrency',
  'schema.Token model price': 'Token model price', 'schema.Generation price': 'Generation price',
  'schema.Service credential': 'Service credential', 'schema.Restrict to tenant': 'Restrict to tenant',
  'schema.Leave empty only for a global operator integration.': 'Leave empty only for a global operator integration.',
};

interface I18nValue { locale: Locale; setLocale: (locale: Locale) => void; t: (key: string, variables?: Variables) => string }
const I18nContext = createContext<I18nValue | undefined>(undefined);

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocale] = useState<Locale>(() => localStorage.getItem('mtc-locale') === 'en' ? 'en' : 'zh-CN');
  useEffect(() => {
    localStorage.setItem('mtc-locale', locale);
    document.documentElement.lang = locale;
  }, [locale]);
  const value = useMemo<I18nValue>(() => ({
    locale,
    setLocale,
    t: (key, variables) => {
      const template = (locale === 'zh-CN' ? zh : en)[key] ?? en[key] ?? key;
      return Object.entries(variables ?? {}).reduce((text, [name, replacement]) =>
        text.replaceAll(`{{${name}}}`, String(replacement)), template);
    },
  }), [locale]);
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n() {
  const value = useContext(I18nContext);
  if (!value) throw new Error('useI18n must be used inside I18nProvider');
  return value;
}

export function localizeSchema<T>(schema: T, locale: Locale): T {
  if (locale === 'en' || !schema || typeof schema !== 'object') return schema;
  if (Array.isArray(schema)) return schema.map((item) => localizeSchema(item, locale)) as T;
  const source = schema as Record<string, unknown>;
  const localized: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(source)) {
    if ((key === 'title' || key === 'description') && typeof value === 'string') {
      localized[key] = zh[`schema.${value}`] ?? value;
    } else {
      localized[key] = localizeSchema(value, locale);
    }
  }
  return localized as T;
}
