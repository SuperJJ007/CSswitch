# CSSwitch

让 Claude Science 的模型推理走第三方 API（阿里通义千问 / DeepSeek 等），保留 Science 那套「AI Jupyter」体验，模型换成便宜或开源的。类比 CC Switch 之于 Claude Code。

## 一、铁律（最高优先级，任何会话都不得违反）

1. **绝不影响用户真实的订阅与登录状态。** 真实 Claude Science 的数据目录是 `~/.claude-science`，登录凭证在 `~/.claude-science/.oauth-tokens`、`active-org.json`、`encryption.key`、`orgs/`、`.key-backups/`。这些文件**只读都要谨慎，绝不复制、绝不修改、绝不删除**。
2. **绝不把真实 OAuth token 复制进任何沙箱。** 复制后两个实例共享同一 token，刷新时 Anthropic 可能轮换刷新令牌，导致用户真实实例被登出。要给沙箱登录，只能在沙箱里**全新独立登录**（另一套会话 token，对真实登录零影响），且由用户手动完成，Claude 不代做登录。
3. **绝不用改过的环境变量去启动用户的真实实例。** 真实实例跑在端口 8765。所有实验用的沙箱必须用**独立 data-dir + 独立端口 + 独立 HOME**，与 8765 完全隔开。
4. **测试默认不碰 Science。** 能用「代理↔上游」单独验证的，就不启动 Science（见 `test/`）。只有到最终整链联调、且用户明确同意时，才启动沙箱 Science，并且仍然遵守第 2、3 条。
5. 动任何有状态的东西前，先确认它不在铁律清单里；拿不准就停下来问用户。

## 二、架构

```
Claude Science（保留登录，仅当启动门票；推理不走 Anthropic）
   │  ANTHROPIC_BASE_URL=http://127.0.0.1:<port>
   ▼
翻译代理（本项目 proxy/qwen_proxy.py，或 CC Switch 内建代理）
   │  剥离入站 OAuth Bearer，注入第三方 key，Anthropic Messages ↔ OpenAI 格式互转
   ▼
阿里 DashScope（通义千问）/ DeepSeek / 其它 OpenAI 兼容端点
```

关键点：Claude 登录只是**启动 Science 的门票**，推理被 `ANTHROPIC_BASE_URL` 导去本地代理后，Anthropic 服务端不经手推理。代理负责把 Science 带来的 OAuth Bearer 丢掉、换成第三方 key，并做格式翻译。

## 三、已验证的事实（有证据，别重复推导）

来自对二进制 `/Applications/Claude Science.app/Contents/Resources/bin/claude-science`（内部代号 operon）的静态分析 + 实测：

1. **base_url 无条件生效**：`LJ()` 直接读 `process.env.ANTHROPIC_BASE_URL`，登录后推理请求会打到它。Codex 实测：登录态下 Science 向本地代理发出了 `GET /v1/models`、`POST /v1/messages`。
2. **手动 API key 被写死拒绝**：凭证解析器 `HLO.resolve()` 只认 OAuth（`_tryOauthToken`），`_tryManualApiKey()` 恒返回 `null`；还有守卫把「等于环境变量的凭证」置空。所以**完全不登录 + 只填 key 的路子走不通**，必须有 OAuth 门票。这也是早期「隔离 HOME 后 mock 收到 0 请求」是**假阴性**的原因：隔离把登录也隔离了，Science 在发 HTTP 前就因无 OAuth 而终止。
3. **CC Switch 的代理本身就是完整翻译器**：其二进制含 `/v1/messages`、`/v1/chat/completions`、`cc_switch_transform_error`、两套协议字段与 SSE 桥接，内建模型目录含 DeepSeek/Qwen/Kimi 等。翻译引擎不用自己造。CC Switch 代理端口默认 `127.0.0.1:15721`，`proxy_config` 的 `app_type` 只允许 `claude/codex/gemini`（Science 复用 `claude` 那条即可，无需新增类型）。
4. **翻译代理 ↔ 真实通义千问，整条链路已跑通**（本项目 `proxy/qwen_proxy.py`，隔离环境实测，未碰 Science/OAuth/CC Switch）：
   - `/v1/models`、非流式、**流式 SSE**、**tool_use 发起**、**tool_result 回喂后模型接着作答** 全部通过；
   - 入站 OAuth Bearer 逐条确认被剥离，未转发上游。
   - 证据：`findings/e2e-proxy-qwen-proof.log`。
