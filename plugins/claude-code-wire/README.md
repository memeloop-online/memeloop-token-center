# claude-code-wire

把发往 anthropic-claude（Claude 订阅 OAuth）上游的 Anthropic Messages 请求，
在网关出口处改写成与官方 Claude Code CLI 完全一致的线上格式（wire format）。
逻辑逐比特对齐 [pi-black](https://github.com/paoloanzn/pi-black) 的参考实现：

- system 数组前置插入 billing block（含 cc_version 指纹与 cch 校验和）和
  Agent SDK block，并剥离客户端自带的旧 billing/SDK/legacy 块；
- cch 校验和：对最终序列化 body（model 置空、删除 max_tokens）做 seeded
  XXH64，取低 20 bit 的 5 位 hex——宿主逐字节转发插件输出，校验和恒自洽；
- 规范请求头：user-agent、x-app、x-claude-code-session-id、
  x-client-request-id、x-stainless-* 系列（宿主在应用这些头之前会先剥离
  客户端自带的指纹头，防止非官方 SDK 的 x-stainless 值泄漏到上游）；
- 可选 metadata.user_id（device_id + account_uuid + 派生 session_id）。

插件出错时宿主 fail-closed 拒绝请求，绝不放过未改写的请求。

## 构建

    rustup target add wasm32-wasip2
    ./build.sh          # 产出 plugin.wasm
    cargo test          # 原生单元测试（XXH64 向量、指纹、system 手术、cch 自洽）

## 配置

所有配置项都有安全的默认值（对齐 Claude Code 2.1.258），可全局或按租户覆盖：

- enabled（默认 true；**关闭会拒绝请求而不是放行未改写流量**）
- claude_code_version / entrypoint
- device_id + account_uuid（可选，必须成对配置；取自该 Claude 账号
  ~/.claude.json 的 userID 与 oauthAccount.accountUuid）
- stainless_* 系列头取值（os/arch/runtime 等，同一账号应保持稳定）

## 必须配合的客户端侧措施（网关拦不到的部分）

Claude Code 客户端会**绕过 API 网关直连**遥测服务（Statsig 事件、Sentry
错误上报），这些流量可能暴露真实环境。如果客户端本身就是 Claude Code，
请务必在其环境中设置：

    export DISABLE_TELEMETRY=1
    export DISABLE_ERROR_REPORTING=1
    export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1

或在 DNS/防火墙层屏蔽 statsig 与 sentry 相关域名。非 Claude Code 客户端
（直接调 /v1/messages 的程序）没有这个问题。

## 出口 IP

请求指纹再完美，源 IP 在中国等不支持地区一样会被封号。请给
anthropic-claude 上游账号配置支持地区的 SOCKS5 出口代理（登录时的
proxy_url，仅接受 socks5h + 私有 IP 字面量）。

## 风险声明

本插件用于让你的订阅流量在上游看来与官方客户端一致。Anthropic 的风控
策略随时可能变化（新版本号、新指纹维度）；claude_code_version 需要跟随
官方 Claude Code 版本更新。使用者需自行评估服务条款风险。
