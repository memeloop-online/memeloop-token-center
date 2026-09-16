# API 总览

完整接口定义见 [OpenAPI](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml)。本页介绍认证、版本、分页与错误处理约定。

## 访问面与凭证

| 面 | 路径 | 凭证 |
| --- | --- | --- |
| 公共网关 | `/v1/*` | `mtc_…` 客户端凭证 |
| 自助 | `/self/v1/*`、`/portal` | `mtc_…` 客户端凭证（仅自身数据） |
| 管理 | `/internal/v1/*`、`/operator`、`/version` | `mts_…` 服务凭证，按 scope 与租户边界授权 |
| 指标 | `/metrics` | 带 `metrics:read` 的服务凭证 |
| 探活 | `/livez`、`/readyz` | 无 |

`/readyz` 使用有界、合并的数据库与归档探测：数据库故障返回 503；归档故障返回 200 但标记降级，资产操作保持 fail-closed。`/healthz` 是 `/livez` 的废弃别名，响应带废弃提示头。

## 路径分组

- `/v1`：OpenAI 兼容（models、chat/completions、responses、embeddings、audio/transcriptions）、Anthropic 兼容（messages、count_tokens）、生成（images、videos、generations）。
- `/self/v1`：`key`、`requests`、`stats`、`sessions`、`conversations`、`generations`、`usage-analysis`、`entitlements`。
- `/internal/v1`：`keys`、`service-tokens`、`upstreams`、`model-routes`、`provider-groups`、`route-groups`、`credential-groups`、`prices`、`requests`、`sessions`、`stats`、`usage-analysis`、`generations`、`plugins`、`plugin-runtime`、`tenants`、`schemas` 等，逐路径的 scope 要求以 OpenAPI 中 `x-required-scope` 为准。

## 版本与错误

- 所有 `/v1`、`/self/v1`、`/internal/v1` 响应携带 `X-MTC-API-Version: v1`。v1 策略允许增量式新增（新字段、新端点）；移除需要公告的废弃窗口与契约更新。
- 错误返回 JSON 与标准 HTTP 状态码，不包含供应商密钥或内部连接信息。
- 请求体超限在 JSON 解析前返回 413；容量饱和返回 503；限流策略命中返回 429。

## 幂等与并发

- 部分写操作支持或要求 `Idempotency-Key` 头，具体以接口定义为准：携带后，精确重放返回原始结果，同键不同体被拒绝。
- 更新类接口使用乐观并发：携带读取时获得的 `expected_updated_at` / `expected_version` / 修订号，冲突返回 409，重新读取后重试。

## 分页与租户

- 大历史列表使用 keyset 游标：`before_created_at` + `before_id` 成对出现，取上一页末行；页大小有上限（请求列表 500，类型化查询 100）。
- 全局服务凭证用 `tenant_external_id` 参数选择租户；租户绑定的凭证始终被限制在自己的租户，越界访问返回 403。

## Responses 的两种传输

`POST /v1/responses` 是 HTTP Responses API；`GET /v1/responses` 是其 WebSocket 形态。两者适用完全相同的认证、路由选择、预留、结算、归档与取消规则；WebSocket 帧大小与期限有界，协议错误以安全的错误帧报告。
