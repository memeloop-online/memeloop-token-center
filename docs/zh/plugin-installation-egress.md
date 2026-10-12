# 仅用于插件安装的出口代理

可信安装策略 JSON 可选设置 `egress_proxy`，值为管理员已批准的私网
IP 字面量 SOCKS5H URL，必须显式指定非零端口。例如
`"egress_proxy": "socks5h://100.64.0.2:1080"` 仅说明字段格式，并不授权或
应用任何集群配置变更。字段不来自 HTTP 安装请求、插件 manifest、账号凭据、
OAuth token 或继承环境；拒绝 userinfo、公网/回环/链路本地/元数据地址、
DNS 主机名、其他协议、路径、查询与 fragment，复用现有私网地址检查。

宿主只向既有内置安装器传 `--egress-proxy`。OCI manifest/config/layer 获取与
Cosign 共同使用该显式代理及远程 DNS；OCI client 构造失败拒绝继续，不回退直连。
生产 registry 仍用正常 HTTPS，source allowlist、digest 与全部 package 校验不变。
Cosign 仍清空环境，只新增受控 `HTTP_PROXY`、`HTTPS_PROXY`、空 `NO_PROXY`，
不设置服务 Pod 全局代理，不继承任意变量，不关闭 TLS、签名身份、issuer、版本、
证书、claims、SCT 或 Rekor 校验。registry 凭据仍走既有独立文件引用。

字段省略/null 保持旧配置与 trust fingerprint；设置后代理被纳入 review/checkpoint
指纹，变更代理需重新 review。失败仅保留固定 workspace/version/verify 阶段、数值
exit/errno 和白名单 stderr 模式类，宿主再次验证并投影；永不输出原始 stderr、
stdout、URL、路径、凭据或用户 payload，API 原有安全错误 category 不变。
模式类只是诊断线索，不证明网络流量或安装成功。

测试使用既有 GHA native 体系，涵盖策略/CLI、拒绝边界、指纹、环境隔离、SOCKS
远程 DNS 及恶意诊断字段；无需新依赖或 workflow。源码不等于已部署、已安装、
已 activate 或可登录，本变更不触发真实 OAuth/推理。
