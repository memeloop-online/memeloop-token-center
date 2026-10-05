# 代理组正式管理 API 契约 v1

状态：后端实现及 GHA 验证中；供父线程协调 UI，不表示已部署或 UI 上线。
基线：PR458 已合并为 `07b9220c18c59c5cc125caaa2478635f2e2e8765`。
复用其内存选组、安全 fallback 和迁移 116，不重复实现。

## 权限与通用约定

- 本轮仅支持原生 `openai-codex` 账号；操作入口要求全局操作员的
  `providers:write` 权限，并校验目标租户与账号/组归属。
- GET 的租户参数为 `tenant_external_id` query；写请求将该参数放在 JSON body。
- 所有响应使用 `Cache-Control: private, no-store`。时间为 Unix 毫秒，ID 为 UUID。
- 组名称和成员标签为 1–64 字符；每组 1–4 个不同的私网 `socks5h://`
  地址。支持私网 IP 及 Kubernetes Service DNS：`service.namespace.svc`、
  `service.namespace.svc.cluster.local`，例如 `mihomo.egress.svc:1080`。
  新增/替换地址复用现有私网出口解析校验，拒绝公网、loopback、link-local、
  混合公网 DNS 结果；不探测 SOCKS/TLS 健康，也不推断某出口持续健康或死亡。
  地址最长 2048 字节，包含认证信息时同样按秘密处理。
- 代理 URL 只写不回显；更新现有成员时省略 `proxy_url` 表示保留原值。
  成员 ID 稳定且由服务端生成，客户端不能用数组下标充当成员身份。
- 未知请求字段拒绝。版本由服务端生成，客户端以 expected 字段做 CAS。
- 配置以数据库为权威来源，URL 使用现有凭据加密机制落盘。
  配置与脱敏审计在事务内提交；失败不产生部分绑定或部分成员更新。

## Endpoints

Codex 请求转发只读取自身使用的账号配置字段，不因额外的账号元数据或
`transport_policy` 扩展字段拒绝请求，也不将这些配置字段透传给上游。
代理组绑定戳由代理选择层处理，不能在固定传输校验中再次维护键名白名单。
固定上游地址、凭据、已知传输参数值以及代理组选择一致性校验仍然生效；
上述管理 API 的写入校验与请求转发兼容性是两个独立职责。

| Method | Path | 成功响应 |
| --- | --- | --- |
| GET | `/internal/v1/transport-proxy-groups?tenant_external_id=tenant-a` | 200 `{ "items": [Group] }` |
| POST | `/internal/v1/transport-proxy-groups` | 201 `Group` |
| GET | `/internal/v1/transport-proxy-groups/{group_id}?tenant_external_id=tenant-a` | 200 `Group` |
| PUT | `/internal/v1/transport-proxy-groups/{group_id}` | 200 `Group` |
| DELETE | `/internal/v1/transport-proxy-groups/{group_id}` | 204，无 body |
| GET | `/internal/v1/upstreams/{account_id}/transport-proxy-group?tenant_external_id=tenant-a` | 200 `Binding` |
| PUT | `/internal/v1/upstreams/{account_id}/transport-proxy-group` | 202 `Binding` |
| DELETE | `/internal/v1/upstreams/{account_id}/transport-proxy-group` | 202 `Binding` |

POST 创建：

```json
{
  "tenant_external_id": "tenant-a",
  "name": "Codex 出口组",
  "members": [
    {"label": "出口 A", "proxy_url": "socks5h://10.20.30.40:1080"},
    {"label": "出口 B", "proxy_url": "socks5h://10.20.30.41:1080"}
  ]
}
```

Group 响应示例；`bound_account_count` 是配置绑定数，不是活跃连接数：

```json
{
  "id": "019c0000-0000-7000-8000-000000000001",
  "tenant_external_id": "tenant-a",
  "name": "Codex 出口组",
  "version": 1,
  "members": [
    {
      "id": "019c0000-0000-7000-8000-000000000002",
      "label": "出口 A",
      "scheme": "socks5h",
      "remote_dns": true,
      "has_auth": false
    },
    {
      "id": "019c0000-0000-7000-8000-000000000003",
      "label": "出口 B",
      "scheme": "socks5h",
      "remote_dns": true,
      "has_auth": false
    }
  ],
  "bound_account_count": 0,
  "created_at": 1791072000000,
  "updated_at": 1791072000000
}
```

PUT 更新完整成员列表；无 `id` 的项为新增成员并要求 `proxy_url`，有 `id`
的项只能引用当前组成员。列表顺序定义候选顺序，不触发健康出口自动切回。

```json
{
  "tenant_external_id": "tenant-a",
  "expected_version": 1,
  "name": "Codex 出口组",
  "members": [
    {"id": "019c0000-0000-7000-8000-000000000002", "label": "出口 A"},
    {"id": "019c0000-0000-7000-8000-000000000003", "label": "出口 B"}
  ]
}
```

已绑定组允许重命名、修改标签、增加/移除成员、替换 URL 和调整候选顺序。
保留 ID 和 URL 均未改变的健康成员选择。移除或替换已绑定组的任意成员时，
PUT 必须额外传 `replacement_member_id`，指向提交列表中已有 ID 的成员；
仅当本 Pod 当前选择无法保留时才使用它。缺失/非法替代成员返回 400。
新增成员 ID 由服务端生成；若要用全新成员替代，可先添加并取得 ID，再提交移除。
组版本、所有绑定账号的 credential generation / transport revision 和基础代理
原子更新；账号 binding_version 不变。新请求等待对应快照，在途请求保留原客户端。
不要求先解绑，不触发 OAuth token 刷新。已绑定组的整体 DELETE 仍返回
`proxy_group_in_use`；成员维护不受该限制。

DELETE 组请求：

