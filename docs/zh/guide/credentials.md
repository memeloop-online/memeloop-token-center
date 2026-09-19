# 客户端凭证

客户端凭证代表一个应用或自动化任务。它决定客户端可以调用哪些模型，并限定客户端通过 `/self/v1/*` 查看自己的请求、统计与会话。

## 使用方式

将凭证放在 HTTPS 请求的 `Authorization` 请求头中：

```http
Authorization: Bearer mtc_example_client_key
```

客户端凭证只用于客户端接口，不能替代其他管理身份。可访问的模型由部署中的路由和授权关系决定。

## 保存与轮换

- 将凭证保存在服务端密钥存储或本地开发环境的受控配置中。
- 不要把凭证写入浏览器代码、日志、截图、提交记录或错误消息。
- 凭证发生泄露时，立即在部署提供的凭证管理界面完成轮换，并更新所有使用方。
- 为不同应用使用不同凭证，便于撤销、审计和分配访问范围。

## 自助视图

客户端可以使用同一凭证访问自己的数据：

```bash
curl "https://mtc.example.com/self/v1/stats" \
  -H "Authorization: Bearer mtc_example_client_key"
```

常用自助路径包括：

- `/self/v1/requests`：当前凭证的请求记录
- `/self/v1/stats`：用量与费用统计
- `/self/v1/sessions`：会话视图
- `/self/v1/conversations`：可回放的对话记录

响应只包含当前凭证可见的数据。部署管理员负责签发、授权和撤销凭证；客户端只需要使用已获授权的模型与路径。
