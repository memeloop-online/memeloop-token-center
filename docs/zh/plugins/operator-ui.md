# Operator UI 扩展

插件可为 Operator 控制台添加侧栏页签和总览卡片。MTC 提供两条实现路径：

- `typed_data_v1`：清单声明数据源，由 MTC 设计系统渲染结构化数据。
- `component_v1`：受信任的 UI 包通过版本化 SDK 注册 React 组件，适合交互式工作台、可视化和完整业务流程。

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
        "component_id": "health-workspace",
        "component_props": { "defaultRange": "24h" },
        "data_endpoint": "health"
      }
    ]
  }
}
```

`operator.sidebar.tab` 需要 `route` 与 `category`；`operator.overview.card` 直接进入总览；`operator.page.before` 和 `operator.page.after` 通过 `target_route` 将组件插入现有 Operator 页面。核心分类包含 `monitoring`、`traffic`、`identity`、`system`，插件也可声明带名称的新分类。图标可选 `activity`、`chart`、`database`、`heart`、`plug`、`shield`。

## React 组件包

组件包导出 `operatorUiPackage`：

```tsx
import { defineOperatorUiPackage } from '@memeloop/token-center-operator-ui-sdk';
import { HealthWorkspace } from './HealthWorkspace';

export const operatorUiPackage = defineOperatorUiPackage({
  apiVersion: 'operator-ui-package-v1',
  pluginId: 'example-observability',
  compatiblePluginVersions: ['1.0.0'],
  components: { 'health-workspace': HealthWorkspace },
});
```

组件收到当前插件、租户、语言、插槽信息和宿主 API。宿主 API 提供：

- `loadServiceData(endpointId)`：读取清单中的服务数据源。
- `request(path, options)`：调用当前 Operator 凭据有权访问的 MTC API。
- `navigate(route)`：进入核心页面或已安装插件页面。

组件可直接使用 React、Fluent UI、图表库及自身状态管理。MTC 的主题变量与 Fluent Provider 会自动覆盖组件区域。

构建时通过 `MTC_OPERATOR_UI_PACKAGES` 注册包，多个包以逗号分隔：

```bash
MTC_OPERATOR_UI_PACKAGES=@memeloop/health-plugin-ui,@memeloop/routing-plugin-ui npm run build
```

清单负责启用页面，构建包负责提供组件。插件卸载或版本变化后，宿主会同步更新页签、卡片和路由。

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

构建期模块解析让组件包进入常规 TypeScript、依赖锁定、代码审查和内容安全策略流程。运行时清单负责选择已编译组件，页面加载过程保持稳定且可追踪。
