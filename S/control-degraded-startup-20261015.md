# 数据库故障时网站与管理入口：source 根因与最小修复

日期：2026-10-15。隔离 worktree：`/home/user/workspace/mtc-control-degraded-startup-20261015`；分支：`fix/control-degraded-startup-20261015`。基于当时最新 `origin/master`：`ee285c49`。未改共享主工作区的 `S/`、`scratch/` 或其他人的 dirty 文件。

## 已确认的源码根因

1. 原 `src/main.rs` 的 `Command::Serve` 先 `AppState::initialize(...).await?`，成功后才 `TcpListener::bind`；初始化错误被丢弃为 `application_initialization_failed`，主函数返回失败退出。原 `src/lib.rs` 初始化依赖数据库连接、可选迁移、插件数据库配置验证及库存加载。因此 DB 无法连接或必要的初始化数据库操作失败时，没有 HTTP listener，静态壳也不可能响应；容器退出可进一步进入重启退避。这是源码上的确定依赖，不能靠 UI 文案修复。
2. `src/api/web.rs` 从镜像内 `MTC_WEB_ROOT`（默认 `/usr/share/memeloop-token-center/web`）读取现有构建产物；`Dockerfile.release` 已随服务镜像携带 web。没有独立静态服务；`/operator`、`/portal`、`/ui-assets` 与业务路由共处应用进程。
3. 产品默认 `charts/.../values.yaml` readiness 为 `/readyz`，startup 为 `/healthz`，liveness 为 `/livez`。`src/api/health.rs` 在 DB 检查失败时保持真实 503。产品 chart 入口原来把静态和业务路径都送往相同、只包含 Ready Pod 的 Service。因此只保住 listener 仍不足以保证默认部署中的入口可达。
4. 必须区分产品默认与现场：本机现有 GitOps `apps/memeloop-token-center/values.yaml` 及 `gitops-mtc-main124-b9-20261015` worktree 的相同文件，明确将 kubelet readiness 临时设为 `/livez`，注释说明为了避免撤掉唯一 control 端点。因此不能把产品默认 `/readyz` 的撤端点直接当作此次生产实际根因。一次只读 `kubectl get deployments` 遇到当前配置的 Kubernetes API connection refused，未取得 live spec；没有重复施压或改变集群配置。
5. 原 `src/error.rs` 将 pool timeout 映射为 `service_overloaded`，其他 SQLx 错误通常归为 `internal_error`，存储满与连接不可用没有明确公开分类。`overloaded` 不足以证明 CPU 忙，也不能据它断定磁盘满。

用户已确认的现场证据是 PG 原 40 GiB 满，造成 503 / headers 前 reset；扩容由外部运维独占，后续只读审计约 80 GiB、35% 已用。本任务沿用该现场结论，没有亲自测量卷，也没有把源码分析冒充历史 Pod 日志、精确失败 SQL 或当前空间状态的独立证明。若 PG 已无法返回 SQLSTATE，应用只能报告连接不可用，不能臆断为存储满。

## 实现范围

- `serve` 先绑定现有端口，提供现有静态壳、磁盘资源和故障健康响应；后台初始化失败后间隔 30 秒重试，与单个用户请求无关。成功才安装原 `router_for_role`，认证、授权、计费、转发和 worker 仍沿原路径。
- 启动故障时，全部业务 API 都返回明确 503，不执行私密读取、写入、授权或上游请求；插件模块不使用磁盘文件替代验证。`/livez` 保持进程存活语义，`/readyz` 保持真实 503，不伪造 dependency readiness。
- 在原 chart Service 模板中增加相同 Pod/端口的 `-web` ClusterIP Service，仅此 Service `publishNotReadyAddresses: true`；原 API Service 保持默认 readiness 撤端点规则。原 chart Ingress 的 `/portal`、`/operator`、`/ui-assets`、`/healthz` 送往 `-web`，`/v1`、`/self`、`/internal/v1` 仍送往原 Service。沿用现有私有 control 入口的 TLS、来源白名单和 NetworkPolicy；未新增外网业务入口或 pipeline。
- 沿现有资源前缀提供 `/ui-assets/service-status`，两种启动状态下都可读，返回固定公开 health 元数据及既有 `error.code/message` 形状。就绪后复用既有 5 秒 readiness 缓存，不新增数据库写探针。
- DB 明确返回 PG `53100` / SQLite `FULL` 才分类 `database_storage_full`；连接 IO/TLS、PG `08*` / `57P01/2/3`、已关闭连接池分类 `database_unavailable`。不把 pool timeout、内存不足、连接数或锁等待推断成 CPU 忙或磁盘满。不改 628 的任何 pool 文件。
- 页面下方使用现有 AppShell 与 Fluent Button 显示中文/英文真实问题。保留最近明确 DB 失败的固定 code 与时间戳，清楚标为历史观察；读取健康不能确认写入恢复。无凭据状态重置、缓存授权扩大或保存成功假象。

