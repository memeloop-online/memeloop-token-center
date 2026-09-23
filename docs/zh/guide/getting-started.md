# 快速开始

Memeloop Token Center（MTC）为文本、图片、音频和视频请求提供统一入口。本文从已经获得客户端凭证开始，介绍如何连接服务、发起请求和查看自己的用量。

## 连接服务

将客户端的服务地址设置为部署提供的 MTC 地址，例如 `https://mtc.example.com`；将客户端凭证放在 `Authorization: Bearer` 请求头中。凭证只应保存在服务端或受控的本地开发环境，不要提交到代码仓库。

MTC 支持以下客户端协议：

- OpenAI 兼容：`/v1/models`、`/v1/chat/completions`、`/v1/responses`、`/v1/responses/compact`、`/v1/alpha/search`、`/v1/embeddings`、`/v1/audio/transcriptions`
- Anthropic 兼容：`/v1/messages`、`/v1/messages/count_tokens`
- 生成任务：`/v1/images/generations`、`/v1/videos/generations`、`/v1/generations`

可用模型和接口取决于凭证获得的授权，以及部署连接的上游服务。

## 发起第一个请求

下面的示例使用 OpenAI 兼容接口。地址、模型名和凭证均为示例值：

```bash
curl -X POST https://mtc.example.com/v1/chat/completions \
  -H "Authorization: Bearer mtc_example_client_key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "example-chat",
    "messages": [{ "role": "user", "content": "你好" }]
  }'
```

使用 `/v1/models` 查看当前凭证可访问的模型：

```bash
curl https://mtc.example.com/v1/models \
  -H "Authorization: Bearer mtc_example_client_key"
```

## 查看自己的请求与用量

客户端凭证可以通过 `/self/v1/*` 查询自己的数据：

```bash
curl "https://mtc.example.com/self/v1/requests?limit=20" \
  -H "Authorization: Bearer mtc_example_client_key"
```

可用视图包括请求记录、统计、会话和对话。查询结果只包含当前凭证有权查看的数据。

## 下一步

- [客户端凭证](credentials.md)：安全保存、轮换和使用凭证
- [模型路由](routing.md)：理解模型名称、授权与故障转移
- [上游账户](upstreams.md)：了解服务如何连接不同模型供应商
- [请求与用量](requests.md)：阅读请求记录、会话和费用口径
- [API 总览](api.md)：查看公开接口族和传输约定
