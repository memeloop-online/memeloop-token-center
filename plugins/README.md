# Plugin packages

本版本使用本地只读挂载作为简化且可审计的插件分发策略。每个插件各占一个目录，至少包含：

- `plugin.json`：插件 id、版本、WIT API 版本、扩展点、capability allowlist 和 JSON Schema provider contribution。
- `README.md`：使用说明。
- 可选 `plugin.wasm`：实现 `wit/token-center.wit` 的 Component Model 组件；纯 Provider/OAuth Schema contribution 可设 `"wasm": null`，traffic policy 则必须提供组件。
- 可选 `icon.png` 与配置 JSON Schema。

设置 `MTC_PLUGIN_DIR` 后，服务会在启动时编译目录下的组件；同名 provider 或不兼容的 `wit_version` 会让启动失败。Helm 用 `plugins.existingConfigMap` 或 `plugins.existingClaim` 只读挂载该目录：ConfigMap 可把一个插件包直接挂在根目录，PVC 则可在多个子目录放置多个插件包。

发现过程是 fail-closed 的：可见的一级子目录必须包含 `plugin.json`；plugin ID 和 provider ID 必须是 1–64 字符的 ASCII 小写字母、数字或连字符。目录符号链接、逃逸包目录的 Wasm 路径、重复或越界的插件/provider ID、无效 SemVer、与 `0.2.x` 不兼容的 WIT 版本、未知 manifest 字段和不受支持的 JSON Schema 都会阻止服务启动。升级插件应先在独立实例挂载新目录并重启验收；运行中的插件目录不可原地修改。OCI 拉取和签名验证不属于本版核心，部署系统应负责校验只读挂载内容的来源和 digest。

核心 key 鉴权、账本和 tenant 边界不可被插件替换。每次 traffic policy 调用使用独立 Store，限制 32 MiB 线性内存和 500 万 fuel。HTTP host call 仅允许 `plugin.json` 明确列出的精确 origin，禁止重定向，请求和响应各限制 16 MiB；方法只允许 `GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE`。插件可向获准 origin 发送 `Authorization` 或供应商 API key，但不能设置 `Host`、`Content-Length`、hop-by-hop、代理转发或方法覆盖 header；最多 64 个 header、解码后共 16 KiB，单个名称/值分别最多 256 字节/8 KiB。原始 URL 的 host 始终用于 HTTP Host、TLS SNI 和证书校验，单次 DNS 解析仍由核心固定。返回值是 `{status, headers, body_base64}` JSON。KV host call 必须显式声明 `{"kind":"kv"}`，数据持久化在 PostgreSQL（测试可用 SQLite）的插件 ID 命名空间中；key 只接受最多 256 字节的安全 ASCII 路径，每个 value 上限 1 MiB，每个插件总量上限 16 MiB。前端扩展保持声明式，只允许 JSON Schema、说明和动作表单，不注入任意 JavaScript。

Traffic policy 的 `decision.reason` 仅是非可信 guest 元数据：核心不会保留、原样记录或放进客户端错误响应，拒绝事件只记录经过 manifest 验证的 plugin ID 和 host-owned 固定 decision code。即使 manifest 声明 `{"kind":"log"}`，`host.log` 的 guest message 也不会原样写入日志；该 capability 只产生包含已验证 plugin ID 与固定事件码的有界日志事件。插件不得依赖 reason 或 log message 向用户传递信息，也不得把 prompt、credential、token 或其他客户数据放入这些字段。

Provider contribution 可选声明 `oauth_adapter`，且必须显式提供 `api_version: oauth-adapter-v1`、`flow_kind: cursor_pkce` 以及 `login_url`、`poll_url`、`refresh_url`。控制面用这一版本化 PKCE 协议访问端点：登录请求追加 `challenge`、`uuid`、`mode=login` 和 `redirectTarget=cli`，轮询请求追加 `uuid` 与 `verifier`，轮询/刷新响应使用 `accessToken`、`refreshToken`。插件加载时会验证 credential schema 能接收该 flow 的原始 OAuth 结果；核心添加的 header 默认值和刷新元数据不属于 provider schema/config。公网端点必须是 HTTPS；K8s 单标签、`.svc`/`.svc.cluster.local` 和私有 IP 可使用 HTTP。OAuth token 只进入核心加密 credential 表，不进入插件 KV、日志或 API 响应。

可执行 Provider 必须另外声明 `component_adapter.api_version=buffered-v1` 和不超过 4 MiB 的 `max_response_bytes`。核心先把 canonical JSON 交给组件 `prepare`，只接受安全 method、同源相对 path、非敏感 header 和有界 body；随后由核心解析并固定目标 URL、执行 SSRF/DNS pinning、添加同一个稳定 upstream account 的 API/OAuth credential、设置超时并读取有界响应，最后调用 `normalize`。组件始终看不到 credential。`stream=true`、组件声明 streaming、跨 origin path、Authorization/Cookie/Host 等 header、超限请求或响应、fuel/epoch/32 MiB 限制、trap 和无效 normalize 输出都 fail-closed，不回退到内置驱动。标准化结果中的 token usage 只用于核心价格表结算，组件不能绕过模型权限、余额、限流、归档或请求审计。

最小 manifest：

```json
{
  "id": "example-policy",
  "version": "1.0.0",
  "wit_version": "0.2.0",
  "capabilities": [
    { "kind": "log" },
    { "kind": "kv" },
    { "kind": "http", "allowed_origins": ["https://oauth.example.com"] }
  ],
  "contributions": {
    "traffic_policy": true,
    "providers": []
  }
}
```

可直接运行的 provider + OAuth + traffic policy + request rewrite 示例位于 `examples/plugins/policy-rewrite`。其中提交了可加载的 Component Model 二进制，也保留等价 WAT 供复核和可重现构建。
