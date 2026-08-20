# Plugin packages

运行时只读取本地只读挂载；生产部署可先由 `install-plugin-oci` init container 从 OCI registry 拉取并验证插件，再把同一 volume 只读挂给 Token Center。每个插件各占一个目录，至少包含：

- `plugin.json`：插件 id、版本、WIT API 版本、扩展点、capability allowlist 和 JSON Schema provider contribution。
- `README.md`：使用说明。
- 可选 `plugin.wasm`：实现 `wit/token-center.wit` 的 Component Model 组件；纯 Provider/OAuth Schema contribution 可设 `"wasm": null`，traffic policy 则必须提供组件。
- 可选 `icon.png` 与配置 JSON Schema。

设置 `MTC_PLUGIN_DIR` 后，服务会在启动时编译目录下的组件；同名 provider 或不兼容的 `wit_version` 会让启动失败。Helm 用 `plugins.existingConfigMap` 或 `plugins.existingClaim` 只读挂载该目录：ConfigMap 可把一个插件包直接挂在根目录，PVC 则可在多个子目录放置多个插件包。

发现过程是 fail-closed 的：可见的一级子目录必须包含 `plugin.json`；plugin ID 和 provider ID 必须是 1–64 字符的 ASCII 小写字母、数字或连字符。目录符号链接、逃逸包目录的 Wasm 路径、重复或越界的插件/provider ID、无效 SemVer、与 `0.2.x` 不兼容的 WIT 版本、未知 manifest 字段和不受支持的 JSON Schema 都会阻止服务启动。服务先用 checked-in manifest Schema 校验原始 JSON，再由 Serde 解析，避免默认值掩盖缺失字段。Provider 配置采用受限的 Draft 2020-12 声明式 profile：允许有界的本地 `#/*` 引用，禁止远程/文件引用和浏览器渲染器未实现的关键字；Schema 限制 256 KiB、32 层和 4096 节点，提交实例限制 1 MiB。升级插件应先在独立实例挂载新目录并重启验收；运行中的插件目录不可原地修改。

## OCI 发布与安装

OCI artifact 必须使用 `application/vnd.memeloop.token-center.plugin.v1` artifact type 和 `{"format_version":1}` config。它不是 tar 包，而是“一层一个文件”：`plugin.json` 使用 `application/vnd.memeloop.token-center.plugin.manifest.v1+json`，至多一个 Wasm 使用 `application/vnd.wasm.content.layer.v1+wasm`，README、图标和 JSON Schema 等使用 `application/vnd.memeloop.token-center.plugin.asset.v1`；每层必须带 ORAS 默认生成的 `org.opencontainers.image.title` 相对路径。这样安装器无需解压归档，绝对路径、`..`、`.`、空路径段、反斜线、重复路径和路径穿越都会直接拒绝。

可以用标准 ORAS 与 Cosign 发布；安装引用最终必须固定到 digest，不能使用 tag：

```bash
printf '{"format_version":1}' > artifact-config.json
oras push --artifact-type application/vnd.memeloop.token-center.plugin.v1 \
  --config artifact-config.json:application/vnd.memeloop.token-center.plugin.config.v1+json \
  ghcr.io/example/token-center-plugins/example-policy:1.0.0 \
  plugin.json:application/vnd.memeloop.token-center.plugin.manifest.v1+json \
  plugin.wasm:application/vnd.wasm.content.layer.v1+wasm \
  README.md:application/vnd.memeloop.token-center.plugin.asset.v1
digest="$(oras resolve ghcr.io/example/token-center-plugins/example-policy:1.0.0)"
cosign sign --key cosign.key \
  "ghcr.io/example/token-center-plugins/example-policy@${digest}"
```

安装器必须同时给出精确 registry/repository 来源 allowlist 和至少一个 Cosign 公钥；多个公钥仅用于无停机轮换，任一可信公钥验签成功即可。验签委托给固定绝对路径 `/usr/local/bin/cosign` 的官方 Cosign v3.1.3（该版本修复 GHSA-fx35-mq7g-6g98），运行时会严格核对其 `gitVersion`。公钥模式使用 `--insecure-ignore-tlog`，以显式配置的离线公钥作为信任根；不会开启 SCT 忽略、deprecated offline 或 private-infrastructure 模式。私有 registry 的用户名、PAT/密码或直接 bearer token 只从挂载文件读取，不接受命令行明文或 Secret env。生产 CLI 固定 HTTPS；HTTP 只存在于 Rust mock-registry 测试入口。

