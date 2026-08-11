# Plugin packages

Token Center 插件是固定 digest 的 OCI artifact，至少包含：

- `plugin.wasm`：实现 `wit/token-center.wit` 的 Component Model 组件。
- `spec.yaml`：插件 id、版本、WIT API 版本、扩展点、requested capabilities 和 JSON Schema 路径。
- `README.md`：使用说明。
- 可选 `icon.png` 与配置 JSON Schema。

核心 key 鉴权、账本和 tenant 边界不可被插件替换。插件只能请求显式 host capability；HTTP 必须通过 allowlist，KV 自动按 tenant 和插件命名空间隔离。v1 前端扩展保持声明式，只允许 JSON Schema、说明和动作表单，不注入任意 JavaScript。

