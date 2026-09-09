# MemeLoop Cloud 权益同步

MemeLoop Cloud 通过 `PUT /internal/v1/integrations/memeloop-cloud/subscription` 发送完整订阅快照。这个接口只部署在 control role；生产环境应保持集群内或 Tailnet 可达，不应加入公开 gateway Ingress。

在发送首个快照前，Cloud 可使用 tenant-scoped、具有 `keys:write` 的 service Bearer 调用
`POST /internal/v1/integrations/memeloop-cloud/principals/ensure`，body 为
`tenant_external_id`、`principal_external_id` 和 `currency`。它确保与 webhook 完全相同的稳定
key ID 和 credit account ID，但不会创建权益、修改策略或路由，也不会记入任何额度。重放不会轮换
凭据；仅在原始的一次性凭据加密重放窗口仍有效时返回 `key`。同一稳定身份使用不同币种返回 409，
tenant-scoped token 访问其他租户返回 403。两个 external ID 必须是无首尾空白的有效标识，避免
Cloud 和 Token Center 对稳定身份作出不同规范化。

租户限定服务令牌要求其租户已存在且处于 active 状态；创建令牌本身不会创建租户。
不存在或已停用的租户仍被认证层拒绝（401），ensure 不能绕过该租户生命周期边界。

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
