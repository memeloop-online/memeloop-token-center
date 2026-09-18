# 凭证管理

MTC 有两类完全隔离的凭证。

| 类型 | 前缀 | 可调用的接口 | 创建方式 |
| --- | --- | --- | --- |
| 客户端凭证 | `mtc_…` | 被授权的 `/v1/*` 模型接口与自己的 `/self/v1/*` 自助视图 | `POST /internal/v1/keys` 或 Operator 控制台 |
| 服务凭证 | `mts_…` | 按 scope 授权的 `/internal/v1/*` 管理接口 | `POST /internal/v1/service-tokens` 或 Operator 控制台 |

客户端凭证永远不能调用管理接口；服务凭证不能代替客户端凭证发起模型请求。

## 稳定身份与轮换

- 每个客户端凭证有一个不可变的 UUIDv7 `key_id`，它是计费账户、策略、请求历史、统计和会话数据的稳定归属。
- 轮换（`POST /internal/v1/keys/{key_id}/rotate`）作废旧凭证并签发新凭证，`key_id` 及其全部历史保持不变。
- 轮换接口要求 `Idempotency-Key`，响应返回新凭证并携带 `Cache-Control: no-store`。

## 复制凭证

客户端凭证列表永远不携带明文。使用列表中的明确「复制凭证」操作，可在单独操作区直接查看当前原值并选择或复制。集成管理界面可调用：

```bash
curl -X POST "https://mtc.example.com/internal/v1/keys/0193f2ab-7c1e-7000-8000-0000000000c3/copy" \
  -H "Authorization: Bearer mts_example_service_token"
```

- 需要 `keys:write` scope；响应为 `no-store`，仅在凭证仍处于启用状态时返回明文。
- 重复调用返回同一个当前凭证，不会触发轮换或任何变更。
- 列表和自助接口永远不包含明文；仅通过 `credential_copy_available` 启用或禁用这个明确操作。

如果授权来源已知当前有效凭证原值，可以在不改变凭证的前提下登记这个准确值，供之后明确复制：

```bash
curl -X PUT "https://mtc.example.com/internal/v1/keys/0193f2ab-7c1e-7000-8000-0000000000c3/credential" \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  --data '{"key":"mtc_example_current_value"}'
```

服务端会校验该值是否匹配当前凭证；操作不会轮换或替换凭证。

## 创建客户端凭证

`POST /internal/v1/keys` 的常用字段（完整定义见 [key-create JSON Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/key-create.schema.json)）：

| 字段 | 说明 |
| --- | --- |
| `principal_external_id` | 调用方的稳定主体标识（必填） |
| `alias` | 便于管理的别名（必填） |
| `currency` | `USD` 或 `CNY`，默认 `USD` |
| `initial_balance` | 预付费初始余额，十进制字符串 |
| `route_ids` / `route_group_ids` | 授予的模型路由 / 路由组，空数组表示不授权任何模型 |
| `policy` | 限流与预算策略，见下表 |

`policy` 子字段：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `requests_per_minute` | 60 | 每分钟请求数上限 |
| `tokens_per_minute` | 100000 | 每分钟词元上限 |
| `max_concurrency` | 4 | 并发请求上限 |
| `enforcement_mode` | `prepaid` | `prepaid` 同步执行余额与限流；`metered_unlimited` 后付费精确记账、不套用共享准入限制 |
| `daily_budget` / `weekly_budget` / `lifetime_budget` | 无 | 预算上限，达到即拒绝 |

创建后可随时通过 `PUT /internal/v1/keys/{key_id}/policy`、`/limits`、`/routing`、`/status`、`/alias` 分项更新；需要并发控制的接口会在 OpenAPI 中标注应携带的版本字段，以其说明为准。

## 服务凭证与 scope

`POST /internal/v1/service-tokens` 接受 `name`、`scopes` 数组和可选的 `tenant_external_id`（留空表示全局运营凭证；填写则将凭证限制在单个租户）。可授予的 scope 按用途分组如下（以 [service-token JSON Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/service-token.schema.json) 与各接口的 `x-required-scope` 标注为准）：

- 凭证与资金：`keys:read`、`keys:write`、`credits:read`、`credits:write`、`entitlements:read`、`entitlements:write`、`settlements:adjust`
- 流量与生成：`requests:read`、`generations:write`、`generations:quarantine:read`、`generations:reconcile`、`filter_assistant:execute`
- 上游与路由：`providers:read`、`providers:write`、`oauth:write`、`routes:read`、`routes:write`、`upstreams:import:write`
- 插件与定价：`plugins:read`、`plugins:write`、`prices:read`、`prices:write`
- 系统：`service_tokens:read`、`service_tokens:write`、`tenants:read`、`tenants:write`、`schemas:read`、`metrics:read`

注意：`filter_assistant:execute` 是独立的计费执行权限，`requests:read` 不会隐式获得它。服务凭证同样支持 `rotate`、`copy`、`status` 管理接口。

## 写操作幂等

部分写操作支持或要求 `Idempotency-Key` 请求头，是否必需以各接口在 OpenAPI 中的标注为准：轮换、余额充值等强制要求；创建凭证、创建路由等可选携带。携带后，相同密钥的精确重放返回原始结果；用同一密钥提交不同请求体会被拒绝。

## 客户端自助视图

持有 `mtc_…` 凭证的客户端可以查询：

| 接口 | 内容 |
| --- | --- |
| `GET /self/v1/key` | 自身凭证信息（不含明文） |
| `GET /self/v1/key/limits` | 当前限流与预算快照 |
| `GET /self/v1/requests`、`/stats`、`/sessions`、`/conversations`、`/generations`、`/usage-analysis`、`/entitlements` | 各自名下的历史与用量 |

浏览器用户可以使用自助门户 `/portal`，输入凭证后数据范围始终限定在该凭证自身。
