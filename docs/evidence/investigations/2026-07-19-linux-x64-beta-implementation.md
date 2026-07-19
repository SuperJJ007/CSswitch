# 2026-07-19 Linux x64 beta 实施记录

状态：**日期化源码实施证据，不是 Linux artifact、安装态或真实 runtime 通过证据。**

## 身份

- 冻结起点：`v0.8.0@3630aac6350b734bdc775e98511ab932d98226f3`。
- 开发版本：`v0.9.0-beta.1`。
- 目标：Ubuntu 24.04 x86_64/glibc，内部 `.deb`。
- 本记录日期：2026-07-19（Asia/Shanghai）。
- `v0.8.0` tag 未移动。本文件的初始源码实施记录形成时尚未 commit/push；后续 GitHub Actions 与 artifact 状态必须写入新的日期化证据，不回写本节。未创建 tag、PR 或 Release。

## 当日上游复核

Claude Science 官方文档当日仍声明 macOS 与 Linux beta，Linux 仅支持 x64 glibc，并要求约 5 GB、socat、Bubblewrap 0.8.0+ 与可用非特权 user namespace：[Requirements](https://claude.com/docs/claude-science/overview)、[Linux install](https://claude.com/docs/claude-science/get-started)、[remote Linux](https://claude.com/docs/claude-science/run-on-remote-linux-server)。

官方 stable pointer 当日返回 build `17bca090`；对应 manifest 为版本 `0.1.20`、build date `2026-07-17T01:22:41Z`，包含 `linux-x64` SHA-256 `1dc492d52fc09e876835a0f20455fa6e578df349a96adaff823edfd401d7801a`。这是当日外部版本身份，不是 CSSwitch 已下载、执行或兼容该 binary 的证据：[stable pointer](https://storage.googleapis.com/operon-dist-cf94a20e-f71c-413c-bd00-9e12b1fedf59/operon-releases/stable)、[manifest](https://storage.googleapis.com/operon-dist-cf94a20e-f71c-413c-bd00-9e12b1fedf59/operon-releases/17bca090/manifest.json)。

历史 Linux/WSL 调研绑定它自己的旧冻结 commit，未被改写成 `3630aac` 当前事实。本实施记录只引用重新核实的上游身份和本分支实际源码。

## 已实现的源码层

- Desktop 新增 macOS/Linux platform adapter：Science 安装路径、browser opener、lsof/ps/id/kill 受信任路径、平台能力与 Linux environment blockers。
- Linux preflight 对 x86_64、Bubblewrap 版本、实际 user namespace probe、socat 与 lsof fail closed；没有 no-sandbox 分支。
- Science launch/stop/doctor 脚本改为 macOS Bash 3.2 与 Ubuntu Bash 兼容；Keychain cleanup 仅在 macOS 执行。
- `get_config` 只读返回 `os`、`arch`、`support_tier`、`official_mode_supported`；schema 保持 v4。
- Linux UI 隐藏 Official Claude，backend 直接拒绝；迁入 official 配置会原子归一为 proxy 并产生一次性说明。
- Desktop、Gateway 与 Codex OAuth 在 Linux 使用固定 `/usr/bin/xdg-open`；Codex sidecar 仅继承固定 HOME/网络和桌面 session allowlist。
- Tauri/npm/Cargo 版本统一为 `0.9.0-beta.1`；deb 元数据声明 Bubblewrap、socat、lsof、xdg-utils 与 openssh-client 依赖。
- 新增 Ubuntu 24.04 hosted runner workflow，执行 release-ready gate、deb 构建、包内容检查、临时 HOME/Xvfb 安装 smoke、SHA-256 与 14 天内部 artifact 上传。

## CI 前安全收口

同日独立审查发现的 CI 前 P1 均在源码层收口：

- Linux launch/stop/doctor 固定 `/bin/bash`；Science `--version`、`status`、`url`、`serve`、`stop` 清空宿主环境，共用权限 `0700` 的隔离 HOME/XDG/TMPDIR，并丢弃宿主 provider/API/Git/SSH 变量。
- system SSH 默认不继承 agent；opt-in 会拒绝相对、缺失、符号链接 config，且仅转交存在的绝对 Unix `SSH_AUTH_SOCK`。
- Linux WebView spike 编译路径禁用；Science URL 只接受当前端口的 loopback HTTP，无 userinfo/fragment，长度有界。
- exact nonce URL 仅在后端内存中交给 opener；失败响应只有脱敏 `http://127.0.0.1:<port>/…` 与 retry，不再提供复制完整 URL。
- `.deb` workflow 改为从 Desktop Entry 精确定位 Desktop、唯一定位 Gateway，检查两个 x86-64 ELF、依赖/资源/权限、Gateway 脱敏空状态及 basename SHA；Xvfb 只声明进程启动、单实例和信号终止。
- 合同测试新增 hostile PATH/环境隔离、SSH config/agent、URL 拒绝矩阵、Linux browser-first 与脱敏 fallback。测试还暴露并修复了脚本为比较路径而尝试进入真实 `.claude-science` 的旧行为；现在只 canonicalize 外层 HOME，不读取真实 Science 目录。
- CI 前复审随后发现 Skill/MCP 的 `science url` 尚未复用 Desktop 的完整 Linux XDG 环境。严格环境 builder 已下沉到共享 `skill-package` core，Desktop 和 Skill/MCP 均调用同一实现；`test/run-rust.sh` 也显式运行该 crate 的 fmt、clippy 与 tests，避免 path dependency 只编译不测试的假绿。
- 同一轮 `gpt-5.6-sol xhigh` 修后复审结论为 P0=0、P1=0。保留一个已知 P2：目录初始化拒绝静态 symlink，但路径式 check/create 尚未用 fd-anchored `openat`/`mkdirat` 闭合同 UID 并发替换竞态；本 beta 不把它表述为已消除。

## 本机验证

本机是 macOS，不是 Linux：

- 最终命令：`RUST_TEST_THREADS=1 bash test/run_all.sh --require-release-ready`，使用完整系统 PATH 并允许隔离回环 mock。
- 结果：`offline=pass`、`loopback=pass`、`scripts=pass`、`rust=pass`、`frontend=pass`；退出码 0，`release-ready green: YES`。
- Desktop Rust 359 项中 355 通过、4 项按既有合同显式 ignored；共享 Skill core 60 项中 56 通过、4 项显式 ignored；Gateway library 233 项与 CLI integration 1 项通过。
- Rust fmt/clippy：Desktop、共享 Skill core 与 Gateway 均通过；Node/Bash 语法、workflow YAML 和 `git diff --check` 通过。
- 验证中曾用缺少 `/usr/sbin` 的手工 PATH 触发 real-machine guard fail-closed；恢复完整系统 PATH 后该层全部通过。该失败是验证环境构造错误，不是通过项，也没有通过放宽 guard 处理。

以上只建立 macOS 源码回归。Linux 编译、workflow、deb、Xvfb 安装态、Ubuntu GUI、Bubblewrap/AppArmor、真实 Science、真实 Codex OAuth 和公开分发均未由这些结果建立。

## 未建立

| 证据层 | 状态 |
|---|---|
| GitHub Actions workflow | 未执行；分支尚未 push |
| 内部 `.deb` / SHA-256 | 未生成 |
| Ubuntu 24.04 安装态 | `NEEDS-REAL-MACHINE` |
| X11 / Wayland / 中文输入 / xdg-open | `NEEDS-REAL-MACHINE` |
| Claude Science 0.1.20 真实隔离 lifecycle | `NEEDS-REAL-MACHINE` |
| Linux Codex live OAuth / model / inference | 未执行；需用户授权测试账号 |
| tag / Release / 分发 | 未创建、未发布 |

后续只能在对应环境完成后追加新的日期化证据；不得把本文件的源码结论升级成 artifact 或 runtime 通过。