```bash
cargo run --features plugin-distribution --bin install-plugin-oci -- \
  "ghcr.io/example/token-center-plugins/example-policy@sha256:<64-hex>" \
  --plugin-dir /plugins \
  --allowed-source ghcr.io/example/token-center-plugins/example-policy \
  --cosign-public-key /var/run/secrets/plugins/cosign.pub
```

生产使用独立的 `memeloop-token-center-plugin-installer@sha256:...` init-container 镜像；Cosign 不进入长期运行的服务镜像。Helm 的 `plugins.ociInstaller` 只接受 digest-pinned installer 镜像和 artifact，公钥及可选 registry auth 通过 Secret 文件卷挂载，`/tmp` 是有上限的内存卷，插件输出是 Pod 专属 `emptyDir`。init container 非 root、只读 rootfs、无 service-account token、drop ALL，安装完成后服务容器只读挂载输出卷。每次安装失败都会阻止 Pod 启动；已经存在的插件目录不会被覆盖。

安装器先验来源和 Cosign 签名，再读取 OCI manifest 并在下载 blob 前检查 descriptor：最多 64 个文件、总计 80 MiB、`plugin.json` 1 MiB、Wasm 64 MiB、单个 asset 8 MiB、config 16 KiB。blob 逐个流式写入有硬上限的 staging 文件，并由 OCI client 校验 descriptor digest；随后复用运行时的 manifest/schema/Wasm 路径校验。最后用 Linux `renameat2(RENAME_NOREPLACE)` 原子发布为 `/plugins/<plugin-id>`，绝不覆盖已有插件，失败会清理隐藏 staging 目录。成功目录包含不带凭据的 `.mtc-oci-install.json` 来源/digest/签名策略收据。升级应安装到新的空 volume 并经过独立实例验收后切换 Deployment；当前 MVP 有意不在运行目录内原地替换版本。

核心 key 鉴权、账本和 tenant 边界不可被插件替换。`traffic_policy` 声明放行/拒绝及流量选择能力，`request_rewrite` 显式声明模型、上游提示和 canonical request 改写能力；插件同时需要两者时应都声明。两者复用 WIT 0.2 的同一个 `post-auth` 调用，以便一次有界执行原子地产生决策和改写，但 manifest 会分别暴露能力供安装审计。每次调用使用独立 Store，限制 32 MiB 线性内存和 500 万 fuel。HTTP host call 仅允许 `plugin.json` 明确列出的精确 origin，禁止重定向，请求和响应各限制 16 MiB；方法只允许 `GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE`。插件可向获准 origin 发送 `Authorization` 或供应商 API key，但不能设置 `Host`、`Content-Length`、hop-by-hop、代理转发或方法覆盖 header；最多 64 个 header、解码后共 16 KiB，单个名称/值分别最多 256 字节/8 KiB。原始 URL 的 host 始终用于 HTTP Host、TLS SNI 和证书校验，单次 DNS 解析仍由核心固定。返回值是 `{status, headers, body_base64}` JSON。KV host call 必须显式声明 `{"kind":"kv"}`，数据持久化在 PostgreSQL（测试可用 SQLite）的插件 ID 命名空间中；key 只接受最多 256 字节的安全 ASCII 路径，每个 value 上限 1 MiB，每个插件总量上限 16 MiB。前端扩展保持声明式，只允许 JSON Schema、说明和动作表单，不注入任意 JavaScript。

插件可在 `contributions.configuration` 声明对象根 JSON Schema 和非敏感默认值。控制面会用同一 Schema 在服务端校验并由 RJSF 渲染表单；`GET|PUT /internal/v1/plugins/{plugin_id}/configuration` 管理全局值和租户覆盖值。租户值优先于全局值，全局值优先于 manifest 默认值。写入要求 `plugins:write`、`Idempotency-Key` 和当前 `expected_version`，同一请求可安全重放，版本冲突返回 409。流量热路径把解析后的插件专属配置放入 WIT `request-context.config-json`，并以最多 5 秒的有界缓存避免逐请求读取数据库；本实例写入会立即失效相关缓存。配置 Schema 递归禁止 `writeOnly: true`，不得存放 API key、OAuth token 或其他凭据；这类值只能进入 provider `credential_schema` 对应的核心加密凭据存储。

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

可直接运行的 provider + OAuth + traffic policy + request rewrite 示例位于 `examples/plugins/policy-rewrite`。其中提交了可加载的 Component Model 二进制，也保留等价 WAT 供复核和可重现构建。
