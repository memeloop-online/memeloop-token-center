# 上游账户

![上游服务列表中的账号、路由、可用性与额度刷新入口；身份信息已替换。](/images/providers.png)

上游账户（upstream account）是 MTC 与 AI 供应商之间的连接单元。它有稳定的 `account_id`、供应商驱动 `driver`、连接配置 `config` 和一份当前加密的凭证代。

## 账户模型

- `api_key`、`oauth`、`none` 是**连接方式**。重新授权时，账户标识、路由和历史归属保持不变。
- 供应商密钥与 OAuth 令牌加密存储，账户接口不返回这些值；`config` 中不含凭证材料。
- 每个账户维护一份按凭证代同步的模型目录（`POST /internal/v1/upstreams/{account_id}/models/sync`），路由创建时用目录校验公开/上游模型兼容性。
- 常用管理接口：`GET/POST /internal/v1/upstreams`、`GET/PUT /internal/v1/upstreams/{account_id}`、`GET /internal/v1/upstreams/{account_id}/health`、`GET /internal/v1/upstream-availability`。

## OAuth 登录

OAuth 账户通过管理端登录流程创建，令牌加密存储：

| 供应商 | 流程 | 端点 |
| --- | --- | --- |
| Codex / Kimi / Copilot / Cursor | 设备码（start + poll） | `POST /internal/v1/oauth/{provider}/start`、`POST /internal/v1/oauth/{provider}/poll` |
| Claude | 授权码（start + complete） | `POST /internal/v1/oauth/claude/start`、`POST /internal/v1/oauth/claude/complete` |
| 插件声明的供应商 | 通用授权码 PKCE | `POST /internal/v1/oauth/authorization-code/start`、`.../complete` |
| 插件适配器 | PKCE 轮询 | `POST /internal/v1/oauth/provider-adapter/start`、`.../poll` |

登录相关的管理接口需要 `oauth:write`。既有账户支持 `POST /internal/v1/upstreams/{account_id}/oauth/refresh` 与 `POST /internal/v1/upstreams/{account_id}/oauth/disconnect`。

## 账户代理

账户可以配置私有 SOCKS5 代理。代理 URL（含可选的代理用户名密码）加密存储，通过专用管理接口查看和修改：

- 普通账户元数据只暴露 `has_proxy`、代理 scheme、远程 DNS 语义、不含主机的标签和指纹，用于界面展示与识别。
- 需要查看或复制完整代理 URL 时，使用显式管理接口（需要 `providers:write`、全局运营权限并校验租户归属）：

```bash
curl "https://mtc.example.com/internal/v1/upstreams/0193f2ab-7c1e-7000-8000-0000000000a1/transport-proxy?tenant_external_id=default" \
  -H "Authorization: Bearer mts_example_service_token"
```

该 GET 返回完整代理 URL、网络范围和编辑所需的版本字段，响应为 `Cache-Control: private, no-store`。`PUT` 同路径携带当前版本可修改代理，保留账户的 API 密钥与 OAuth 令牌。规则要点：

- `socks5`：目标与代理由 MTC 解析并固定；`socks5h`：供应商主机名交给代理解析，仅允许安全私网 IP 字面值代理。
- HTTP(S) 代理、公网 SOCKS 和主机名形式的 `socks5h` 一律拒绝；元数据地址等特殊地址永远被阻断。

## 额度读取

`GET /internal/v1/upstreams/{account_id}/quota`（需要 `providers:read`）按账户读取供应商侧额度快照，用于运营观测与（可选的）路由插件输入：

- 快照缓存 30 秒并带并发合并；读取失败后最多保留 5 分钟的有界旧值，失败不会把旧证据伪装成新值。
- 运营端刷新控件会显式请求 `fresh=true`，并以 `trigger=manual` 或 `trigger=bulk` 标记动作；这会跳过仍在有效期内的缓存，同时保留与更新中的读取安全合并的能力。
- 返回按**窗口**组织：每个窗口包含标识、周期（如 5 小时或每周）、重置时间、已用/剩余比例与是否耗尽。窗口信息来自供应商响应的显式字段——未知的量保持未知，绝不显示成 0 或满额。
- `unsupported` 表示该账户类型没有额度适配器，不等于「无限额度」。
- 额度读取不刷新令牌、不发起模型请求、不消费重置额度。

当前适配范围：

| 供应商 | 读取内容 |
| --- | --- |
| Codex（原生 OAuth） | 用量窗口、重置时间、重置额度余额 |
| Kimi（原生 OAuth） | 用量窗口比例与周期 |
| Google Antigravity（插件 OAuth） | 按 group/bucket 的剩余比例与显式窗口元数据 |
| Cursor（原生 OAuth） | 只读的模型列表、当前账期用量与计划信息。**MTC 不提供 Cursor 推理能力**：Cursor 账户只能用于上述只读读取，不能作为模型请求的上游。 |

## 额度重置

部分供应商提供「重置额度」积分。MTC 把它建模为显式二次确认操作，任何读取都不会隐式触发：

1. `POST /internal/v1/upstreams/{account_id}/quota-reset/prepare`（`Idempotency-Key`）：基于新鲜的只读观测创建操作，返回一次性 `confirmation_token`（120 秒有效）——此步不消耗任何额度。
2. `POST /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}/confirm`：凭确认令牌真正向供应商发起重置。
3. `POST /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}/reconcile`：重置后重新核对观测；`GET /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}` 可查询操作状态。

准备、确认、对账各有独立的幂等键；未决操作会阻止对同一账户的再次 prepare。

## 删除与退役

删除账户前用 `GET /internal/v1/upstreams/{account_id}/deletion-readiness` 检查是否仍有路由引用。账户被停用后历史请求归属保持不变。
