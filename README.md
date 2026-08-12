# memeloop-token-center

PostgreSQL-first、低内存、可扩展的多协议 AI 网关和额度中心。

当前实现重点是可验证的核心纵向链路：稳定 key 身份与无迁移轮换、OpenAI/Anthropic 请求转发、请求归档、余额与只读自助统计。生产环境默认使用 PostgreSQL 与 S3；SQLite 和内存对象存储只用于测试。

同一个镜像支持 `serve --role gateway|control|worker|all`。生产 Helm 默认拆分 gateway、control 与 worker；`all` 只供个人或临时测试部署。gateway 不注册 `/internal/v1/*`，control 不注册模型和 self-service 路由。

## Key 身份与权限

一个逻辑 key 由不可变 UUIDv7 `key_id` 标识，密钥字符串只是它的一代 credential。轮换时只吊销旧 credential 并生成下一代，策略、余额账户、请求记录、统计和会话簇始终引用同一个 `key_id`，不复制或迁移历史。

下游 key 只能访问：

- 获准的 `/v1/*` 模型接口。
- `/self/v1/key`、`/self/v1/stats`、`/self/v1/requests*` 和 `/self/v1/conversations*`。
- `/portal` 中与该稳定 key 关联的 CPAMP 风格统计、错误和请求详情。

它不能访问 `/internal/v1/*`。内部 API 仅接受独立的 service token；发放余额还必须提供 `Idempotency-Key`。

## 当前 API

- OpenAI：`/v1/chat/completions`、`/v1/responses`、`/v1/embeddings`。
- Anthropic：`/v1/messages`、`/v1/messages/count_tokens`。
- Service：创建/轮换 key、设置同币种模型价格、幂等发放余额。
- Provider：创建稳定上游账号、轮换 API/OAuth credential、建立公开模型到上游模型的路由。
- OAuth：Cursor PKCE 登录、轮询完成和 refresh；登录状态是有时限的加密 token，可跨 K8s 副本重试。
- Self-service：key 信息、请求列表/详情、聚合统计、逻辑会话簇/关系边。

模型请求先按正文 token 上界与最大输出做余额预留，同时原子执行 RPM 限流；响应返回 usage 后结算实际费用并释放差额。daily、rolling-weekly 与 lifetime budget 均在调用上游前检查。

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

## 测试

```bash
cargo test
cargo test --test cucumber
```

Cucumber 端到端测试会启动 SQLite、内存对象存储和 Mock 上游，不需要 PostgreSQL 或真实 S3。

## 配置与插件

核心、key policy、provider 账号和模型路由的 JSON Schema 位于 `schemas/`。`GET /internal/v1/provider-types` 还会返回每种 provider 贡献的配置与 credential Schema，前端可直接交给 JSON Schema 表单渲染器。API key 与 OAuth 都是同一种稳定上游账号的 credential；轮换只推进 generation，请求历史继续引用同一个账号主键。credential 使用带认证加密后再写入数据库，并且管理 API 只返回脱敏元信息。

Cursor 登录入口为 `/internal/v1/oauth/cursor/start` 与 `/internal/v1/oauth/cursor/poll`，刷新入口为 `/internal/v1/upstreams/{account_id}/oauth/refresh`。这里负责 PKCE、token 生命周期与稳定账号；Cursor 原生 Connect/Agent Runtime 到公开协议的转换仍应由专用 provider 插件完成，不能假装成无损 OpenAI 兼容。当前内置 `http-json` driver 可用于兼容上游或独立适配 sidecar。

插件 ABI 位于 `wit/token-center.wit`；OCI 插件包格式和 capability 限制见 `plugins/README.md`。Helm Chart 位于 `charts/memeloop-token-center`，生产 values 只引用外部 PostgreSQL/S3 Secret，不把凭证写入 ConfigMap。

React/Vite 管理端位于 `web/`：`/operator` 是 service token 控制面，提供实时请求、上游账号 Schema 表单、路由、key 和 OAuth；`/portal` 是下游 key 只读统计。前端静态资产在镜像构建时生成，不依赖 Node.js 运行时。
