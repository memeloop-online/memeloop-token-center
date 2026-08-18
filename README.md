# memeloop-token-center

开源的高性能 AI 文本、图片、视频中转站与 AI 用量数据统计服务。

当前实现重点是可验证的核心纵向链路：稳定凭据身份与无迁移轮换、OpenAI/Anthropic 请求转发、Seedance/ComfyUI 异步生成、请求归档、余额与只读自助统计。生产环境默认使用 PostgreSQL 与 S3；SQLite 和内存对象存储只用于测试。

客户端按公开协议接入而不是按品牌分叉：Codex 可用 Responses，Claude Code 可用 Anthropic Messages，Copilot、Cursor、WorkBuddy 及其他 OpenAI-compatible 客户端使用相同 `/v1/*` 网关。请求记录会结合客户端 session header、正文 metadata 与 Merkle 上下文前缀推断逻辑对话。

同一个镜像支持 `serve --role gateway|control|worker|all`。生产 Helm 默认拆分 gateway、control 与 worker；`all` 只供个人或临时测试部署。gateway 不注册 `/internal/v1/*`，control 不注册模型和 self-service 路由。

## 凭据身份与权限

一个逻辑凭据由不可变 UUIDv7 `key_id` 标识，密钥字符串只是它的一代 credential。轮换时只吊销旧 credential 并生成下一代，策略、余额账户、请求记录、统计和会话簇始终引用同一个 `key_id`，不复制或迁移历史。迁移 CPA 时还可把原凭据以 peppered HMAC credential 绑定到这个稳定身份；不会保存明文，也不要求客户端切换。

下游凭据只能访问：

- 获准的 `/v1/*` 模型接口。
- `/self/v1/key`、`/self/v1/stats`、`/self/v1/requests*`、`/self/v1/generations*` 和 `/self/v1/conversations*`。
- `/portal` 中与该稳定 key 关联的 CPAMP 风格统计、错误和请求详情。

它不能访问 `/internal/v1/*`。内部 API 仅接受独立的 service token；发放余额还必须提供 `Idempotency-Key`。

## 当前 API

- OpenAI：`/v1/models`、`/v1/chat/completions`、`/v1/responses`、`/v1/embeddings`。
- Anthropic：`/v1/messages`、`/v1/messages/count_tokens`。
- 多模态生成：`/v1/generations`、`/v1/videos/generations`、`/v1/images/generations`；内置火山引擎 Seedance 视频任务、ComfyUI 本地/Cloud 工作流、OpenAI Images API，以及把 Codex Responses `image_generation` 工具映射为标准 Images API 的路由模式，统一执行模型权限、额度预留、限流、计费和 S3/CAS 归档。
- Service：创建/轮换凭据、更新模型/限流/预算 policy、设置同币种模型价格、幂等发放余额及撤销完整未消费的 grant；可创建 tenant 绑定、scope 最小化的服务凭据供 memeloop web 使用。服务凭据也有稳定 UUID 与 generation，轮换后旧 token 立即失效。
- 上游提供商：在同一资源模型和管理界面中创建稳定上游账号；API 凭据、OAuth、订阅桥与无需认证的私有 ComfyUI 只是不同接入方式。统一列表提供接入方式、凭据到期时间和路由数；支持配置编辑、启停、无正文健康检查、审计安全删除、幂等 credential/OAuth 刷新、模型路由、Copilot/Cursor subscription bridge、Cursor PKCE 和插件贡献的 OAuth Adapter。
- Self-service：key 信息、请求列表/详情、聚合统计、逻辑会话簇/关系边。
- Operator：`/internal/v1/request-events` 以追加式 started/finished 事件提供 SSE 尾流，跨副本从 PostgreSQL 游标续读；请求列表支持时间、keyset 游标、凭据、模型、协议、状态、错误、上游、路由、耗时、费用、别名和主体筛选；统计复用这些维度，默认最近 30 天且最大 93 天。`/internal/v1/requests/{request_id}` 按需加载文本或生成任务归档正文。全部接口同时执行 service scope 与 tenant 边界检查。

