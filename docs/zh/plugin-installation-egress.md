# 仅用于插件安装的出口代理

可信安装策略 JSON 可选设置 `egress_proxy`，值为管理员已批准的私网
IP 字面量 SOCKS5H URL，必须显式指定非零端口。例如
`"egress_proxy": "socks5h://100.64.0.2:1080"` 仅说明字段格式，并不授权或
应用任何集群配置变更。字段不来自 HTTP 安装请求、插件 manifest、账号凭据、
OAuth token 或继承环境；拒绝 userinfo、公网/回环/链路本地/元数据地址、
DNS 主机名、其他协议、路径、查询与 fragment，复用现有私网地址检查。

宿主向既有内置安装器传 `--egress-proxy`。OCI manifest/config/layer 获取与
Cosign 共同使用该显式代理及远程 DNS；OCI client 构造失败拒绝继续，不回退直连。
生产 registry 仍用正常 HTTPS，source allowlist、digest 与全部 package 校验不变。
Cosign 仍清空环境，只新增受控 `HTTP_PROXY`、`HTTPS_PROXY`、空 `NO_PROXY`，
不设置服务 Pod 全局代理，不继承任意变量，不关闭 TLS、签名身份、issuer、版本、
证书、claims、SCT 或 Rekor 校验。registry 凭据仍走既有独立文件引用。

## 认证 SOCKS

策略可选增加 `egress_proxy_username_file`、`egress_proxy_password_file`：必须
同时提供两个绝对挂载文件路径和 `egress_proxy`。对应 CLI 参数使用连字符；
endpoint 仍拒绝 userinfo，HTTP 安装请求拒绝这三个字段，不接受凭据值参数、
继承凭据环境或账号代理回退。

复用既有 `plugins.runtimeInventory.registrySecrets`，选择已有 Secret 的专用
proxy keys，以 `0440` 只读挂载到 Control 的
`/var/run/mtc-plugin-registry/<index>/<key>`。历史挂载名称不代表认证混用：
registry 认证仍仅由独立 `source_credentials` 配置。账号 transport groups 和
OAuth 凭据属于不同信任域，不自动复用。无需新 chart schema、新身份、共享代理
修改或匿名/IP 豁免。若既有受保护代理配置把 authentication 打包在单个 YAML key，
不能把整份 YAML 当用户名/密码：应由配置 owner 通过既有受保护流程投影同一已授权
凭据，不打印到工单，不更改代理认证。

子安装器限量读取 regular file；去尾部 CR/LF 后每项须为 1–255 字节 UTF-8，
拒绝内嵌控制字符。仅在 RAM 内编码为 OCI client 和清空后的 Cosign 子进程代理
环境；不进 argv、API、收据、诊断或 Docker registry 配置。凭据类型 Debug 固定
脱敏，读取/解析失败只返回既有固定 egress-policy 错误。SOCKS 用户名/密码协商本身
不加密，仍须使用批准的私网出口并保护 Secret 挂载及进程环境访问权限。

文件引用纳入 review/checkpoint 指纹；与 registry 凭据相同，凭据内容只是传输授权，
不是签名信任根，不进入指纹。同路径轮转凭据不替代或弱化新包验签；省略/null
凭据字段保留匿名行为和旧指纹。

字段省略/null 保持旧配置与 trust fingerprint；设置后代理被纳入 review/checkpoint
指纹，变更代理需重新 review。失败仅保留固定 workspace/version/verify 阶段、数值
exit/errno 和白名单 stderr 模式类，宿主再次验证并投影；永不输出原始 stderr、
stdout、URL、路径、凭据或用户 payload，API 原有安全错误 category 不变。
模式类只是诊断线索，不证明网络流量或安装成功。

测试使用既有 GHA native 体系，涵盖策略/CLI、拒绝边界、指纹、环境隔离、SOCKS
远程 DNS、RFC1929 认证及特殊字符、认证失败不直连、凭据文件边界、registry
认证隔离和恶意诊断字段；无需新依赖或 workflow。源码不等于已部署、已安装、
已 activate 或可登录，本变更不触发真实 OAuth/推理。
