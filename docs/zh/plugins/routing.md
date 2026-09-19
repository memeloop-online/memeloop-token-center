# 组路由（group-routing-v1）

组路由是一个独立的可选 ABI：插件对宿主**已经授权**的候选账户做排序，并给出有界的健康建议。它不创建授权、不接触凭证与请求正文。WIT 定义见 [token-center.wit](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit) 的 `group-routing-plugin` world：

```wit
interface group-routing-v1 {
  plan: func(input-json: string) -> result<string, string>;
  observe: func(input-json: string) -> result<string, string>;
}
```

## 宿主如何使用策略

组路由策略只处理宿主已经授权的候选。部署可以为分组选择原生策略或已安装的插件策略，并为多个命中分组提供稳定优先级；选择策略本身不会授予新的模型或账户权限。

策略代码缺失、无效或 trap 时，宿主回退到原生处理，请求继续使用核心的授权、健康和预算边界。

## plan：规划

输入示例：

```json
{
  "tenant_id": "tenant",
  "seed": 42,
  "remaining_deadline_ms": 1000,
  "config": {},
  "candidates": [
    {
      "tenant_id": "tenant",
      "route_id": "route",
      "account_id": "account",
      "generation": 1,
      "health": "transient"
    }
  ]
}
```

- `health` ∈ `healthy`、`transient`、`hard_quota`、`authentication`。
- `remaining_deadline_ms` 是核心原生预算的剩余值，不是可续期的插件超时；plan 不能延长它。
- `seed` 在宿主选择范围内稳定，用于可复现排序，不是授权令牌。

输出示例：

```json
{
  "candidates": [
    {
      "tenant_id": "tenant",
      "route_id": "route",
      "account_id": "account",
      "generation": 1,
      "allow_transient_probe": true,
      "cooldown_ms": 1000,
      "recovery_wait_ms": 500,
      "recheck_ms": 100,
      "stickiness": false
    }
  ]
}
```

输出契约：

- 结果必须是输入候选身份的**精确排列**（含租户与凭证代）：不允许过滤、复制、注入或修改身份。
- `allow_transient_probe` 只能对 `transient` 候选为 true；`hard_quota` / `authentication` 状态不能被插件复活。
- `stickiness` 让候选进入宿主的稳定排序层（会话种子 rendezvous），单纯的 0 冷却或 sticky 不等于准入。
- 全部字段必填、未知字段拒绝；最多 1024 个候选、1 MiB 输入/输出；标识符非空 ≤128 字节；延迟字段 ≤300000 ms，`recovery_wait_ms` 还必须落在剩余 Deadline 内。

## observe：终态观测

请求终态时宿主再次调用组件，输入把 `candidates` 换成单个 `candidate` 加一个 `outcome`（`success`、`transient_failure`、`hard_quota`、`authentication`、`cancelled`），输出是与 plan 候选同形的单个指令对象，身份必须与输入一致。观测不能发起等待、探测或重放；长流结束后 `remaining_deadline_ms` 可能为 0，此时必须返回 `recovery_wait_ms: 0`。

## 原生健康模式

清单中声明 `"health_policy": "native"` 时，插件排序照常生效，但宿主不采用插件的健康指令：准入、冷却、探针与恢复全部走原生路径，`observe` 也不会被调用。这适合只想做候选偏好的策略包。

## 可选额度上下文

签名清单可以用 `capabilities: [{ "kind": "group_routing_quota" }]` 选择加入（要求 `health_policy: "native"`）。加入后 `plan` 输入增加 `quota_context`：

```json
{
  "quota_context": {
    "version": "account-windows-v1",
    "now_ms": 1000,
    "accounts": [
      {
        "account_id": "authorized-account-id",
        "generation": 7,
        "provider": "example-oauth",
        "observed_at": 900,
        "valid_until": 1100,
        "windows": [
          {
            "id": "summary",
            "period_seconds": 604800,
            "reset_at": 2000,
            "reset_is_estimated": false,
            "remaining_fraction": 0.5,
            "exhausted": false
          }
        ]
      }
    ]
  }
}
```

时间为 Unix 毫秒；窗口的周期、重置时间、剩余比例与耗尽状态都可以为 `null`——未知就是未知，宿主不会合成「可用」。未声明该能力的插件收到的输入与之前完全一致（不含 `quota_context` 字段）。

## 隔离

每次调用有独立的 fuel、内存与最多 100 ms 执行时间；路由组件没有网络与 KV 访问，即使同包其他贡献声明了这些能力。请求从规划到终态观测固定使用同一份已编译组件与清单快照，中途升级插件不改变在途请求的行为。
