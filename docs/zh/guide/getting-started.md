# 快速开始

Memeloop Token Center（MTC）是面向 AI 文本、图片、音频与视频请求的统一网关：客户端使用开放协议接入，MTC 负责凭证管理、模型路由、配额与余额记账、请求历史和归档。

## 访问面

同一服务镜像以 `serve --role gateway|control|worker|all` 运行，对外呈现三个逻辑访问面。本文档统一以 `https://mtc.example.com` 作为示例地址，实际地址以你的部署为准。

| 访问面 | 路径 | 用途 | 凭证 |
| --- | --- | --- | --- |
| 网关 | `/v1/*`、`/self/v1/*`、`/portal` | 客户端 AI 请求与自助查询 | `mtc_…` 客户端凭证 |
| 控制面 | `/internal/v1/*`、`/operator`、`/version` | 运营管理与 Operator 控制台 | `mts_…` 服务凭证 |
| 探活 | `/livez`、`/readyz` | 进程健康与依赖就绪检查 | 无 |

支持的客户端协议：

- OpenAI 兼容：`/v1/models`、`/v1/chat/completions`、`/v1/responses`（HTTP 与 WebSocket）、`/v1/embeddings`、`/v1/audio/transcriptions`
- Anthropic 兼容：`/v1/messages`、`/v1/messages/count_tokens`
- 生成类：`/v1/images/generations`、`/v1/videos/generations`、`/v1/generations`

## 发起第一个请求

以下步骤使用 curl 调用控制面 API；也可以在 Operator 控制台（`https://mtc.example.com/operator`）中用表单完成同样操作。前提是你已有一个受支持供应商的 base_url 与可用模型名。示例中的地址、凭证与 ID 均为虚构，实际操作时请使用创建账户和路由返回的 `id`，以及创建凭证返回的 `key_id`。

### 1. 准备服务凭证

管理操作需要 `mts_…` 服务凭证。首次部署通过引导流程获得，之后在 Operator 控制台的「服务凭证」页创建。本流程需要上游、路由、凭证与价格的读写权限；只给日常使用者授予其工作所需的权限。

### 2. 添加上游账户

```bash
curl -X POST https://mtc.example.com/internal/v1/upstreams \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -d '{
    "tenant_external_id": "default",
    "name": "Example Provider Account",
    "driver": "http-json",
    "config": { "base_url": "https://api.provider-example.com" },
    "credential": { "type": "api_key", "value": "sk-example-upstream-key" }
  }'
```

响应中的 `id` 是账户标识，后续接口将它称为 `account_id`。OAuth 类供应商请改用登录流程，见[上游账户](upstreams.md)。

### 3. 同步模型目录

```bash
curl -X POST "https://mtc.example.com/internal/v1/upstreams/0193f2ab-7c1e-7000-8000-0000000000a1/models/sync?tenant_external_id=default" \
  -H "Authorization: Bearer mts_example_service_token"
```

同步可能返回 `already-syncing` 等进行中状态，即刷新仍在后台执行。创建路由前，用 `GET /internal/v1/upstreams/{account_id}/models` 确认目标上游模型已出现在目录中；同步完成的目录用于路由时的模型兼容性校验。

### 4. 配置模型价格

在 Operator 控制台的「定价」页为公开模型 `example-chat` 创建价格：选择币种（USD 或 CNY）并填写输入/输出词元单价。没有价格的模型无法通过请求前的预留校验，请务必在首发请求前完成本步。

### 5. 创建模型路由

```bash
curl -X POST https://mtc.example.com/internal/v1/model-routes \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 8f2a1c4e-0000-4000-8000-example0002" \
  -d '{
    "tenant_external_id": "default",
    "public_model": "example-chat",
    "upstream_model": "provider-chat-v2",
    "protocol": "openai",
    "upstream_account_ids": ["0193f2ab-7c1e-7000-8000-0000000000a1"]
  }'
```

`public_model` 是客户端可见的模型名，`upstream_model` 是发往供应商的模型名。

### 6. 创建客户端凭证并授权路由

```bash
curl -X POST https://mtc.example.com/internal/v1/keys \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 8f2a1c4e-0000-4000-8000-example0003" \
  -d '{
    "tenant_external_id": "default",
    "principal_external_id": "user-alice",
    "alias": "alice-dev",
    "initial_balance": "10.00",
    "route_ids": ["0193f2ab-7c1e-7000-8000-0000000000b2"]
  }'
```

`initial_balance` 是预付费初始余额（默认 `0`，余额为零时请求会被拒绝）。响应中的 `key` 字段是 `mtc_…` 明文凭证。如果丢失，可以通过授权的复制接口取回当前凭证，见[凭证管理](credentials.md)。

### 7. 发起请求

```bash
curl -X POST https://mtc.example.com/v1/chat/completions \
  -H "Authorization: Bearer mtc_example_client_key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "example-chat",
    "messages": [{ "role": "user", "content": "你好" }]
  }'
```

### 8. 查看用量

客户端可随时查询自己的请求历史与统计：

```bash
curl "https://mtc.example.com/self/v1/requests?limit=20" \
  -H "Authorization: Bearer mtc_example_client_key"
```

也可以使用自助门户 `https://mtc.example.com/portal`。

## 下一步

- [凭证管理](credentials.md)：轮换、额度策略、服务凭证 scope
- [模型路由](routing.md)：多账户路由、健康与故障转移、传输策略
- [上游账户](upstreams.md)：OAuth 登录、代理、额度窗口与刷新
- [请求与用量](requests.md)：历史查询、会话元数据、费用口径
- [API 总览](api.md)：版本策略、幂等、分页与错误约定
