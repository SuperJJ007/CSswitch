# 依赖分析：CSSwitch 能不能开箱即用

> 目的：把「缺 node」这类报错的**问题本身**研究清楚，供决策与实机测试对照。不含具体改法（改法见 [`../CHANGELOG.md`](../CHANGELOG.md) / [`known-issues.md`](known-issues.md)）。

> **进展（2026-07-03，v0.1.4 已发布）：node 缺口已从根上拔掉。** 虚拟 OAuth 伪造器已用 Rust 原生密码学重写（`desktop/src-tauri/src/oauth_forge.rs`），与 `.mjs` 的 v2 GCM 格式字节兼容（node↔rust 对拍单测过），一键流程零 node。**A 类和 B 类都解决**（不管装没装 node）。**剩 python3 一个缺口**（代理，下一步同法移 axum）。下文分析保留为问题记录。

## 一句话结论

一键越过登录的整条流程里，**「用户机器上不保证存在」的运行时依赖只有两个：`node` 和 `python3`**，而且**都是我们自己 shell out 去调解释器造成的**。其余全是 macOS 自带（zsh/bash/security/open/pkill/WebView）或 Science 本身（给定）。把这两处编译进 Rust，CSSwitch 就只依赖「macOS + 已装 Science」——对目标用户是给定的，即真正开箱即用。

## 完整依赖面（点「一键越过登录」→ Science 起来）

| 依赖 | 用途 | 用户机器上保证吗 |
|------|------|----------------|
| `zsh` / `bash` | 跑启动/停止脚本 | ✅ macOS 自带 |
| `security` | 沙箱独立钥匙串（避免弹钥匙串窗） | ✅ macOS 自带（Keychain CLI） |
| `open` / `pkill` | 开浏览器 / 清进程 | ✅ macOS 自带 |
| WKWebView | 面板 UI（Tauri 用系统 WebView） | ✅ macOS 自带 |
| Claude Science.app | 沙箱要跑的本体 | ⭘ 目标用户必装 = 给定 |
| **`node`** | 伪造虚拟 OAuth（`make-virtual-oauth.mjs`） | ❌ **不保证** ← 缺口 |
| **`python3`** | 翻译代理（`csswitch_proxy.py`，纯标准库、无 pip 依赖） | ❌ **不保证**（见洞察 2） ← 缺口 |

代理是**纯 Python 标准库**（无第三方包）→ 只要 python3 能跑就行，不需要 pip install。这也意味着把它移 Rust 时没有隐藏的第三方逻辑要搬。

## 洞察 1（关键）：缺 node 有两种，PATH 兜底只治其中一种

- **A 类：装了 node，但 GUI 从访达启动只有最小 PATH、看不见它**（如那台装了 22.14.0 的测试机）。→ v0.1.3 的 PATH 两层兜底（常见目录 + 登录 shell）**能治**。
- **B 类：根本没装 node**。目标用户是**做研究的、不一定是开发者**，凭什么装 node？→ PATH 兜底**一点用没有**；只有 node-ectomy（伪造器移 Rust）或打包 bundle 一个 node 才能治。

**「好几个客户」里若有 B 类，v0.1.3 对他们无效**；node-ectomy 对这些人不是「更好」，是「唯一能用」。**这是实机测试第一件要分清的事**（见下）。

## 洞察 2：python3 也埋着同样的雷（只是当前没爆）

- 干净 Mac（没装 Xcode 命令行工具）上，`/usr/bin/python3` 是个**占位程序**，一跑就弹「安装开发者工具」，并不能直接用。目标用户不一定是开发者 → 不能假设 python3 一定能用。
- **但当前客户没踩到它**：代码里 `one_click_login` **先查 python3（起代理）、再查 node**。客户报的是「缺 node」而非「缺 python3」，**反推出这些机器上 python3 是好的、只有 node 不行**。所以就当前反馈而言，node 是唯一活跃缺口。
- 结论：python3 现在不是活跃问题，但对更广的用户不保证；真开箱即用，最终它也得移 Rust（axum）。

## 洞察 3：只有这两个是「自找的」

其余依赖全是 OS 自带或 Science 本身，删不掉也不用删。**唯一让 CSSwitch 不能开箱即用的，就是我们自己起的 node 与 python 两个解释器**。cc-switch（同为 Tauri 2）连 live 代理都编译进单个 Rust 二进制、对用户零依赖——证明这条路走得通（打包这层可抄，OAuth 伪造的逻辑抄不了，因为 Claude Code 不需要登录、它没有伪造器）。

## 实机测试要分清的事（在复现「缺 node」的机器上，用终端 = 你真实 shell）

1. `command -v node`
   - **有输出** → A 类（PATH 问题，v0.1.3 能治）。顺带记下路径（Homebrew？nvm？fnm？）。
   - **无输出** → B 类（根本没装，只有移 Rust / bundle 能治）。
2. `python3 --version`
   - 正常打印版本 → python3 可用。
   - 弹「安装命令行工具」/报错 → 是占位，python3 也是潜在缺口。
3. 观察这几台失败机器**是否普遍没装 node** → 估 B 类占比 → 决定 node-ectomy 紧不紧急。
4.（等 v0.1.3 出了构建后）装上新版，看 A 类机器是否不再误报。

## 决策含义（稍后定，不在本文档下结论）

- 失败机多为 **A 类** → v0.1.3 PATH 兜底救大部分，node-ectomy 从容排 0.1.4。
- 不少是 **B 类** → v0.1.3 救不了他们，node-ectomy 是刚需，应提前。
- 无论 A/B，**治本都是把 node（先）与 python（后）移进 Rust**。PATH 兜底只是止血。
