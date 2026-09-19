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