模型请求先按正文 token 上界与最大输出做余额预留，同时原子执行 RPM 限流；响应返回 usage 后结算实际费用并释放差额。生成任务按秒或任务等单位预留，worker 使用 PostgreSQL `FOR UPDATE SKIP LOCKED` 租约领取任务并幂等结算。daily、rolling-weekly 与 lifetime budget 均在调用上游前检查；崩溃窗口遗留且没有请求/任务引用的额度预留会由 worker 有界回收。

逻辑对话会优先识别 `X-MTC-Conversation-Id`、`X-MTC-Turn-Id`、`X-MTC-Parent-Turn-Id`、`X-MTC-Branch-Id`、`X-MTC-Compaction` 和 `X-Claude-Code-Session-Id` 等显式元信息。结构化 parent/branch/compaction 会保存为版本化关系证据；缺少元信息时使用语义原子和 tenant-scoped Merkle 前缀树推断 continuation、retry、edit、branch 或 candidate。低置信度只进入候选会话簇，不强制合并。

## 快速开始

```bash
export MTC_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/memeloop_token_center
export MTC_KEY_PEPPER=replace-with-at-least-32-random-bytes
export MTC_SERVICE_TOKEN="$(openssl rand -hex 32)"
export MTC_ARCHIVE_BACKEND=s3
export MTC_S3_BUCKET=memeloop-token-center
export MTC_UPSTREAM_OPENAI_URL=http://127.0.0.1:4010
cargo run -- serve
```

服务端管理 API 使用 `MTC_SERVICE_TOKEN`；请为生产环境生成至少 32 个随机字节（上面的命令将其编码为 64 个十六进制字符），并通过 Secret 管理器注入。启动时会拒绝少于 32 字节，或在任意位置含 Unicode 空白字符/`Cc` 类控制字符的值，以避免 Secret 换行和不可见字符造成歧义；这是格式下限，不会估算随机性或熵。下游模型 API 和 `/self/v1/*` 使用生成的 `mtc_...` key。用户 key 只能调用被授权模型以及读取该稳定 key 身份对应的请求和统计，不能创建 key、发放余额或修改策略。

`POST /internal/v1/keys` 接受可选 `Idempotency-Key`。同一键和同一规范化请求会在 24 小时内重放同一个稳定 key 响应；换请求体复用会被拒绝。响应缓存使用独立 AAD 的认证加密，worker 到期清除。`PUT /internal/v1/keys/{key_id}/policy` 可由 tenant-scoped `keys:write` 服务凭据幂等更新权限与限流，而不改变 key 身份或历史。

## 测试

```bash
cargo test
cargo test --test cucumber
```

Cucumber 端到端测试会启动 SQLite、内存对象存储和 Mock 上游。当前实际运行通过 4 个 feature、56 个默认场景、320 个步骤，包括稳定凭据轮换、CPA 原凭据兼容、权限/额度/限流、空模型 allowlist、鉴权先于正文解析、跨租户授权矩阵、订阅 entitlement/grant、OpenAI/Anthropic/兼容客户端、API/OAuth 同管线、Cursor PKCE、Copilot/Cursor subscription bridge、CPA 账号安全导入、逻辑会话，以及 Seedance、ComfyUI、OpenAI Images、Codex Responses 生图的转发、归档和计费。浏览器 dogfood 使用 TypeScript Cucumber.js，Playwright 仅作为步骤中的浏览器驱动；当前 fixture run 为 10 个场景、76 个步骤，最近一次完整运行通过 10/10 场景和 76/76 步骤。`MTC_TEST_POSTGRES_URL` 会额外启用真实 PostgreSQL 数据库集成测试；当前工作树已在 fresh PostgreSQL 17 上从 v1 迁移到 v35，并复跑预算/结算、归档 staging、生成、managed OAuth、proxy exactly-once、110k 会话分页、历史凭据、session archive 与用量聚合门禁。生产规模迁移锁时、最终 EXPLAIN/延迟、15 分钟内存和 live 部署仍必须在发布前单独验收。CPAMP 首次、重复、迟到重叠和新水位导入另由隔离 PostgreSQL acceptance 脚本验证，不能等同于 live 数据迁移。