```json
{"tenant_external_id": "tenant-a", "expected_version": 2}
```

## 账号绑定

PUT 绑定请求：

```json
{
  "tenant_external_id": "tenant-a",
  "group_id": "019c0000-0000-7000-8000-000000000001",
  "expected_group_version": 2,
  "initial_member_id": "019c0000-0000-7000-8000-000000000002",
  "expected_binding_version": 0,
  "expected_credential_generation": 7,
  "expected_updated_at": 1791072000000
}
```

- 从未管理过的账号 `binding_version` 为 0；每次绑定/解绑单调增加，解绑后
  保留版本墓碑，不能回到 0 造成 ABA。
- `initial_member_id` 是首次绑定/更换组时的显式起点。对同一组的重复配置不能
  将已经健康地切到其他成员的运行时选择重置为起点。
- 绑定写入、基础代理更新、账号凭证代次及 transport revision 更新原子提交。
  现有单代理修改入口遇到有效组绑定返回冲突，不能绕过组管理。
- 配置提交成功不等于所有 Pod 已应用；202 响应明确保留异步生效语义。

Binding 响应示例：

```json
{
  "account_id": "019c0000-0000-7000-8000-000000000004",
  "tenant_external_id": "tenant-a",
  "binding_version": 1,
  "group_id": "019c0000-0000-7000-8000-000000000001",
  "group_version": 2,
  "initial_member_id": "019c0000-0000-7000-8000-000000000002",
  "credential_generation": 8,
  "updated_at": 1791072001000,
  "runtime": {
    "scope": "this_process",
    "configuration_state": "pending",
    "applied_binding_version": null,
    "applied_group_version": null,
    "selected_member_id": null,
    "observed_at": 1791072001000
  }
}
```

`runtime.configuration_state` 为 `unbound | pending | applied | unavailable`。
`applied` 仅说明处理本次管理请求的进程已加载匹配配置，不代表全部 gateway/OAuth
Pod 已生效，也不代表代理健康。无本地流量/选择时 `selected_member_id` 为 null。
不得将后台持久化的最后选择伪装成实时或跨 Pod 全局选择。

DELETE 解绑请求必须显式选择保留的单代理成员，不提供 DIRECT 回退：

```json
{
  "tenant_external_id": "tenant-a",
  "expected_binding_version": 1,
  "expected_group_version": 2,
  "expected_credential_generation": 8,
  "expected_updated_at": 1791072001000,
  "single_proxy_member_id": "019c0000-0000-7000-8000-000000000003"
}
```

解绑返回的 Binding 中 `group_id`、`group_version`、`initial_member_id` 为 null，
`binding_version` 与账号代次增加。旧环境变量配置不得因解绑而重新激活该账号的组。

## 错误

沿用现有 `{ "error": { "code": "...", "message": "..." } }` 外壳，不回显 URL：

| HTTP | code | 含义 |
| --- | --- | --- |
| 400 | `invalid_request` | 格式、地址、成员数或账号类型不支持 |
| 401/403 | `unauthorized` / `forbidden` | 既有认证/权限规则 |
| 404 | `not_found` | 当前租户下目标不存在 |
| 409 | `proxy_group_version_conflict` | 组版本已改变 |
| 409 | `proxy_group_binding_conflict` | 绑定版本或账号代次/updated_at 已改变 |
| 409 | `proxy_group_in_use` | 整体删除被绑定组或绕过组修改单代理 |
| 409 | `proxy_group_capacity_exceeded` | 超出有界配置预算 |
| 503 | `service_overloaded` | 写入暂不可用；不得返回假成功 |

## 运行时与持久化边界

- 正式配置写入必须持久化成功；运行时选择仍为已有最佳努力持久化，不提升为
  跨 Pod 共识。配置版本与选择代次分开，健康出口不轮询、不自动 failback。
- 后台加载并发布有界快照；最多 256 个配置组、256 个绑定账号，每组最多四成员，
  活跃绑定配置维持 256 KiB 序列化预算。超限拒绝管理写入，不静默截断。
- 现有账号/路由快照携带绑定标识及版本；选组只验证内存中的对应版本，缺失或
  过期时 fail closed，不临时查 DB，也不降级 DIRECT。
- 使用独立后台连接与顺序工作，不能用 timeout 宣称底层 DB 操作已被硬终止。
- 正式绑定/解绑墓碑优先于旧 Secret JSON；正常创建、选择、维护代理组不要求
  修改 Secret 或重启。旧入口仅兼容历史配置，不能作为管理功能验收替代品。
- 只复用 458 的 connect-only fallback；送达不明、HTTP/2 reset/GOAWAY、body
  failure 和已输出请求不因管理组存在而重放。
- 116 已发布，不修改；配置表、绑定/解绑墓碑使用新的独立迁移，编号落盘前
  再核对并协调。115/#455 原样保留，114/#422 不占位。

## 独占实现范围与验收

本任务负责 Rust API、数据库配置、运行时接入、OpenAPI 和对应后端测试；不修改
`web/`。共享路由/迁移注册由本任务串行做最小接入，不修改 423/457 的归档、
会话或转发持久化实现。只用现有 worktree 或确有并行需要的隔离 worktree。

迁移暂用 117；已检查本地并行 worktree 与当前远端基线无 117，合并前父线程仍需
协调最终编号。115/116 原样保留，不占用 114/#422。

按用户授权直接实现及 GHA。验收包含权限与租户隔离、加密/脱敏、CRUD、
CAS 冲突、绑定原子性及墓碑、禁止 DIRECT、后台配置生效与旧版本拒绝、健康
选择保持、未知送达不重放，以及 SQLite/PostgreSQL 迁移和重放。
所有 build/test 在 GHA 执行；不修改生产账号、代理或 Secret，不创建重复 PR。
