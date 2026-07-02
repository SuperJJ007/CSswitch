# CSSwitch

让 [Claude Science](https://claude.com)（AI Jupyter 体验）的模型推理走第三方 API（DeepSeek 原生 / 阿里通义千问 等），保留 Science 那套交互，把底层模型换成更便宜或开源的。类比 CC Switch 之于 Claude Code。

推理请求经本地翻译代理导向你自付的第三方模型，登录门用本地自造的「虚拟 OAuth」越过，全程零真实凭证。

---

## ⚠️ 免责声明

- 本项目仅供**个人学习与研究**用途。
- 推理请求经本地代理直连你自己付费的第三方模型（DeepSeek / 通义千问 等），**不经过 Anthropic 服务端**做推理；使用的是本地自造的虚拟登录令牌，**零真实 Anthropic 凭证**。
- 需要说明：Science 在**启动阶段**仍会尝试访问其硬编码的 profile / account 接口（`api.anthropic.com`），该请求失败不影响使用。因此本项目**不宣称**「完全零 Anthropic 接触」这类绝对说法。若要完全阻断，需另加网络隔离并以抓包验收，本项目不做此承诺。
- 本项目与 Anthropic **无任何从属、合作或背书关系**。
- 本项目对 Science 登录令牌加密格式的逆向、以及「越过登录」的实现，可能触及相关服务条款与版权法规（如美国 DMCA §1201 的反规避条款）。是否适用、有无豁免需专业法律判断，**使用者自负风险**。
- 软件按「现状」提供，**不提供任何形式的担保**。详见 [LICENSE](./LICENSE)（MIT）。

本项目不偷取 Anthropic 算力（推理走用户自付第三方）、不泄露用户密钥、不含恶意代码、不损害第三方。相关风险属于法律、条款与个人关系层面。

---

## 背景

Claude Science 是一套「AI Jupyter」桌面产品。它的登录只是**启动门票**：登录后，推理请求的目标地址由环境变量 `ANTHROPIC_BASE_URL` 无条件决定。CSSwitch 就利用这一点，把推理导向本地一个翻译代理，代理再剥掉 Science 带来的 OAuth Bearer、换成第三方 key、按需做协议翻译，最终打到 DeepSeek 或通义千问。

登录门无法用「只填 API key、不登录」绕过（Science 的凭证解析器写死只认 OAuth），但可以在**隔离沙箱**里写一份本地自造的虚拟 OAuth 令牌让 Science 认为已登录，全程不碰真实登录、不联网刷新。

## 架构

```
托盘图标 / 菜单栏 GUI（Tauri，进程管家）
   │
   ├─ 网页前端面板：provider 选择 / 第三方 key / 起停 / 一键越过登录 / 状态灯
   │
   ├─ 子进程①  proxy/csswitch_proxy.py   常驻，是 ANTHROPIC_BASE_URL 的目标
   │            剥离入站 OAuth、注入第三方 key（经环境变量，绝不进命令行）、按 provider 透传或翻译
   │
   ├─ 子进程②  scripts/launch-virtual-sandbox.sh
   │            内部调 make-virtual-oauth.mjs 写虚拟登录，再起隔离的 Science serve
   │
   └─ 本地配置  ~/.csswitch/config.json（0600）：provider key、端口、上次选择
```

```
Claude Science（沙箱·虚拟登录）
   │  ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>
   ▼
csswitch_proxy.py（翻译代理）
   │  剥离入站 Bearer，注入第三方 key，路径 secret 鉴权
   ▼
DeepSeek 原生 Anthropic 端点  /  DashScope（通义千问，OpenAI 兼容，双向翻译）
```

- **DeepSeek**（默认）：走原生 Anthropic 端点，代理只「改模型名 + 换鉴权 + 归一化 thinking + 夹 max_tokens + 重试」，thinking / tool_use 原生保真，不翻译协议。
- **Qwen**：走 DashScope OpenAI 兼容端点，代理做 Anthropic ↔ OpenAI 双向翻译（流式以 SSE 回放保真 tool_use）。

## 铁律（安全边界，最高优先级）

CSSwitch 的第一原则是**绝不影响你真实的 Claude Science 订阅与登录**：

1. 真实数据目录 `~/.claude-science`（含 `.oauth-tokens`、`encryption.key`、`active-org.json` 等）**绝不复制、绝不修改、绝不删除**。
2. **绝不把真实 OAuth token 复制进沙箱**（共享 token 会在刷新时可能把你真实实例登出）。沙箱只用本地自造的虚拟令牌。
3. 真实实例跑在端口 **8765**；所有沙箱一律用**独立 data-dir + 独立端口 + 独立 HOME**，与 8765 完全隔开。脚本对 8765 与真实目录路径做**失败关闭**的护栏断言。
4. 测试默认**不碰 Science**（只打「代理 ↔ 上游」），仅在最终整链联调且用户明确同意时才起沙箱。

代理侧亦守边界：入站 `Authorization` / `x-api-key` 一律剥离不转发；第三方 key 只驻内存、不进日志、不进命令行；只监听回环地址；路径 secret 鉴权挡跨源与误调用（威胁模型边界见 `docs/` 规格）。

## 快速开始

前置：本机已安装 Claude Science、`python3`、`node`。第三方 key 放进环境变量（值不入库、不显示）。

### 1. 起翻译代理

```bash
# DeepSeek（默认）
DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991

# 通义千问
DASHSCOPE_API_KEY=... python3 proxy/csswitch_proxy.py --provider qwen --port 18991
```

### 2. 起沙箱 Science（虚拟登录）

```bash
scripts/launch-virtual-sandbox.sh --port 8990 --proxy-url http://127.0.0.1:18991
```

取 UI 链接（浏览器打开即已是登录态）：

```bash
HOME=.sandbox/home "/Applications/Claude Science.app/Contents/Resources/bin/claude-science" \
  url --data-dir .sandbox/home/.claude-science
```

停止沙箱（按沙箱 data-dir，绝不影响真实 8765）：

```bash
scripts/stop-science-sandbox.sh
```

### 3. 菜单栏 GUI

一个 macOS 菜单栏 app（Tauri），把上面这些步骤收进一个面板：选 provider、填第三方 key（本地 `~/.csswitch/config.json`，0600，只回显末位掩码）、一键越过登录、起停、三盏状态灯。构建与用法见 [`desktop/README.md`](./desktop/README.md)。

> 注：GUI 只负责编排子进程与读写配置，已验证的越权与翻译逻辑仍留在 Python / Node / shell 里被调用，以保住铁律护栏与已验证行为。

## 安装后自检

```bash
scripts/doctor.sh        # 只读环境诊断：依赖、key 有无（值不显示）、端口、config 权限、铁律自检
scripts/verify-proxy.sh --port 18991 [--secret <s>]   # 校验运行中的代理（/health + /v1/models，零上游花费）
scripts/self-test.sh     # 跑离线回归套件（隔离，不碰 Science、不联网）
```

## 目录结构

```
proxy/csswitch_proxy.py          【主】provider 可切代理：deepseek 透传 / qwen 翻译
proxy/qwen_proxy.py              早期单 provider（千问）翻译代理，已被上者取代
scripts/make-virtual-oauth.mjs   虚拟 OAuth 伪造器（只写沙箱，护栏拒绝真实目录与符号链接）
scripts/launch-virtual-sandbox.sh 起沙箱 Science + 写虚拟登录 + 指向代理（推荐入口）
scripts/stop-science-sandbox.sh  停沙箱（按 data-dir，绝不碰真实 8765）
scripts/doctor.sh / verify-proxy.sh / self-test.sh   运维三件套（只读诊断 / 校验代理 / 离线回归）
scripts/daily-maintenance.*      每日巡检定时任务（只读 + 规划，见 CLAUDE.md）
test/                            隔离回归测试（只打代理，不碰 Science）
findings/                        证据与二进制分析记录
docs/superpowers/                设计规格与实现计划
desktop/                         Tauri 菜单栏 app（进程管家 + 前端面板）
~/.csswitch/config.json          运行期用户配置（不在仓库内，0600）
```

## 测试

```bash
bash test/run_all.sh     # python 单元 + node 伪造器 + bash 脚本/运维三件套，全绿即通过
```

全部测试在隔离环境运行，**不启动 Science、不联网上游**。

## 风险与边界

请先读上方[免责声明](#️-免责声明)。更完整的威胁模型、公开边界决策与法律/条款风险分析见 `docs/superpowers/specs/`。公开发布前，本项目设有一道法律/条款自查闸门。

## 许可

[MIT](./LICENSE)。