memeloop web 的服务端应以稳定用户、订阅和计费周期身份调用 `PUT /internal/v1/entitlements`，用单调 `version` 对账该周期应有的信用额度；取消使用 `/internal/v1/entitlements/cancel`，只有订阅身份确实更换时才使用 `/internal/v1/entitlements/replace`。每个注册或 webhook 事件必须使用可持久恢复的确定性 `Idempotency-Key`。旧的 account grant/reversal API 只适合一次性人工调整，不应用来表达会员订阅生命周期；服务凭据只能保存在 memeloop web 后端，不能下发到浏览器。

数据库 DDL 位于 `migrations/`，当前工作树 schema 版本为 v35；每个版本都在同一事务和 PostgreSQL advisory transaction lock 下应用。PostgreSQL 的请求与事件表按天分区，v21 的非分区 locator 为全局请求/事件 ID 提供唯一所有权和叶分区时间坐标；v22 用事务化状态、UTC 日桶和滚动周边界明细避免预算热路径扫描完整账本；v23–v27 增加生成与请求 facts/日聚合、按凭据分页的会话投影、生成资产元数据和 CPAMP 来源摘要，避免统计与排障查询扫描宽历史表；v28–v35 增加可证明的 session-archive exact/unlinked 投影、生成 worker 的持久提交状态与 `preparing` 租约、按稳定凭据隔离的会话修复、上游 OAuth 刷新生命周期、CPA 原凭据到稳定 key 的一对一约束、受控 managed OAuth 导入映射，以及归档 staging lease/fence 状态机。SQLite 保留等价的小型测试 schema。连接池默认每进程最多 4 个连接且不预热，HTTP 上游连接池也有严格空闲上限；同步生图响应限制为 16 MiB、每副本最多两个并发缓冲，其他大对象继续流式归档，避免重现 CPA 的高内存占用。fresh PostgreSQL 17 的 v1–v35 全迁移与 v34/v35 定向并发门禁已通过；生产规模迁移锁时、最终 EXPLAIN/延迟、15 分钟内存和 live 部署门禁仍必须在发布前完成，不能用 SQLite/Cucumber 结果替代。

`ops/migrate-cpamp.sh` 是 PostgreSQL 增量导入器：首次导入所有 CPAMP usage/alias/price，之后从 checkpoint watermark 回看默认 24 小时，只把尚未存在的 event hash 写入请求明细、facts、请求日聚合及用量小时/日聚合。导入行会快照 USD、默认服务档位、协议、错误与时延桶；这些聚合只从本次成功取得 locator 的新 facts 产生，因此完整重放不会重复累计。回看窗口用于覆盖 CPAMP 延迟刷盘；确定性 UUID 与幂等插入保证周末切换前可以反复运行。`ops/kubernetes/cpamp-import-job.yaml` 提供了只读挂载 CPA Manager Plus PVC 的 K8s Job 模板，默认就是增量模式。`CPAMP_RESET_IMPORT=true` 仅用于重建 dogfood 导入租户，不应在生产增量切换中使用。

`ops/import-cpa-session-archive.sh` 在 CPAMP 身份导入后接入 `cpa-session-archive` v0.7.x 真实导出的 schema-v1 或 identity-aware schema-v2 JSONL，把精确匹配的请求/响应正文写入 BLAKE3 CAS，并补充可证明的会话观察。它默认只 dry-run，整批预检失败即零写入，支持 tenant/source checkpoint 和重放，且不会覆盖已有真实对象；当前通过的是 SQLite/CAS fixture，尚不是 PostgreSQL/S3 或 live session 迁移证据。