## Kimi 可消费的 API 合约

继续使用现有 `ApiError.status/code/message/requestId`，无需修改 parser；没有改 `useOperatorScope.ts`、`OperatorAccessSettings`、`Operator.tsx` 或其 contracts。

| 情况 | HTTP | error.code |
| --- | --- | --- |
| 启动尚未完成 | 503 | service_initializing |
| 启动时明确存储满 / 业务数据库操作明确存储满 | 503 | database_storage_full |
| 明确数据库连接不可用 | 503 | database_unavailable |
| DB 初始化失败但无更具体驱动证据 | 503 | database_initialization_failed |
| 其他 AppState 初始化失败 | 503 | application_initialization_failed |
| 健康探测发现 schema 未就绪 | 503 | database_schema_outdated |

这些是依赖失败，不能因此要求用户重填/丢弃服务凭据，也不能将未知身份视为已授权。数据库故障时不把受保护数据、空列表或写入结果伪造为成功。API 原 readiness Service 可能由入口直接返回通用 503，页面仍能通过 `-web` 的状态路径读取原因。状态路径的 `last_database_error: {code, observed_at}` 是本进程最后观察到的固定诊断，`observed_at` 为 Unix 秒；不是集群持久事实、容量监测或当前可写证明。多副本可能观察不同历史，不据此声称全局恢复。

## 部署边界与残余风险

没有部署、合并、改卷、重启或清理数据；没有生产业务写请求。此 PR 的源码和 chart 必须由发布 owner 一起消费；当前 GitOps 使用 vendored/pinned chart、手写 Ingress，需要由其 owner 同步 `-web` Service 及静态路径 backend。只发布新 binary、仍把网站流量送往 readiness 撤端点的旧 Service，不构成完整可用性修复。直接访问原 NodePort/LoadBalancer 的静态请求仍受原 Service readiness；本次目标是既有网站 Ingress。

静态产物缺失仍明确 503；配置解析失败（发生在 serve 分支前）、镜像/Pod 无法启动、节点/入口/网络故障不属于 DB 降级承诺。现有 startup/liveness 必须探测进程存活路径，不能改成依赖 `/readyz`。不提供脱离授权的数据读取或免费转发。

## 审查与验证

第一轮只读源码审查已覆盖启动顺序、原 auth 链、静态路径验证、no-store、安全响应头、health 语义、Ingress 白名单及 ServiceMonitor 不选择 `-web` 组件。仅执行独立 Rust 文件格式化；没有运行本地 tests/build/check/install，也不把格式化当成测试通过。

新增 CI 回归：故障时原静态壳与资源 200、liveness 200/readiness 503、私密/写入/转发路径拒绝、恢复后原认证仍拒绝无效凭据、驱动分类证据边界、Helm 静态与 API backend 分离、中英文 footer/历史写入失败/320px 布局。使用既有 GHA；提交/PR 和实际 CI 状态在交付时补充。最多两轮 review，无 sub-agent，无 Wiki。


PR：[#634](https://github.com/memeloop-online/memeloop-token-center/pull/634)，首提交 `5fc505ad`。第一轮 [GHA 38767919986](https://github.com/memeloop-online/memeloop-token-center/actions/runs/38767919986) 的安全、依赖、migration-smoke、memory-binary/service release input 构建通过；API contract 缺少新状态路径边界、原插件校验失败类别回归、新浏览器英文词序断言失败。第二轮修正：补全 OpenAPI/边界及 unknown 初始化健康形状，仅保留明确数据库错误细节而保留普通插件校验 `InitializationError::Plugin`，修正英文断言词序。未放宽 auth、测试门禁或更改他人 plugin 文件。最终 CI 按实际状态交付，不声称首轮通过。
