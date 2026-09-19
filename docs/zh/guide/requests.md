# 请求与用量

![实时请求列表中的模型、客户端凭据、用量、费用与响应表现；身份信息已替换。](/images/requests.png)

MTC 为每个客户端请求保留可查询的模型、协议、状态、用量和费用信息。本文说明客户端可见的请求视图，以及如何理解结算数据。

## 查询自己的请求

客户端凭证可以通过以下路径查询自己的数据：

| 路径 | 内容 |
| --- | --- |
| `GET /self/v1/requests` | 分页请求记录 |
| `GET /self/v1/stats` | 用量与费用统计 |
| `GET /self/v1/sessions` | 会话与关联请求 |
| `GET /self/v1/conversations` | 可回放的对话记录 |

使用 `limit` 与服务返回的游标继续读取下一页。不同协议的字段可能略有差异，未知值会保留为未知，不应按零处理。

## 会话与执行元数据

下游应用可以在文本请求上附加可选元数据，用于请求列表和会话视图：

| 请求头 | 含义 |
| --- | --- |
| `X-MTC-Session-Name` | 人类可读的会话名 |
| `traceparent` | W3C trace 上下文 |
| `X-MTC-Trace-Id` / `X-MTC-Span-Id` / `X-MTC-Parent-Span-Id` | trace/span 标识 |
| `X-MTC-Agent-Id` / `X-MTC-Parent-Agent-Id` | 代理实例及其父级 |
| `X-MTC-Task-Kind` | 任务类型，如 `interactive` 或 `background` |
| `X-MTC-Session-Labels` | 有界的字符串标签对象 |

这些字段只用于可视化和关联，不改变授权、路由或计费身份。缺少的值保持缺失，MTC 不会根据请求正文推测会话名称或任务类型。

## 用量与费用

- `provider_reported` 表示供应商在终态报告的用量。
- `provider_estimated` 表示供应商提供的估算值。
- `contract_ceiling` 表示无法取得可靠用量时采用的合同上限。
- `not_observed` 表示没有观察到有效用量；它不是实际费用为零。

费用是 MTC 本地账本中的结算金额，不等同于供应商账单。请求详情中的归档状态用于区分正文仍在上传、上传失败和没有正文。