## 配置与插件

核心、key、service token、provider 账号、模型路由和生成价格的 JSON Schema 位于 `schemas/`，并由 `GET /internal/v1/schemas` 提供。`GET /internal/v1/provider-types` 还会返回每种 provider 贡献的配置与 credential Schema，前端直接交给 JSON Schema 表单渲染器。API key、OAuth 和无需认证的私有上游都是同一种稳定上游账号的 credential；替换可在同一账号上原子切换 API key/OAuth 并推进 generation，请求历史和模型路由继续引用同一个账号主键。credential 使用带认证加密后再写入数据库，并且管理 API 只返回脱敏元信息。

Copilot/Cursor 订阅登录入口为 `/internal/v1/oauth/subscription-bridge/start` 与 `/internal/v1/oauth/subscription-bridge/poll`。Token Center 只加密保存 bridge 返回的 opaque handle 和可选 bridge secret，真实 OAuth 状态继续隔离在 bridge 的持久卷中；下游 OpenAI Chat 请求会原生映射到 bridge `/v1/execute`，并继续执行同一套模型权限、额度、限流、计费和归档。Cursor 直接 PKCE 入口为 `/internal/v1/oauth/cursor/start` 与 `/internal/v1/oauth/cursor/poll`，刷新入口为 `/internal/v1/upstreams/{account_id}/oauth/refresh`。Cursor 原生 Connect/Agent Runtime 到公开协议的转换仍需对应 provider adapter，不能假装成无损 OpenAI 兼容。

`POST /internal/v1/imports/cpa/subscription-accounts` 可以上传 CPA 导出的 Copilot/Cursor auth JSON，并把 opaque handle 幂等迁移为稳定上游账号；API 不返回 handle、bridge secret 或原文件正文。`POST /internal/v1/imports/cpa/managed-oauth` 使用服务端 catalog 中受审核的适配器导入 Codex 与 legacy Gemini auth 文件：调用者不能指定 driver、base URL、刷新地址或账号配置，原文件也不会写入响应或日志。只有具备专用 transport 的 driver 才会对外声明可路由协议；未知的 Kimi 等私有 OAuth 文件继续 fail closed，不能把 token 文件机械伪装成通用 OAuth。

插件 ABI 位于 `wit/token-center.wit`；运行时使用 Wasmtime Component Model，每次调用限制 32 MiB、固定 fuel 和 epoch deadline，HTTP 只能访问 manifest 中由运维审核的精确 origin。声明 `kv` capability 的插件可以使用 PostgreSQL/SQLite 中按插件 ID 隔离的持久 KV，每个值上限 1 MiB、每个插件上限 16 MiB；未声明 capability 的调用会在 host 边界被拒绝。Provider contribution 可以携带 JSON Schema 和 `oauth_adapter`，由 `/internal/v1/oauth/provider-adapter/*` 执行固定的 PKCE adapter 协议，无需给插件任意进程内权限。声明 `buffered-v1` component adapter 后，第三方 provider 还能准备非 OpenAI 形状的有界上游请求并标准化非流式响应；credential 只由核心在 SSRF/同源/大小/超时检查后注入，组件不可见，模型权限、核心价格表计费和归档仍不可绕过。流式请求在该 ABI 明确 fail-closed。OCI 插件包格式、`plugin.json` 和 capability 限制见 `plugins/README.md`。Helm Chart 可从只读 ConfigMap/PVC 加载插件；生产 values 只引用外部 PostgreSQL/S3 Secret，不把凭证写入 ConfigMap。

React/Vite 管理端位于 `web/`：`/operator` 是 service token 控制面，提供租户聚合指标、模型/日期/错误分布、实时请求尾流、按需归档排障、统一上游提供商、路由、凭据、服务凭据和计费同步；`/portal` 是下游凭据只读统计，可查看生成任务的状态、费用、错误和归档对象。前端静态资产在镜像构建时生成，不依赖 Node.js 运行时。
