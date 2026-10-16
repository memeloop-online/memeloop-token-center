# 管理界面扩展

安装经过签名的插件后，可以新增页面、卡片、页面扩展和模型路由操作，无需重新构建 Token Center。插件提供独立的 ESM 模块，导出 `activateOperatorUi(host)`，使用宿主提供的 React 和 Fluent，并返回 `operator-ui-package-v1` 组件注册表。清单中的贡献声明组件、模块路径与 SHA-256 摘要，由安装快照提供校验后的资源。

`operator.sidebar.tab` 支持自定义分组（`id` 与 `label`）和插件路由。`operator.model-route.action` 在每条模型路由的操作区渲染 `component_v1` 组件，通过 `props.context.modelRoute` 提供当前路由。操作组件可以调用 `props.api.navigate(destination, { modelRoute })`，将所选资源传给独立的插件页面。跳转沿用宿主的未保存修改保护；上下文只保存在内存中，离开页面、切换租户或更换凭据后失效。

[管理界面 SDK](https://github.com/memeloop-online/memeloop-token-center/tree/master/web/operator-ui-sdk) 提供同源 JSON 请求 `api.requestApi({ path, method, body, signal })`，以及需要登录的 OpenAPI 探索接口 `api.discoverApi(signal)`。请求使用当前页面的管理凭据，继续接受后端权限范围与资源授权检查。桥接层为接口声明的租户参数补充当前租户，不授予新权限。签名模块在管理页面的浏览器上下文中运行，因此只应安装可信发布者的插件。

临时模型请求使用 OpenAI chat 的 `POST /internal/v1/inference/chat/completions`，或 Anthropic Messages 的 `POST /internal/v1/inference/messages`，正文为 `{ key_id, request }`。使用者需要选择当前管理身份有权使用的已有计费凭据（要求 `keys:write`），插件不会收到明文密钥。非流式对话请求继续应用该凭据的模型策略、路由、余额与计费规则。关闭页面不保证上游请求已经取消，也不保证不再产生费用。界面对话记录可以只保留在内存中，正常请求记录仍按部署的保留策略处理。

提供给模型的工具应限定在任务需要的操作内，避免将返回密钥的接口内容放入模型上下文，说明写入操作的影响，并响应取消。示例和文本模型游乐场维护在独立的[示例插件仓库](https://github.com/memeloop-online/memeloop-token-center-example-plugins)。