5. **虚拟 OAuth（本地自造令牌，零 Anthropic、零真实凭证）已跑通整链**（2026-07-02，证据 `findings/e2e-virtual-oauth-fullchain-proof.log`）：
   - `_tryManualApiKey` 恒 null，但登录门票**不必真登录**：直接在沙箱 auth_dir 伪造一份加密令牌即可让 Science 认为已登录。
   - 令牌文件 `.oauth-tokens/<account_uuid>.enc`，格式 `"v2:"+base64(IV(12)‖AES-256-GCM‖tag(16))`；派生 `hkdfSync("sha256", base64(OAUTH_ENCRYPTION_KEY), Buffer.alloc(0), "operon:aes-256-gcm:oauth", 32)`，AAD=`v2:oauth`，明文是 token blob JSON。目录里须**恰好一个** `.enc`。
   - `encryption.key` 是换行分隔 `KEY=VALUE`：`OAUTH_ENCRYPTION_KEY`/`ANTHROPIC_API_KEY_ENCRYPTION_KEY`/`USER_SECRET_ENCRYPTION_KEY`(base64≥16B) + `JWT_SIGNING_SECRET`(≥16 字符)。keychain 镜像账号按**路径 SHA256** 派生（`encryption.key-<hash12>`），沙箱与真实天然隔离；本机实测 keychain 写入超时被跳过，纯用文件。
   - 关键坑：`token_expires_at` 必须设远期 ISO 串（如 `2099-01-01T...Z`），否则 `qP()` 判过期 → `_refreshToken` 联网打 `platform.claude.com` → 失败即无凭证。`provider="claude_ai"`，scopes=`user:inference user:file_upload user:profile user:mcp_servers user:plugins`。
   - `subscription_type` 由令牌自填、启动/鉴权阶段**不做服务端付费校验**（profile/account 走硬编码 api.anthropic.com，失败无害）。即**无需任何 Anthropic 账号**，"免费账号门票"问题作废。
   - 工具：`scripts/make-virtual-oauth.mjs`(Node，与二进制字节级一致) + `scripts/launch-virtual-sandbox.sh`。
   - Science 自身 `GET /api/auth/status` 返回 `authenticated:true, email:virtual@localhost.invalid`；真实 agent 会话中 `claude-opus-4-8→qwen-max`(推理) 与 `claude-haiku-4-5-20251001→qwen-turbo`(标题) 都经代理译到千问并在 transcript 渲染。
   - 沙箱守护 API 驱动：身份取自磁盘令牌（`AE()="none"` 用 `O9()`），写操作需 `Origin: http://localhost:<port>` + 双提交 CSRF（cookie `operon_csrf` 回显到头 `x-operon-csrf`）；建会话 `POST /api/frames {project_id}`，发消息 `POST /api/frames/:id/message {input_data:{request:"..."}, model}`（**用户文本键是 `request`，不是 text**）。
   - **沙箱钥匙串弹窗（已修，2026-07-02）**：Science 会把 `encryption.key` 镜像进 macOS 钥匙串。沙箱用独立 HOME(`.sandbox/home`)，其下无任何钥匙串，`HOME=$SANDBOX_HOME` 的进程 securityd 报「找不到默认钥匙串」，于是反复弹「找不到钥匙串 → 还原为默认」窗。这是纯隔离副作用，不是报错；误点「还原为默认」会改钥匙串默认设置，正解是点「取消」，Science 会退回读磁盘上的 `encryption.key` 文件照常工作。修法：`launch-virtual-sandbox.sh` 在**沙箱 HOME 内**建一个独立、空密码、不自动锁的 `Library/Keychains/login.keychain-db`，只在 `HOME=$SANDBOX_HOME` 上下文里 `security create/list-keychains/default-keychain -d user -s`（写的是沙箱侧偏好，`securityd` 按 HOME 隔离）。核对前后**真实** `~/Library/Keychains` 的 default 与 list 逐字节不变。修后启动日志出现 `encryption keys copied to the macOS Keychain`，弹窗消失。

