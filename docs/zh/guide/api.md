# API 总览

MTC 为客户端提供模型请求接口和凭证范围内的自助查询接口。完整的公开字段与协议定义见仓库中的 [OpenAPI](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml)。

## 客户端接口

| 路径族 | 用途 |
| --- | --- |
| `/v1/*` | 模型列表、文本、图片、音频、视频和其他生成请求 |
| `/self/v1/*` | 当前客户端凭证自己的请求、统计、会话与对话 |
| `/portal` | 部署提供的自助入口（如已启用） |

公开模型接口兼容 OpenAI 与 Anthropic 的常见协议；具体能力由部署连接的上游和路由授权决定。

## 版本与错误

- 响应携带 `X-MTC-API-Version: v1`，用于识别 API 版本。
- 错误返回 JSON 和标准 HTTP 状态码，不包含供应商凭证材料。
- 新字段和新接口可以在 v1 内增量加入；移除已有字段或行为需要公开的废弃窗口。

## 重试与分页

支持写入的请求应遵循接口文档中的幂等要求，并在重试时复用同一个 `Idempotency-Key`。列表接口使用服务返回的游标继续读取，客户端不要用偏移量推断下一页。

## Responses 传输

`/v1/responses` 支持 HTTP 与 WebSocket 传输。客户端应处理增量事件、正常终态和错误终态；网络中断后是否可以继续请求取决于客户端的幂等策略和上游能力。

## Anthropic Messages 与 Claude Code 网关

MTC 通过 `POST /v1/messages` 和 `POST /v1/messages/count_tokens` 提供 Anthropic Messages 接口，并复用客户端凭据认证、路由授权、审计、计费和上游网络策略。Claude Code 可将 `ANTHROPIC_BASE_URL` 指向 MTC，并使用 MTC 客户端凭据。

- `anthropic-version`、`anthropic-beta`、其他 `anthropic-*` 请求头，以及 `x-claude-code-*` 会话和代理标识会传递给 Anthropic 格式上游。请求字段保持开放，工具、思考、缓存和上下文管理等新能力可与对应 beta 能力一起传递。
- 流式 Messages 响应以 `text/event-stream` 持续转发，心跳使用 Anthropic 的 `event: ping` 与 `{"type":"ping"}` 数据格式。Ping 是传输控制事件。`retry-after`、`retry-after-ms`、`x-should-retry` 和 `anthropic-ratelimit-*` 响应头会返回给客户端，用于重试和额度展示。
- `GET /v1/models` 支持 Anthropic 网关模型发现：`limit` 可取 1 至 1000，并可携带一个 `before_id` 或 `after_id` 游标。响应使用 Anthropic 列表结构，仅列出当前 MTC 客户端凭据拥有有效 Anthropic 路由的模型。
- Anthropic 格式上游返回错误时，MTC 会将状态码、响应体和重试/额度响应头返回给客户端；审计记录保留请求终态事实，不保存上游错误响应体。

当选中的上游支持该端点时，可使用 `/v1/messages/count_tokens`。各服务商的协议转换由对应 provider adapter 提供，核心网关保持 Anthropic Messages 信封结构。
