# memeloop-token-center

PostgreSQL-first、低内存、可扩展的多协议 AI 网关和额度中心。

当前实现重点是可验证的核心纵向链路：稳定 key 身份与无迁移轮换、OpenAI/Anthropic 请求转发、Seedance/ComfyUI 异步生成、请求归档、余额与只读自助统计。生产环境默认使用 PostgreSQL 与 S3；SQLite 和内存对象存储只用于测试。

同一个镜像支持 `serve --role gateway|control|worker|all`。生产 Helm 默认拆分 gateway、control 与 worker；`all` 只供个人或临时测试部署。gateway 不注册 `/internal/v1/*`，control 不注册模型和 self-service 路由。

## Key 身份与权限

一个逻辑 key 由不可变 UUIDv7 `key_id` 标识，密钥字符串只是它的一代 credential。轮换时只吊销旧 credential 并生成下一代，策略、余额账户、请求记录、统计和会话簇始终引用同一个 `key_id`，不复制或迁移历史。

下游 key 只能访问：

- 获准的 `/v1/*` 模型接口。
- `/self/v1/key`、`/self/v1/stats`、`/self/v1/requests*`、`/self/v1/generations*` 和 `/self/v1/conversations*`。
- `/portal` 中与该稳定 key 关联的 CPAMP 风格统计、错误和请求详情。

它不能访问 `/internal/v1/*`。内部 API 仅接受独立的 service token；发放余额还必须提供 `Idempotency-Key`。

## 当前 API

- OpenAI：`/v1/chat/completions`、`/v1/responses`、`/v1/embeddings`。
- Anthropic：`/v1/messages`、`/v1/messages/count_tokens`。
- 多模态生成：`/v1/generations`、`/v1/videos/generations`、`/v1/images/generations`；内置火山引擎 Seedance 视频任务与 ComfyUI 本地/Cloud 图片、视频工作流，统一执行模型权限、额度预留、限流、计费、轮询和 S3/CAS 归档。
- Service：创建/轮换 key、更新模型/限流/预算 policy、设置同币种模型价格、幂等发放余额及撤销完整未消费的 grant；可创建 tenant 绑定、scope 最小化的服务凭据供 memeloop web 使用。服务凭据也有稳定 UUID 与 generation，轮换后旧 token 立即失效。
- Provider：创建稳定上游账号、轮换 API/OAuth credential、配置无需认证的私有 ComfyUI、建立公开模型到上游模型的路由。
- OAuth：原生接入 CPA Subscription Bridge 的 GitHub Copilot/Cursor 订阅登录、opaque handle 推理，以及 Cursor 直接 PKCE/refresh；插件 provider 还能声明同协议的 OAuth Adapter。登录状态是有时限的加密 token，可跨 K8s 副本重试。
- Self-service：key 信息、请求列表/详情、聚合统计、逻辑会话簇/关系边。
- Operator：`/internal/v1/request-events` 以追加式 started/finished 事件提供 SSE 尾流，跨副本从 PostgreSQL 游标续读，不加载归档正文。

模型请求先按正文 token 上界与最大输出做余额预留，同时原子执行 RPM 限流；响应返回 usage 后结算实际费用并释放差额。生成任务按秒或任务等单位预留，worker 使用 PostgreSQL `FOR UPDATE SKIP LOCKED` 租约领取任务并幂等结算。daily、rolling-weekly 与 lifetime budget 均在调用上游前检查；崩溃窗口遗留且没有请求/任务引用的额度预留会由 worker 有界回收。

逻辑对话会优先识别 `X-MTC-Conversation-Id`、`X-Claude-Code-Session-Id` 等显式元信息；缺少元信息时使用语义原子和 tenant-scoped Merkle 前缀树推断 continuation、retry、edit、branch 或 candidate。低置信度只进入候选会话簇，不强制合并。

## 快速开始

```bash
export MTC_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/memeloop_token_center
export MTC_KEY_PEPPER=replace-with-at-least-32-random-bytes
export MTC_SERVICE_TOKEN=replace-with-a-bootstrap-service-token
export MTC_ARCHIVE_BACKEND=s3
export MTC_S3_BUCKET=memeloop-token-center
export MTC_UPSTREAM_OPENAI_URL=http://127.0.0.1:4010
cargo run -- serve
```

服务端管理 API 使用 `MTC_SERVICE_TOKEN`；下游模型 API 和 `/self/v1/*` 使用生成的 `mtc_...` key。用户 key 只能调用被授权模型以及读取该稳定 key 身份对应的请求和统计，不能创建 key、发放余额或修改策略。

`POST /internal/v1/keys` 接受可选 `Idempotency-Key`。同一键和同一规范化请求会在 24 小时内重放同一个稳定 key 响应；换请求体复用会被拒绝。响应缓存使用独立 AAD 的认证加密，worker 到期清除。`PUT /internal/v1/keys/{key_id}/policy` 可由 tenant-scoped `keys:write` 服务凭据幂等更新权限与限流，而不改变 key 身份或历史。