## 四、尚未验证 / 待办（别当已全通）

- [x] **整链联调**：真实 Science(沙箱·虚拟登录) → 本项目代理 → 千问，同一次运行合上。见上第三节第 5 点。
- [x] **门票能否用免费账号**：作废——根本不需要任何 Anthropic 账号，伪造令牌即可，tier 自填不校验。
- [x] **多轮工具循环已验证**：`tool_use(python) → 人工批准门 → 内核执行 → tool_result 回喂 → 继续作答`，答案正确（`print((999*888)+77)` → 887189）。
  - 坑1：模型写**裸表达式**（`result` 而非 `print(result)`）时，Science 的 tool_result 只含 stdout（空），模型会瞎猜 → 让模型用 `print()`。
  - 坑2：代码执行是**人工批准门**（`output_data.pending_input_requests`，UI 里点「运行」）。无头驱动：`POST /api/frames/:id/resolve-input`，body `{responses:[{requestId,tool_id,approved:true,action:"allow",scope:"conversation|project|always"}]}`；`--dangerously-skip-approvals` 在此 ant build 被禁用。
  - 健壮性：DashScope 偶发连接抖动（SSL EOF `_ssl.c:1129`/握手超时/对端断开），代理已加**连接级重试**（`dashscope_call`，4 次退避，仅重试连接错误、不重试 4xx）。实测自动恢复。
