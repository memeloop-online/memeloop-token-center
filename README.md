# Memeloop Token Center

简体中文 · [English](README.en.md)

Memeloop Token Center（MTC）是面向团队的 AI 网关：通过一个入口管理模型服务、客户端凭据、路由权限与用量。

完整产品文档见[文档站](https://memeloop-online.github.io/memeloop-token-center/zh/)。

## 统一模型入口

将客户端的服务地址与 API Key 配置为 MTC，即可使用已授权的模型。接口包括：

- OpenAI 兼容：`/v1/models`、`/v1/chat/completions`、`/v1/responses`、`/v1/embeddings`
- Anthropic：`/v1/messages`、`/v1/messages/count_tokens`
- 语音转写：`/v1/audio/transcriptions`
- 生成任务：`/v1/images/generations`、`/v1/videos/generations`、`/v1/generations`

接口与模型的可用能力取决于接入的上游及路由配置。使用方法见[产品文档](https://memeloop-online.github.io/memeloop-token-center/zh/)。

## 路由与权限

- 模型路由把公开模型名映射到一个或多个上游账号，统一管理账号选择与故障切换。
- 路由可按客户端凭据逐一授权；客户端只能访问被授权的模型与自身的 `/self/v1/*` 视图。
- 上游账号支持 API Key 与 OAuth 接入。
- 多租户隔离：凭据、账号、路由与用量均按租户边界管理。

## 用量与请求

- 请求发出前预留额度、余额与价格上限，完成后按实际上报用量结算。
- 管理端提供实时请求流、会话视图、用量分析、上游剩余额度与可用性观测。
- 客户端可自助查询自己的凭据信息、请求记录、统计与会话。
- 会话与文本请求记录支持查看和回放。

## 插件

通过 WebAssembly 插件扩展模型服务、OAuth 登录、流量策略与请求改写。接口、能力与开发流程见[插件开发文档](https://memeloop-online.github.io/memeloop-token-center/zh/plugins/)。

## 产品界面

以下截图来自真实运行的管理端界面；账户名、邮箱、凭据别名、域名与 ID 等身份信息已替换为示例文字，统计数据保持真实。

![总览：运行监控快照、上游剩余额度与账号/模型组合](docs/public/images/overview.png)

![请求：实时请求流与逐条用量、费用、状态](docs/public/images/requests.png)

![上游服务：账号接入、路由数与近期可用性](docs/public/images/providers.png)
