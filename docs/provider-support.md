# Provider / API 支持调研与计划（Historical Research）

> **Historical / reference-only status（2026-07-09）：** 本文是早期 provider/API 调研留档，不是当前产品支持矩阵、release gate 或 live provider 验证清单。当前实现事实以 [`desktop/src-tauri/src/templates.rs`](../desktop/src-tauri/src/templates.rs)、[`catalog/capabilities.v1.json`](../catalog/capabilities.v1.json)、[`docs/provider-capability-matrix.md`](provider-capability-matrix.md) 和运行时 `status().catalog` 为准。本文保留 CC Switch / CS native 参考和候选 provider 线索，但不得被解读为 CSSwitch 已完整支持、验证或承诺这些 provider。

目标：记录第三方 API 支持方向。本文回答两个核心问题（到底走 Anthropic 端点还是 OpenAI 端点、Claude Science 的工具调用格式怎么处理），并给出候选 provider 清单与实现优先级。对应 roadmap 见 [`known-issues.md`](known-issues.md) 第 2 条。

> 状态：**调研封存**（用户 2026-07-03 叫停）。#1 文案脱敏已完成；**本研究的落地切片「面板内自定义 OpenAI 端点」已定为 #1 之后的下个主线**（见 [`known-issues.md`](known-issues.md) 文末）。已定稿：核心分型、Science 工具调用格式与翻译坑、CC Switch 参考（第三节）、国际 provider 数据（已调研）。**待续**：把国产各家 OpenAI 端点 / 模型细节并成第四节大表（深挖 agent 中断）。
>
> 2026-07-08 维护注记：主线现已有静态 [`catalog/capabilities.v1.json`](../catalog/capabilities.v1.json) 和 runtime `status().catalog` diagnostics，用来暴露当前 profile 命中的 provider/tool 规则以及固定边界规则。它们是诊断与可观测性入口，不让 catalog 驱动代理行为，也不等同于 live provider、真实账号态、Science GUI E2E、DMG 签名/公证或官方托管 MCP/Directory/remote skill 能力已验证。

## 一、核心结论：到底是 Anthropic 端口还是 OpenAI 端点？

**两边都有，取决于 provider；决定权不在 Science，在上游。**

- **Science 侧永远是 Anthropic**：Claude Science 以为自己在跟 Anthropic 说话，`ANTHROPIC_BASE_URL` 只改「打到哪」，不改「说什么」。所以**代理【入站】永远收到 Anthropic Messages API 格式**（`POST /v1/messages`，含 `system` / `messages` / `tools`，内容块 `text` / `tool_use` / `tool_result` / `image`，SSE 流式）。
- **代理【出站】格式由 provider 决定**，分两条路：

  1. **Anthropic 原生透传（首选）**：provider 提供 Anthropic 兼容端点（如 DeepSeek `https://api.deepseek.com/anthropic`）。代理只需**剥掉入站 OAuth、注入你的第三方 key、原样转发**。零翻译、保真最好（`tool_use`、流式、thinking 都不失真）。CSSwitch 当前 DeepSeek 走这条。
  2. **OpenAI 兼容翻译**：provider 只有 OpenAI 兼容端点（如千问 DashScope `https://dashscope.aliyuncs.com/compatible-mode/v1`）。代理必须**出站 Anthropic→OpenAI、回程 OpenAI→Anthropic 双向翻译**，含工具调用与流式的格式转换。CSSwitch 当前 Qwen 走这条。

- **实现取向**：**优先接「有 Anthropic 原生端点」的 provider**（透传，一条 `PROVIDERS` 配置即可，几乎零成本、零保真损失）；只有 OpenAI-only 的才写/复用翻译层。代理里 `PROVIDERS` 已经用这个二分法编码（deepseek=透传 / qwen=翻译）。

## 二、Claude Science 的工具调用格式，以及翻译保真坑

Science **重度依赖工具调用**（实测启动即注册多个 agent + 一批 MCP 工具，`image_processing_available:true`）。所以「翻译类 provider 的工具调用是否保真」是选型的核心，不是可选项。

**两种格式的关键差异（翻译层必须处理）：**