- [x] **并行工具调用已验证**：一轮 assistant 里 `bash+read_file+edit_file+bash` 4 个 tool_use、4 个 tool_result 经代理完整往返；Auditor/verifier 子 agent 循环也走代理。
- **全量工具清单**：主 agent 26 个（`web_search bash python r repl save_artifacts read_file edit_file manage_environments manage_packages fetch_article_fulltext list_compute compute_details ask_about_compute skill ask_user search_skills boundary summary_query request_network_access list_host_grants request_host_access delete_host_files update_step_status wait_for_notification generate_plan`）+ 标题 agent 1 个。抓取法：代理设 `PROXY_DUMP_TOOLS=<dir>` 落盘请求里的 tools 数组。**代理对工具名不特判，tool_use/tool_result 通用互转**——实测 python/r/bash/read_file/edit_file/search_skills 均触发并往返。
- **实测发现的两个非代理问题**：(a) Science 里 `bash` 与 `read_file/edit_file` **文件视图不互通**（bash 写 /tmp 的文件 read_file 读不到，报 File not found）；(b) **qwen-max 在 OPERON 复杂协议里跟随力不稳**：单工具算术题正确，多步/并行/带 verifier 注入的流程会冒出协议元话术、跑偏——这是模型质量问题，非代理/虚拟登录问题。换更强模型（qwen3-max / deepseek）可能改善。
- [~] **agent 特性其余覆盖**：已验 tool_use/tool_result/并行调用/verifier 子 agent；已修 max_tokens 夹取、SSE 回放、上游重试。仍待验：思考块、cache_control、多模态图像块。
- [x] **DeepSeek 接入完成（默认上游，2026-07-02）**：新代理 `proxy/csswitch_proxy.py`，provider 可切（`--provider deepseek|qwen`）。
  - DeepSeek 走**原生 Anthropic 端点** `https://api.deepseek.com/anthropic/v1/messages`，鉴权头 `x-api-key`，代理只「改模型名 + 换鉴权 + 归一化 thinking + 夹 max_tokens + 重试」，**不翻译协议** → thinking/tool_use 原生保真。
  - 模型：`claude-opus-4-8→deepseek-v4-pro`、`claude-haiku/sonnet→deepseek-v4-flash`。选择器主列表机制见下条：`/v1/models` 现广告 `claude-opus-4-8`(显示名「DeepSeek V4 Pro」) + `claude-haiku-4-5`(显示名「DeepSeek V4 Flash」)，两者直接平铺在选择器主列表，`model_map` 再把这些 id 映射回真实 deepseek id。默认 id 恰是 `claude-opus-4-8`，主按钮即显示「DeepSeek V4 Pro」。
  - **模型选择器主列表机制（逆向 `s0`/`ZjO`/`XjO`/`hB_`）**：面板对可选项有两道硬规则。① `s0`：id 必须以 `claude-` 开头，否则整条不显示。② `hB_` 分两层：只有 `ZjO(id)<3`（`claude-opus*`=0，`claude-sonnet*`=1，`claude-haiku*`=2，其它=3）且 `XjO(id)` 命中 `^claude-(opus|sonnet|haiku)-<纯数字版本>$`（家族名后只能跟数字，不能带 `deepseek`/`flash` 等词）的 id 才进【主列表】(`overflow:false`)，每 family 只留一个；其余一律折叠进「More models」(`overflow:true`)。`hB_` 用 `{...x}` 保留 `/v1/models` 给的 `name`，只覆盖描述行。所以要让第三方模型直接平铺，就把它挂在 `claude-opus-4-8`/`claude-haiku-4-5` 这类主列表 id 上、显示名照写第三方。早期用 `claude-deepseek-*` 会被判 tier3 沉进折叠区，且主按钮显示「Default」（因默认 id `claude-opus-4-8` 不在广告列表里，没匹配上）。
  - **两处透传坑（已修）**：① Science 发 `thinking.type:"auto"`，DeepSeek 只认 `adaptive/enabled/disabled` → 归一化为 `adaptive`；② 强制 `tool_choice`（`type:tool/any`，如标题/verdict 生成）时 DeepSeek 不允许 thinking，且 flash **默认 thinking 开**（请求里 thinking=null 也冲突）→ 强制工具时无条件置 `thinking:{type:disabled}`。
  - 健壮性：连接 + 完整读体都重试（`http_post`/`open_stream`，覆盖 IncompleteRead、SSL EOF、503 too-busy）。
  - 实测：主推理(v4-pro 流式+thinking)、标题(v4-flash 强制工具)、**工具循环**(python→exec→tool_result→答案 56877 正确) 全通；且 **v4-pro 跟随 OPERON 协议明显比 qwen-max 稳**。
- [ ] Qwen(DashScope) 仍为 OpenAI-兼容备选（`--provider qwen`，翻译路径）；`proxy/qwen_proxy.py` 为其早期单 provider 版，已被 `csswitch_proxy.py` 取代。
- [ ] 决定最终用哪条代理：自研 `qwen_proxy.py`（透明可控）还是复用 CC Switch 内建代理（覆盖面广）。

## 五、目录与用法

```
proxy/csswitch_proxy.py         【主】provider 可切代理：deepseek(原生 Anthropic 透传, 默认) / qwen(OpenAI 翻译)
                                  起法: DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991
proxy/qwen_proxy.py             早期单 provider(千问)翻译代理，已被 csswitch_proxy.py 取代
scripts/make-virtual-oauth.mjs  虚拟 OAuth 伪造器（Node，字节级一致；只写沙箱，护栏拒真实目录）
scripts/launch-virtual-sandbox.sh 起沙箱 Science + 写虚拟登录 + 指向代理（推荐入口）
scripts/launch-science-sandbox.sh 旧版：起沙箱但需手动真登录（保留，一般用不到）
scripts/stop-science-sandbox.sh   停沙箱（按 data-dir，绝不影响真实 8765）
scripts/daily-maintenance.sh      每日巡检 wrapper：无头跑受限 claude，检查 Science 官方更新 + 扫仓库 + 出规划报告
scripts/daily-maintenance.prompt.md 上者的巡检提示词（内嵌铁律，只读+抓网页+写报告）
scripts/com.csswitch.maintenance.plist launchd 定时（每天 09:00/21:00 Asia/Shanghai）
scripts/install-maintenance.sh    安装/卸载/查看定时：install|uninstall|status|run
test/                           隔离回归测试（只打代理，不碰 Science）
findings/                       证据与二进制分析记录
findings/auto-maint/            每日巡检输出：report-*.md 规划报告 + science-version.last 版本基线 + logs/（整目录 git 忽略，只留 .gitkeep 占位）
.sandbox/                       沙箱 Science 的独立 HOME/data-dir（git 忽略）
```

