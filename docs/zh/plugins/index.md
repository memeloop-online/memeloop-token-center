# 插件概述

MTC 插件是版本化的 WebAssembly 组件（Component Model），用于在明确边界内扩展路由、供应商适配、OAuth 流程和请求策略。每个插件包包含 `plugin.json` 清单、可选的 `.wasm` 组件、README 与图标。

权威契约：

- [WIT 接口定义 `token-center.wit`](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)
- [plugin.json 清单 Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)

## 能力模型

插件能做什么完全由清单中的 `capabilities` 决定：

| 能力 | 含义 |
| --- | --- |
| `log` | 产生有界的宿主日志事件 |
| `kv` | 使用按插件命名空间隔离的键值存储 |
| `http` | 仅访问清单精确列出的 HTTPS origin |
| `group_routing_quota` | 读取已授权候选的额度窗口快照 |

主机为每次调用提供有界的 fuel、内存和执行期限。插件不会看到上游凭证，也不能绕过模型权限、余额、限流或审计边界。

## 扩展点

- 流量策略：在认证后对请求做有界判断或改写。
- Provider 组件：为特定供应商提供模型目录、请求准备和响应规范化。
- OAuth 适配器：声明供应商所需的授权码或设备码流程。
- 组路由：对已经授权的候选进行可复现排序和有限健康建议，见[组路由](routing.md)。

插件包的实现、清单和 OCI 结构见[插件开发](development.md)。

## 安全边界

插件请求使用核心生成的身份和权限上下文；敏感凭证只在核心与供应商之间使用。插件失败时核心回退到原生路径，已开始的请求继续使用入场时的插件快照。

插件可以扩展产品能力，但不会改变客户端可见的授权边界。部署中的安装、批准和启用流程由管理员按其环境的发布策略完成。
