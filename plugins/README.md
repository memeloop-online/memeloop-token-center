# Plugin packages

Token Center 插件是固定 digest 的 OCI artifact。解包后的每个插件各占一个目录，至少包含：

- `plugin.wasm`：实现 `wit/token-center.wit` 的 Component Model 组件。
- `plugin.json`：插件 id、版本、WIT API 版本、扩展点、capability allowlist 和 JSON Schema provider contribution。
- `README.md`：使用说明。
- 可选 `icon.png` 与配置 JSON Schema。

设置 `MTC_PLUGIN_DIR` 后，服务会在启动时编译目录下的组件；同名 provider 或不兼容的 `wit_version` 会让启动失败。Helm 用 `plugins.existingConfigMap` 或 `plugins.existingClaim` 只读挂载该目录。

核心 key 鉴权、账本和 tenant 边界不可被插件替换。每次 traffic policy 调用使用独立 Store，限制 32 MiB 线性内存和 500 万 fuel。HTTP host call 仅允许 `plugin.json` 明确列出的精确 origin，禁止重定向，请求和响应各限制 16 MiB；返回值是 `{status, headers, body_base64}` JSON。KV host call 必须显式声明 `{"kind":"kv"}`，数据持久化在 PostgreSQL（测试可用 SQLite）的插件 ID 命名空间中；key 只接受最多 256 字节的安全 ASCII 路径，每个 value 上限 1 MiB，每个插件总量上限 16 MiB。前端扩展保持声明式，只允许 JSON Schema、说明和动作表单，不注入任意 JavaScript。

最小 manifest：

```json
{
  "id": "example-policy",
  "version": "1.0.0",
  "wit_version": "0.1.0",
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
