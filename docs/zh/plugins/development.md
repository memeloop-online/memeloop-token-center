# 插件开发

本文覆盖开发一个 MTC 插件的最小闭环：实现 WIT 导出 → 编写 `plugin.json` → OCI 打包签名 → 交由运营者安装发布。接口的权威定义是 [token-center.wit](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)（当前包版本 `memeloop:token-center@0.2.0`）与[清单 Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)。

## WIT ABI

`plugin` world 中，组件 import 宿主能力、export 扩展点：

```wit
interface host {
  log: func(level: string, message: string);
  kv-get: func(key: string) -> result<option<list<u8>>, string>;
  kv-put: func(key: string, value: list<u8>) -> result<_, string>;
  http-request: func(method: string, url: string, headers-json: string, body: list<u8>) -> result<list<u8>, string>;
}
```

组件可导出两个接口（按清单声明启用）：

- `traffic-policy.post-auth(context, request-json) -> decision`：认证后的有界流量策略/请求改写钩子。
- `upstream-provider`：`list-models`、`quote`、`prepare`、`normalize`——缓冲式上游 Provider 适配。

关键记录：

```wit
record request-context {
  tenant-id: string, principal-id: string, key-id: string,
  protocol: string, model: string, config-json: string,
}
record decision {
  allow: bool, reason: option<string>, model: option<string>,
  upstream-account-id: option<string>, request-json: option<string>,
}
record metering {
  currency: string, amount: string,
  input-tokens: u64, output-tokens: u64, estimated: bool,
}
```

## 流量策略示例

`post-auth` 返回的 `decision` 示例：

```json
{
  "allow": true,
  "reason": "matched allowlist",
  "model": null,
  "upstream_account_id": null,
  "request_json": null
}
```

- `model` 非空表示请求改写；改写后的模型仍会重新经过路由授权校验。
- `upstream_account_id` 只是已授权集合内的**偏好排序**，不能新增或覆盖授权。
- `reason` 与 `log` 消息是非受信元数据：不会原样进入日志或错误响应，不要用来向用户传递信息，更不要放入提示词或凭证。

## Provider 组件（buffered-v1）

可执行 Provider 在清单中声明 `component_adapter.api_version = "buffered-v1"` 与 `max_response_bytes`（≤ 4 MiB）。交互顺序：

1. 宿主把规范化、无凭证的请求 JSON 交给组件 `prepare`，组件返回缓冲信封：安全 method、同源相对 path、非敏感 header 与 base64 body，并断言 `streaming=false`。
2. 宿主校验信封、固定目标 URL、执行 SSRF/DNS 防护、**在调用返回后才注入**该稳定账户的 API/OAuth 凭证，设置超时并读取有界响应。
3. 宿主把无凭证的响应信封交给 `normalize`，组件返回公开协议响应与 token 用量；用量只用于核心价格表结算。

组件始终看不到凭证。声明 `stream=true`、跨 origin 路径、敏感 header、超限或组件 trap 都会 fail-closed，不会回退到内置驱动。

## OAuth 声明

Provider 可以声明 `oauth_adapter`（`api_version: "oauth-adapter-v1"`，`flow_kind: "cursor_pkce"`，提供 `login_url`、`poll_url`、`refresh_url`），由控制面执行版本化 PKCE 协议；也可以声明 `authorization_code_pkce` 使用通用授权码流程。两种方式下 token 都只进入核心加密凭证表，组件与插件 KV 不接触 token。

## 最小清单

```json
{
  "id": "example-policy",
  "version": "1.0.0",
  "wit_version": "0.2.0",
  "capabilities": [
    { "kind": "log" },
    { "kind": "kv" },
    { "kind": "http", "allowed_origins": ["https://plugin-api.example.com"] }
  ],
  "contributions": {
    "traffic_policy": true,
    "request_rewrite": true,
    "configuration": {
      "schema": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "mode": { "type": "string", "enum": ["safe", "fast"] }
        }
      },
      "default": { "mode": "safe" }
    },
    "providers": []
  }
}
```

规则要点：

- `id` / provider id 为 1–64 位小写字母、数字或连字符；`version` 为 SemVer；`wit_version` 必须与 `0.2.x` 兼容；未知清单字段会被拒绝。
- 纯声明式 Provider/OAuth 包可设 `"wasm": null`；声明了 `traffic_policy` 必须包含组件。
- 配置 Schema 使用 Draft 2020-12 的受限声明子集：允许有界本地 `#/*` 引用，禁止远程/文件引用与 `writeOnly`，大小、深度与节点数有上限。
- `http` host 调用只允许 `GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE`，仅访问声明的 origin，请求与响应各有界。

## OCI 打包与签名

插件以「一层一个文件」的 OCI 制品分发（不使用 tar 层）：

| 内容 | 媒体类型 |
| --- | --- |
| config | `application/vnd.memeloop.token-center.plugin.config.v1+json`（`{"format_version":1}`） |
| `plugin.json` | `application/vnd.memeloop.token-center.plugin.manifest.v1+json` |
| Wasm（至多一个） | `application/vnd.wasm.content.layer.v1+wasm` |
| README、图标等 | `application/vnd.memeloop.token-center.plugin.asset.v1` |

artifact type 为 `application/vnd.memeloop.token-center.plugin.v1`。使用标准工具发布并签名（仓库地址为虚构示例）：

```bash
printf '{"format_version":1}' > artifact-config.json
oras push --artifact-type application/vnd.memeloop.token-center.plugin.v1 \
  --config artifact-config.json:application/vnd.memeloop.token-center.plugin.config.v1+json \
  ghcr.io/example/token-center-plugins/example-policy:1.0.0 \
  plugin.json:application/vnd.memeloop.token-center.plugin.manifest.v1+json \
  plugin.wasm:application/vnd.wasm.content.layer.v1+wasm \
  README.md:application/vnd.memeloop.token-center.plugin.asset.v1 \
  assets/operator-ui.mjs:application/vnd.memeloop.token-center.plugin.asset.v1
digest="$(oras resolve ghcr.io/example/token-center-plugins/example-policy:1.0.0)"
cosign sign --key cosign.key "ghcr.io/example/token-center-plugins/example-policy@${digest}"
```

安装引用必须固定到 digest（不使用 tag）。运营侧的安装策略会配置可信仓库 allowlist 与 Cosign 公钥，验签失败或来源不在 allowlist 的制品无法安装。

## 边界说明

- 本文列出的扩展点构成当前 ABI。Operator React 模块使用经过签名、按摘要寻址的 `component_v1` 契约；流式请求钩子与插件自创账户/路由不在该 ABI 中。
- 组路由使用独立的 `group-routing-plugin` world，见[组路由](routing.md)；Operator 界面扩展见[Operator UI](operator-ui.md)。
