# v0.8.1 Provider 兼容范围

本页描述 CSSwitch v0.8.1 对 OpenCode Go、Grok 与 Gemini 的产品合同。它是源码支持范围，不等同于某个公开 DMG 或真实账号已经验收；artifact 与 live provider 结果必须另行记录。

## 连接合同

| Provider | 模板 | Base URL | Transport | 认证 |
|---|---|---|---|---|
| OpenCode Go | OpenCode Go — OpenAI Chat | `https://opencode.ai/zen/go/v1` | OpenAI Chat Completions | Bearer API key |
| OpenCode Go | OpenCode Go — Anthropic Messages | `https://opencode.ai/zen/go/v1` | Anthropic Messages | Bearer API key |
| Grok | Grok（xAI） | `https://api.x.ai/v1` | OpenAI Chat Completions | Bearer API key |
| Gemini | Gemini（OpenAI 兼容） | `https://generativelanguage.googleapis.com/v1beta/openai` | OpenAI compatibility | Bearer API key |

官方依据：[OpenCode Go](https://opencode.ai/docs/zh-cn/go/)、[xAI Inference API](https://docs.x.ai/developers/rest-api-reference/inference)、[Gemini OpenAI compatibility](https://ai.google.dev/gemini-api/docs/openai)。Gemini native API 不在 v0.8.1 范围内。

OpenCode Go 的模型协议不是根据名称猜测，而是读取版本化的 [`catalog/opencode-go-model-routes.v1.json`](../../catalog/opencode-go-model-routes.v1.json)。该快照记录来源与更新时间，并与 `/v1/models` 的实时结果求交。已知模型必须使用对应模板；未知模型不会从探测结果自动启用，只有用户明确选择 transport 后才能手工加入。发送上游的始终是裸模型 ID，不含 `opencode-go/` 前缀。

## v0.8.1 支持矩阵

| 能力 | OpenCode Go | Grok | Gemini OpenAI compatibility |
|---|---|---|---|
| 文本与多轮 | 支持 | 支持 | 支持 |
| tools / `tool_choice` | 支持，按所选 transport 映射 | 支持 | 支持 |
| 模型发现 | 官方路由表与 live 结果求交；可显式手填 | `/v1/models`；可手填 | `/models`；可手填 |
| 标题 / classifier | 已纳入本地 mock 与 loopback 门禁 | 已纳入本地 mock 与 loopback 门禁 | 已纳入本地 mock 与 loopback 门禁 |
| 图片 | limited | limited | limited |
| 厂商专有 reasoning | limited；K3 仅覆盖受签名保护的多轮 reasoning/tool 恢复 | limited | limited |
| 原生流式 | limited | limited | limited |
| 结构化输出 | limited | limited | limited |

“支持”表示请求转换、路由、错误分类和隔离 mock/loopback 合同已建立；不表示每个上游账号、模型或区域都已 live 验收。探测失败不会修改正式配置，用户手填的模型也不会被 CSSwitch 静默替换。

## 错误与恢复边界

- 推理请求不自动重发；网络或协议失败由下一次用户请求显式恢复。
- 上游 400、401、403、404、429 与 5xx 保留可区分的 HTTP 分类，错误正文有界且脱敏。
- SSE 只有在上游 2xx 且确认 `text/event-stream` 后才开始；开流后的失败只发一个 terminal error，不发送 `message_stop`。
- K3 的 reasoning 与 tool call 绑定为 CSSwitch 自有 opaque signature；历史被篡改、签名属于其他 profile、tool 参数非法或响应结构畸形时，本地 fail closed。
- Kimi relay 只删除完整 server-tool block 和“内容为空且没有有效 signature”的 thinking block；其余 thinking、usage、stop reason、原始 index 生命周期和 terminal 必须验证。
- DeepSeek native Anthropic 与 DSML detect/rewrite/off 不使用 Kimi 过滤规则。