## 测试

```bash
cargo test
cargo test --test cucumber
```

Cucumber 端到端测试会启动 SQLite、内存对象存储和 Mock 上游，不需要 PostgreSQL 或真实 S3。当前覆盖 14 个场景、93 个步骤，包括稳定 key 轮换、权限/额度/限流、订阅 grant 幂等撤销、OpenAI/Anthropic、API/OAuth 同管线、Cursor PKCE、Copilot/Cursor subscription bridge、CPA 账号安全导入、逻辑会话以及 Seedance/ComfyUI 异步生成与归档。

memeloop web 对余额发放使用 `POST /internal/v1/accounts/{account_id}/grants`；取消或替换订阅时使用 `POST /internal/v1/accounts/{account_id}/grant-reversals`，body 指向原 grant 的幂等键，且 reversal 自己必须使用新的 `Idempotency-Key`。为避免对已经消费的服务做隐式退款，当前只允许撤销仍完整未消费的 grant；部分消费后的退款需要由业务侧人工或后续按 grant lot 结算。

数据库 DDL 位于 `migrations/`，每个版本都在同一事务和 PostgreSQL advisory transaction lock 下应用。PostgreSQL 的请求与事件表按天分区，并为 key/tenant/模型/状态/错误/上游/逻辑会话尾查建立专用 B-tree 与 BRIN 索引；生成任务有领取、稳定 key 时间线、上游任务和预留关联索引。SQLite 保留等价的小型测试 schema。连接池默认每进程最多 8 个连接且不预热，HTTP 上游连接池也有严格空闲上限；控制响应与生成文件分别有 4 MiB 和 512 MiB 流式上限。

## 配置与插件

核心、key、service token、provider 账号、模型路由和生成价格的 JSON Schema 位于 `schemas/`，并由 `GET /internal/v1/schemas` 提供。`GET /internal/v1/provider-types` 还会返回每种 provider 贡献的配置与 credential Schema，前端直接交给 JSON Schema 表单渲染器。API key、OAuth 和无需认证的私有上游都是同一种稳定上游账号的 credential；轮换只推进 generation，请求历史继续引用同一个账号主键。credential 使用带认证加密后再写入数据库，并且管理 API 只返回脱敏元信息。

Copilot/Cursor 订阅登录入口为 `/internal/v1/oauth/subscription-bridge/start` 与 `/internal/v1/oauth/subscription-bridge/poll`。Token Center 只加密保存 bridge 返回的 opaque handle 和可选 bridge secret，真实 OAuth 状态继续隔离在 bridge 的持久卷中；下游 OpenAI Chat 请求会原生映射到 bridge `/v1/execute`，并继续执行同一套模型权限、额度、限流、计费和归档。Cursor 直接 PKCE 入口为 `/internal/v1/oauth/cursor/start` 与 `/internal/v1/oauth/cursor/poll`，刷新入口为 `/internal/v1/upstreams/{account_id}/oauth/refresh`。Cursor 原生 Connect/Agent Runtime 到公开协议的转换仍需对应 provider adapter，不能假装成无损 OpenAI 兼容。

`POST /internal/v1/imports/cpa/subscription-accounts` 可以上传 CPA 导出的 Copilot/Cursor auth JSON，并把 opaque handle 幂等迁移为稳定上游账号；API 不返回 handle、bridge secret 或原文件正文。Codex、Kimi 等 auth 文件会以 `requires_provider_adapter` 明确跳过，因为它们的私有 OAuth 执行与刷新语义不是公开 OpenAI API 契约；可以继续把 CPA 当兼容上游，或安装经过审核的 provider adapter，不能把 token 文件机械伪装成通用 OAuth。

插件 ABI 位于 `wit/token-center.wit`；运行时使用 Wasmtime Component Model，每次调用限制 32 MiB 与固定 fuel，HTTP 只能访问 manifest 中由运维审核的精确 origin。声明 `kv` capability 的插件可以使用 PostgreSQL/SQLite 中按插件 ID 隔离的持久 KV，每个值上限 1 MiB、每个插件上限 16 MiB；未声明 capability 的调用会在 host 边界被拒绝。Provider contribution 可以携带 JSON Schema 和 `oauth_adapter`，由 `/internal/v1/oauth/provider-adapter/*` 执行固定的 PKCE adapter 协议，无需给插件任意进程内权限。OCI 插件包格式、`plugin.json` 和 capability 限制见 `plugins/README.md`。Helm Chart 可从只读 ConfigMap/PVC 加载插件；生产 values 只引用外部 PostgreSQL/S3 Secret，不把凭证写入 ConfigMap。

React/Vite 管理端位于 `web/`：`/operator` 是 service token 控制面，提供实时请求、上游账号 Schema 表单、路由、key 和 OAuth；`/portal` 是下游 key 只读统计，可查看生成任务的状态、费用、错误和归档对象。前端静态资产在镜像构建时生成，不依赖 Node.js 运行时。
