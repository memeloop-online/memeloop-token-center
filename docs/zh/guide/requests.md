# 请求与用量

![实时请求列表中的模型、客户端凭据、用量、费用与响应表现；身份信息已替换。](/images/requests.png)

MTC 为每个请求保留稳定的租户、凭证、路由、账户与价格快照归属。本文说明如何查询历史、如何给请求附加会话元数据，以及如何理解用量与费用口径。

## 查询接口

| 角色 | 接口 | 内容 |
| --- | --- | --- |
| 运营（`requests:read`） | `GET /internal/v1/requests` | 最近请求与生成任务，最多 500 条 |
| 运营 | `GET /internal/v1/requests/{request_id}` | 单请求详情（需配 `request_kind` 为 `text` 或 `generation`） |
| 运营 | `GET /internal/v1/requests/{request_id}/archive/{side}` | 归档正文（`side` 为 `request` 或 `response`） |
| 运营 | `GET /internal/v1/stats`、`/usage-analysis`、`/request-events`、`/sessions` | 聚合统计、用量分析、实时事件、会话视图 |
| 客户端 | `GET /self/v1/requests`、`/stats`、`/sessions`、`/conversations` | 同口径、仅限自身凭证的数据 |

分页使用 keyset 游标：取上一页最后一行的 `created_at` 与 `request_id`，作为下一页的 `before_created_at` + `before_id`（必须成对）。列表过滤条件按 AND 组合，时间区间闭区间；精确单条查询与游标参数互斥。

## 类型化过滤器

`POST /internal/v1/requests/query` 接受一棵「仅 AND」的类型化过滤树。字段、操作符和值类型都是白名单，服务端映射到固定索引列并绑定参数——它不是 SQL，也不能执行任意查询文本。最多 12 个条件、100 行。

```json
{
  "tenant_external_id": "default",
  "limit": 50,
  "ast": {
    "logical_operator": "and",
    "conditions": [
      { "field": "model", "operator": "equals", "value": { "type": "model", "value": "example-chat" } },
      { "field": "status", "operator": "equals", "value": { "type": "status", "value": "error" } },
      { "field": "created_at", "operator": "greater_than_or_equal", "value": { "type": "timestamp", "value": 1780000000000 } }
    ]
  }
}
```

可用字段：`created_at`、`key_id`、`model`、`protocol`、`status`、`error_code`、`upstream_account_id`、`route_id`、`duration_ms`、`cost_micros`、`key_alias`、`principal`。保存的与最近的过滤器（`/internal/v1/filter-presets`）按服务身份与租户隔离。

### 过滤器助手

运营可以让模型把自然语言意图翻译成上述过滤树：`PUT /internal/v1/filter-assistant/settings` 配置一条已启用的 MTC 模型路由与计费凭证后，`POST /internal/v1/filter-assistant/plan` 返回经过校验的 AST 供预览。只有用户意图、当前时间和固定 AST 结构会发送给模型；请求记录与上游配置不会外发。调用模型需要独立的 `filter_assistant:execute` scope，无效或越界的模型输出永远不会变成已应用的过滤器。

## 会话与执行元数据

下游应用可以在任何 `/v1/*` 文本请求上附加可选的声明式元数据，用于控制台的时间线、关系图与成本视图：

| 请求头 | 含义 |
| --- | --- |
| `X-MTC-Session-Name` | 人类可读的会话名 |
| `traceparent` | W3C trace 上下文 |
| `X-MTC-Trace-Id` / `X-MTC-Span-Id` / `X-MTC-Parent-Span-Id` | 显式 trace/span 覆盖 |
| `X-MTC-Agent-Id` / `X-MTC-Parent-Agent-Id` | 稳定的代理实例/角色及其父级 |
| `X-MTC-Task-Kind` | 任务类型，如 `interactive`、`background` |
| `X-MTC-Session-Labels` | 最多 16 对字符串的 JSON 对象标签 |

要点：

- 等价字段也可放在请求体的 `metadata` 下（snake_case），请求头优先。标签键 ≤64 字符、值 ≤128 字符；形似凭证/密钥的键会被丢弃。
- 这些声明只用于可视化与审计投影：不改变授权、路由、计费身份，也不会把低置信的候选关系升级成确定关系。
- 缺失的值保持缺失——MTC 不会用模型从提示词里「猜」会话名或任务类型。会话详情中的 `structure` 投影单独承载协议侧证据（会话/轮次/父级/响应 ID 等），与人工声明明确区分。

## 用量与费用口径

请求记录中的 `usage_basis` 表明词元用量的来源：

| 值 | 含义 |
| --- | --- |
| `provider_reported` | 供应商终态报告的实测用量 |
| `provider_estimated` | 供应商给出的估算 |
| `contract_ceiling` | 未取得可靠用量时按合同上限的保守结算 |
| `not_observed` | 未观测到有效用量，本地实际用量为零 |

理解费用时请注意：

- `cost` 是 MTC 本地账本的结算金额，不是供应商发票；外部账单需另行对账。
- `not_observed` 表示「费用未知」而非「费用为零」：请求记录不应把数值零当作实际零成本。
- 只有 `provider_reported` 的输出词元可以作为实测生成速度的分子。

## 归档完整性

请求与响应正文在加密后进入归档。详情中的 `archive_complete=false` 表示正文尚不完整，需要结合归档状态区分等待上传、上传失败或缺失；该字段本身不保证稍后一定可以读取。生成任务的产物通过 `GET …/generations/{job_id}/assets/{asset_id}` 或请求资产接口读取。