| 维度 | Anthropic（入站，Science 侧） | OpenAI（出站，翻译目标） |
|---|---|---|
| 工具定义 | `tools[].input_schema`（JSON Schema） | `tools[].function.parameters`（JSON Schema） |
| 模型发起调用 | 内容块 `tool_use{ id, name, input(对象) }` | `tool_calls[]{ id, function.name, function.arguments(JSON 字符串) }` |
| 工具结果回传 | 内容块 `tool_result{ tool_use_id, content(字符串或块数组) }` | 独立消息 `role:"tool"{ tool_call_id, content(字符串) }` |
| system | 顶层 `system` 字段 | 一条 `role:"system"` 消息 |
| 图片 | 内容块 `image{ source: base64 }` | `content[].image_url` |
| 流式工具参数 | `content_block_delta` 的 `input_json_delta`（增量 JSON 片段） | `delta.tool_calls[].function.arguments`（字符串片段）+ index |
| 停止原因 | `stop_reason`: `tool_use` / `end_turn` / `max_tokens` | `finish_reason`: `tool_calls` / `stop` / `length` |
| 推理/思考 | `thinking` 块（或无） | 各家不一：DeepSeek `reasoning_content`、OpenAI o 系列不回明文 |

**已知保真坑（翻译层要一一处理，否则工具调用会碎）：**

1. **input 对象 ↔ arguments 字符串**：Anthropic 的 `tool_use.input` 是对象，OpenAI 的 `arguments` 是 JSON 字符串，双向要 `json.dumps` / `json.loads`；模型偶尔吐非法 JSON 片段，需容错。
2. **tool_result 的富内容**：Anthropic 的 `tool_result.content` 可以是块数组（含图片），OpenAI 的 `tool` 消息只吃字符串 → 富内容要降级/摊平。
3. **流式增量对齐**：Anthropic 的 `input_json_delta` 与 OpenAI 的 `arguments` 片段要按 tool 的 index/id 正确拼接与重排；多工具并行（parallel tool calls）时 index 映射易错。
4. **停止原因映射**：`finish_reason:"tool_calls"` 必须映回 `stop_reason:"tool_use"`，否则 Science 不知道该去执行工具、对话卡住。
5. **thinking / reasoning**：DeepSeek 原生端点的思考不失真；翻译类要决定把 `reasoning_content` 映成 Anthropic `thinking` 还是丢弃（影响 Science 的展示与 agent 逻辑）。
6. **role 交替与 max_tokens**：Anthropic 强约束 user/assistant 交替且要求 `max_tokens`；从 OpenAI 侧回填时要补齐。
7. **图片**：Science 会处理图片，`image` 块 ↔ `image_url` 的 base64/URL 形态要转。

> 结论：**Anthropic 原生端点省掉上面全部坑**。这就是为什么优先接原生端点、把翻译层留给确实只有 OpenAI 端点的 provider，并且对翻译类要专门测工具调用与流式。

## 三、CC Switch 支持哪些（参考）

