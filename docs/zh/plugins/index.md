# 插件概述

MTC 插件是**版本化的 WebAssembly 组件**（Component Model），不是动态加载的原生库。每个插件包包含一份 `plugin.json` 清单、可选的 `.wasm` 组件、README 与图标。清单声明扩展点（contributions）与能力（capabilities），主机在明确的边界内执行组件。

权威契约：

- [WIT 接口定义 `token-center.wit`](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)
- [plugin.json 清单 Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)
- 管理端点见 [OpenAPI](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml) 的 `/internal/v1/plugins*` 与 `/internal/v1/plugin-runtime*`

## 能力模型

插件能做什么完全由清单中的 `capabilities` 决定：

| 能力 | 含义 |
| --- | --- |
| `log` | 产生有界的宿主日志事件（不含插件原文） |
| `kv` | 按插件命名空间隔离的键值存储 |
| `http` | 仅访问清单精确列出的 origin，禁止重定向，请求/响应有界 |
| `group_routing_quota` | 组路由插件读取已授权候选的额度窗口快照（见[组路由](routing.md)） |

主机为每次调用提供有界的 fuel、内存与执行期限。核心系统始终保留凭证注入、目的地校验、授权、定价、配额、记账、归档与错误净化——组件永远看不到上游凭证材料，也不能绕过模型权限、余额、限流或审计。

## 安装与发布

插件包以签名 OCI 制品分发（打包方法见[插件开发](development.md)）。运营者在 Operator 控制台的 **Plugins** 页完成完整工作流，安装与激活始终需要全局 `plugins:write`：

![插件从安装验签、清单审查、制品批准到正式发布；回滚通过新修订恢复先前内容。](/diagrams/plugin-publication.svg)

- 安装只是拉取、验签并暂存，**不会激活代码**；批准后制品进入候选清单，发布才全局生效。
- 发布与回滚都使用修订 CAS + 幂等键；回滚总是产生一个单调递增的新修订，不会倒回计数器。
- 进行中的请求固定使用其入场时的插件快照，新发布不影响在途请求。

管理 API（均需全局 scope，读 `plugins:read`、写 `plugins:write`）：

| 方法与路径 | 用途 |
| --- | --- |
| `GET /internal/v1/plugin-runtime`（或 `/candidates`） | 当前修订与可用/暂存的版本 ID |
| `GET /internal/v1/plugin-runtime/history` | 安装任务、版本历史与操作者审计 |
| `POST /internal/v1/plugin-runtime/installations` | 发起安装任务（`Idempotency-Key`，202） |
| `GET /internal/v1/plugin-runtime/installations/{id}` | 查看任务与清单审查内容 |
| `POST /internal/v1/plugin-runtime/installations/{id}/approve` | 按审查摘要批准登记 |
| `POST /internal/v1/plugin-runtime/installations/{id}/retry` | 重试中断/失败的任务 |
| `POST /internal/v1/plugin-runtime/publish` | 按 `{inventory_id, expected_revision}` 发布 |
| `POST /internal/v1/plugin-runtime/rollback` | 按 `{target_revision, expected_revision}` 回滚 |
| `GET /internal/v1/plugins`、`GET /internal/v1/plugins/runtime-access` | 已加载清单与当前凭证的插件权限 |

## 插件配置

插件可以在清单中声明对象根的 JSON Schema 与非敏感默认值。运营者通过表单或 API 维护配置：

```bash
curl "https://mtc.example.com/internal/v1/plugins/example-policy/configuration" \
  -H "Authorization: Bearer mts_example_service_token"
```

- 生效优先级：**租户覆盖值 > 全局值 > 清单默认值**。
- `PUT` 需要 `plugins:write`、`Idempotency-Key` 和当前 `expected_version`；冲突返回 409，可安全重放。
- 配置 Schema 禁止 `writeOnly` 字段——API key、OAuth token 等机密不允许放在插件配置里，只能使用核心加密凭证存储。

## 扩展点一览

| 扩展点 | 文档 |
| --- | --- |
| 流量策略 / 请求改写（`traffic-policy.post-auth`） | [插件开发](development.md) |
| 上游 Provider 与 OAuth（`upstream-provider`） | [插件开发](development.md) |
| 组路由 plan/observe（`group-routing-v1`） | [组路由](routing.md) |
| Operator 侧栏页签 / 概览卡片（`typed_data_v1`） | [Operator UI](operator-ui.md) |
