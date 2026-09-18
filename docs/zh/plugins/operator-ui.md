# Operator UI 扩展

插件可为 Operator 控制台添加侧栏页签和总览卡片。MTC 提供两条实现路径：

- `typed_data_v1`：清单声明数据源，由 MTC 设计系统渲染结构化数据。
- `component_v1`：从已安装且完成签名校验的插件包加载摘要寻址 React 模块，适合交互式工作台、可视化和完整业务流程。

清单契约位于 [plugin-manifest.schema.json](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)，React 契约位于 `web/operator-ui-sdk`。

## 清单示例

```json
{
  "capabilities": [
    { "kind": "http", "allowed_origins": ["https://plugin-api.example.com"] }
  ],
  "contributions": {
    "service_data": [
      {
        "id": "health",
        "url": "https://plugin-api.example.com/v1/health",
        "required_scope": "metrics:read",
        "response_schema": {
          "type": "object",
          "required": ["status"],
          "properties": { "status": { "type": "string" } }
        },
        "fallback": { "status": "offline" }
      }
    ],
    "operator_ui": [
      {
        "id": "health-tab",
        "slot": "operator.sidebar.tab",
        "category": { "id": "monitoring" },
        "route": "plugin-health",
        "label": "服务健康",
        "icon": "heart",
        "renderer": "component_v1",
        "module_entry": "assets/operator-ui.mjs",
        "component_id": "health-workspace",
        "component_props": { "defaultRange": "24h" },
        "data_endpoint": "health"
      }
    ]
  }
}
```

`operator.sidebar.tab` 需要 `route` 与 `category`；`operator.overview.card` 直接进入总览；`operator.page.before` 和 `operator.page.after` 通过 `target_route` 将组件插入现有 Operator 页面。核心分类包含 `monitoring`、`traffic`、`identity`、`system`，插件也可声明带名称的新分类。图标可选 `activity`、`chart`、`database`、`heart`、`plug`、`shield`。

## React 模块

签名 OCI 制品携带一个自包含 ESM 文件。激活函数接收 MTC 当前使用的 React 与 Fluent 运行时，宿主与插件共享同一 React 实例：

```js
export function activateOperatorUi({ React, Fluent, defineOperatorUiPackage }) {
  function HealthWorkspace({ api, contribution }) {
    return React.createElement(Fluent.Text, null, contribution.label);
  }
  return defineOperatorUiPackage({
    apiVersion: 'operator-ui-package-v1',
    pluginId: 'example-observability',
    compatiblePluginVersions: ['1.0.0'],
    components: { 'health-workspace': HealthWorkspace },
  });
}
```

组件收到当前插件、租户、语言、插槽信息和宿主 API。宿主 API 提供：

- `loadServiceData(endpointId)`：读取清单中的服务数据源。
- `navigate(route)`：进入核心页面或已安装插件页面。

组件使用宿主提供的 React 与 Fluent UI，也可把其他浏览器库打包进单个 ESM 文件。MTC 的主题变量与 Fluent Provider 会覆盖组件区域。

发布时把模块作为插件资产层与 `plugin.json` 一起放入 OCI 制品：

```bash
oras push --artifact-type application/vnd.memeloop.token-center.plugin.v1 \
  --config artifact-config.json:application/vnd.memeloop.token-center.plugin.config.v1+json \
  ghcr.io/example/example-observability:1.0.0 \
  plugin.json:application/vnd.memeloop.token-center.plugin.manifest.v1+json \
  assets/operator-ui.mjs:application/vnd.memeloop.token-center.plugin.asset.v1
```

安装流程校验固定 OCI 摘要与签名。活动运行时保存模块的准确字节，在插件目录中发布 SHA-256，并通过不可变同源地址加载。发布、回滚或卸载插件后，目录刷新会同步更新页签和页面插槽，无需重新构建 MTC。每个贡献都有独立加载状态和错误边界。

## 结构化数据路径

轻量页面可选 `typed_data_v1`。`data_endpoint` 指向同一清单的 `service_data`，`presentation` 可选通用键值视图、`health_intelligence_v1` 或 `projection_v1`。`projection_v1` 支持文本、指标、状态和链接组件，详情见 [UI 投影 Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-ui-projection.schema.json)。

服务数据由控制面读取并校验，返回统一信封：

```json
{
  "data": { "status": "healthy" },
  "partial": false,
  "provenance": {
    "plugin_id": "example-observability",
    "endpoint_id": "health",
    "origin": "https://plugin-api.example.com",
    "fetched_at": 1780000000000,
    "source": "network"
  }
}
```

`component_v1` 模块随签名插件制品进入 MTC 内容安全策略。首版每个入口使用一个 ESM 文件；后续 SDK 可在保持内容寻址机制的前提下扩展分块清单。