- **仓库**：[github.com/farion1231/cc-switch](https://github.com/farion1231/cc-switch)（Tauri/Rust + React 的托盘 app；除 Claude Code 外还管 Codex / Gemini CLI / OpenCode 等）。很受欢迎（star 数调研值异常高，未采信具体数字）。
- **机制**：给 Claude Code 换 provider = 把选中预设的 `env`（`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`，个别用 `ANTHROPIC_API_KEY` 或 OAuth）写进 Claude Code 的 live 配置。**本质是 base_url + token 切换器**，与我们对 Science 的思路同源。
- **规模**：内置 **70 个 Claude 预设**。**65 个直连 Anthropic 端点透传**（`apiFormat=anthropic`）；**5 个非 Anthropic 格式**（Gemini Native、OpenCode Go、GitHub Copilot、Codex、Nvidia）走 cc-switch **自带的翻译代理**转成 Anthropic。新版预设 schema 加了 `apiFormat` 字段（`anthropic` / `openai_chat` / `openai_responses` / `gemini_native`）。
- **可加自定义**：用户能加任意自定义端点（v3.16「Full URL Endpoint Mode」把 base_url 当精确上游 URL）。

**关键启示（对 CSSwitch 直接有用）**：cc-switch 的预设表证明**大量国产 provider 都已提供 `/anthropic` 端点**，这些对我们就是「透传」一条路，几乎零成本。从其预设里能确认的国产 Anthropic 端点（并入下表核对）：

| Provider | Anthropic 端点（cc-switch 预设里的 base_url） |
|---|---|
| DeepSeek | `https://api.deepseek.com/anthropic` |
| 智谱 GLM | `https://open.bigmodel.cn/api/anthropic`（国际 `https://api.z.ai/api/anthropic`） |
| Kimi / Moonshot | `https://api.moonshot.cn/anthropic`（`https://api.kimi.com/coding/`） |
| MiniMax | `https://api.minimaxi.com/anthropic`（国际 `.minimax.io`） |
| 阿里百炼 Bailian | `https://dashscope.aliyuncs.com/apps/anthropic`（编码版 `coding.dashscope...`） |
| 火山方舟 Volcano Ark | `https://ark.cn-beijing.volces.com/api/coding`（豆包 `.../api/compatible`） |
| StepFun 阶跃 | `https://api.stepfun.com/step_plan` |
| 百度千帆 | `https://qianfan.baidubce.com/anthropic/coding` |
| 硅基流动 SiliconFlow | `https://api.siliconflow.cn`（国际 `.com`） |
| 小米 MiMo | `https://api.xiaomimimo.com/anthropic` |
| ModelScope / Novita / Longcat | `api-inference.modelscope.cn` / `api.novita.ai/anthropic` / `api.longcat.chat/anthropic` |
| OpenRouter（聚合） | `https://openrouter.ai/api`（cc-switch 当 anthropic 透传） |
| AWS Bedrock | `https://bedrock-runtime.${AWS_REGION}.amazonaws.com`（原生 Claude-on-Bedrock，SigV4） |

**和我们的差异**：cc-switch 换的是 **Claude Code**（配置即用、无强制 OAuth）；CSSwitch 换的是 **Claude Science**（强制 OAuth 门票 → 我们还要伪造虚拟登录 + 起隔离沙箱）。所以**「有没有 Anthropic 端点」这一列可以直接借鉴 cc-switch，但门票机制我们独有**。

## 四、CSSwitch 候选 provider 大表（封存，待续做）

> 调研被叫停，未合成完整大表。**已有素材**：① 国产各家的 **Anthropic 原生端点**见上面第三节表（已核实，来自 CC Switch 预设源码）；② 国际 provider（OpenAI/Gemini/OpenRouter/Groq/Together/Fireworks/xAI/Mistral/Perplexity/DeepInfra/Ollama/LM Studio/vLLM/LiteLLM）的端点与工具/reasoning 数据已调研（结论：仅 OpenRouter / vLLM / LiteLLM 是 Anthropic 原生可透传，其余仅 OpenAI 格式需翻译；xAI 无 `/v1/messages`）。
>
> **续做时**要补的列：provider | Anthropic 原生端点 | OpenAI 兼容端点 | 主打 agentic/工具模型 | 工具调用支持与坑 | reasoning/thinking | 实现方式（透传/翻译） | 优先级 | 备注。国产的 OpenAI 端点（如 DeepSeek `api.deepseek.com/v1`、千问 `dashscope.../compatible-mode/v1`、智谱 `open.bigmodel.cn/api/paas/v4` 等）与模型细节需二次核实后并入。

## 五、实现优先级（框架，慢慢做）

1. **第一梯队（透传，最省事最保真）**：有 Anthropic 原生端点的 provider，接一条 `PROVIDERS` 配置即可（像 DeepSeek）。优先把国产里有 Anthropic 端点的都接上。
2. **第二梯队（翻译，复用现有 Qwen 翻译层）**：只有 OpenAI 兼容端点、但工具调用规范的（阿里千问、多数聚合器）。重点回归测工具调用 + 流式。
3. **第三梯队（自定义端点）**：面板内让用户填任意 OpenAI 兼容 base_url / 模型名 / 鉴权头，不改代码即可加新 provider（覆盖长尾与本地 Ollama/LM Studio）。
4. **通用适配器思路（备选）**：对只有 OpenAI 端点的长尾，可考虑用 LiteLLM / 自建 axum 适配把它们统一成 Anthropic `/v1/messages`，从而都走透传一条路（与 python-ectomy 合并考虑）。
