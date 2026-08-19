# MemeLoop Cloud 权益同步

MemeLoop Cloud 通过 `PUT /internal/v1/integrations/memeloop-cloud/subscription` 发送完整订阅快照。这个接口只部署在 control role；生产环境应保持集群内或 Tailnet 可达，不应加入公开 gateway Ingress。

## 认证与重试

配置至少 32 字节、无空白字符的 `MTC_MEMELOOP_CLOUD_WEBHOOK_SECRET`。请求必须携带：

- `Idempotency-Key`：Cloud 中不可复用的事件 ID；服务只持久化其租户作用域哈希。
- `X-MTC-Webhook-Timestamp`：Unix 秒，允许与服务时间相差五分钟。
- `X-MTC-Webhook-Signature`：`v1=` 加 URL-safe、无 padding 的 base64 HMAC-SHA-256。

签名消息是时间戳的 ASCII 字节、一个 `.` 字节和未经改写的 HTTP body。先序列化 body，再签名并发送相同字节。无效签名、过期时间戳或未配置 Secret 均返回 401。

同一租户内重复的 `Idempotency-Key` 和相同快照会重放原权益结果，不会增加第二份额度或凭据。相同事件 ID 携带不同快照返回 409。凭据明文只在初次创建后的 24 小时加密重放窗口内返回；之后 `credential.key` 为 `null`，不会为了重试而轮换或复活旧凭据。

## 快照语义

`status=active` 可表达注册（`desired=0`）、开通、续费、升级、降级和取消后的重新开通。它必须包含当前 `external_cycle_id`、账期、币种、目标总额度、严格递增的 `version` 和完整凭据策略。

`status=cancelled` 必须使用更高版本，并省略额度和账期范围。它只回收当前周期未消费的额度；已消费额度、请求、归档和统计始终保留。取消后的重新开通仍使用相同 key ID、account ID 和历史归属。

乱序的低版本快照返回 409。额度账本与策略更新都以同一个持久订阅版本为条件，因此较旧事件即使与较新事件并发，也不能回滚模型权限、限流或预算。

完整字段与响应定义以 [OpenAPI](../../openapi/openapi.yaml) 为准。