### 每日维护巡检定时任务

- **是什么**：本机 launchd 每天 09:00 / 21:00（Asia/Shanghai）无人值守地跑一次 `claude -p` 巡检，
  ①查 Claude Science 官方是否有更新（本机已装版本 vs `science-version.last` + 抓公开产品页）；
  ②扫仓库（`git status`/待办清单/TODO）；③**先规划**，把「今天该做什么」写进 `findings/auto-maint/report-*.md`。
- **安全边界（默认「规划+汇报」模式，守铁律）**：该 session **只读仓库 + 抓公开网页 + 只往 `findings/auto-maint/` 写报告**；
  用 `--allowedTools` 白名单（不放行任意 Bash）+ `--disallowedTools` 硬禁 `git commit/push/add/checkout`、`rm`、
  以及对 `~/.claude-science` 的读写；不改代码、不提交、不推送、不动分支、不启动任何 Science。
- **装/卸/看**：`scripts/install-maintenance.sh {install|uninstall|status|run}`。手动测：`bash scripts/daily-maintenance.sh`。
- **升级到自动改动**：若要让它进一步在**隔离分支**上改代码+测+提交（绝不动 main、绝不推送），
  改 `daily-maintenance.prompt.md` 的授权段并放开 wrapper 里对应的 `disallowedTools`；这是有状态操作，改前须用户确认。
- **局限**：launchd 环境无 `DASHSCOPE_API_KEY`/`DEEPSEEK_API_KEY`，跑不了需要上游 key 的 `test/` 整链测试，
  也检测不到「官方已发布但本机没装」的更新（app 无 appcast，只能对比已装版本 + 公开页）。

- 起代理（默认 DeepSeek）：`DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991`
  切千问：`DASHSCOPE_API_KEY=... python3 proxy/csswitch_proxy.py --provider qwen --port 18991`（也支持 `--env-file <某个.env>`）
- 跑隔离回归测试：见 `test/`（会自动起代理、打完停掉）。
- **整链（虚拟登录，推荐）**：先起代理，再 `scripts/launch-virtual-sandbox.sh --port 8990 --proxy-url http://127.0.0.1:18991`。
  取 UI 链接：`HOME=.sandbox/home <bin> url --data-dir .sandbox/home/.claude-science`（浏览器打开即已登录）。
  停：`scripts/stop-science-sandbox.sh`。
- 起沙箱 Science（旧版真登录）：`scripts/launch-science-sandbox.sh`（一般不用，除非要测真登录路径）。

## 六、环境备忘

- 真实 Science 数据目录 `~/.claude-science`，端口 8765，绝对不碰。
- 阿里 key 以 `DASHSCOPE_API_KEY` 存在于用户 shell 环境（值不显示、不入库）。DashScope 兼容端点：`https://dashscope.aliyuncs.com/compatible-mode/v1`。
- 模型映射见 `csswitch_proxy.py` 的 `PROVIDERS[<provider>]["model_map"]`（deepseek: claude-opus-4-8→deepseek-v4-pro、claude-haiku/sonnet→deepseek-v4-flash；qwen: claude-* → qwen-max/plus/turbo）。选择器广告的 id 见同处 `"models"`。
- Python 用 conda 环境（见用户全局记忆），避免系统 3.9。
