# 已验证状态快照

最后复核：2026-07-23。当前维护基线为 v0.8.2；历史版本的固定证据不在本文件重复。

## v0.8.2 已发布

- 公开 peeled `v0.8.2`、发布时 `origin/main` 与 clean build source 均为 `0e740814c5cb30d7623757231ced882767f28a53`。
- 五层 release-ready Gate 退出 0；Desktop 为 378 passed / 7 explicit ignored，Gateway 为 272 lib + 1 CLI passed，loopback 为 102 passed。这是源码与隔离合同证据，不自动证明真实 provider、SSH server 或 Science live 推理。
- Tauri 首轮 app 外层 seal 不完整，正确被 `codesign --verify --deep --strict` 阻断且未上传。最终 app 经 Gateway→外层 app 的 ad-hoc seal，并从一次性空 staging 生成 DMG。
- 最终 DMG 的大小、SHA-256、GitHub digest、重新下载逐字节一致性、只读挂载根目录精确白名单、arm64 executable identity 与 app seal 已建立；根目录只有一个正式 app 和 Applications 链接。
- 最终 app 已经授权覆盖安装；应用目录只保留一个 CSSwitch。安装版 UI 连续三轮官方→第三方→一键开始均首次成功，代理与 Science 最终显示运行正常，OpenCode Go 卡片显示 `kimi-k3`。
- SSH 修复针对 Claude Science 隔离 HOME 的真实前置解析边界生成具体授权 Host 块；本机没有真实 SSH server，因此没有声称远端命令执行通过。
- 最终公开 artifact 没有执行真实 Grok/Gemini/OpenCode/Kimi/DeepSeek/Codex 推理。
- 最终 app 为 ad-hoc seal；没有建立 Developer ID、notarization、stapled ticket 或 Gatekeeper acceptance 证据。

## 证据入口

- 当前发布与附件：[v0.8.2 release evidence](../../docs/evidence/releases/v0.8.2.md)
- Acceptance 候选：[2026-07-17 browser-only Acceptance](../../docs/evidence/investigations/2026-07-17-codex-browser-only-acceptance.md)
- 历史版本：[发布证据索引](../../docs/evidence/releases/README.md)

本文件不保存本机 worktree 路径、临时 artifact 位置或可漂移的工作区数量；这些状态必须实时查询。
