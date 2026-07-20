# 已验证状态快照

最后复核：2026-07-20。当前维护基线为 v0.8.1；历史版本的固定证据不在本文件重复。

## v0.8.1 已发布

- 公开 peeled `v0.8.1`、发布时 `origin/main` 与 clean build source 均为 `c93c7e64d75703d38f08c385ed94460b5057831b`。
- 五层 release-ready Gate 退出 0；Desktop 为 361 passed / 5 explicit ignored，Gateway 为 271 lib + 1 CLI passed，loopback 为 99 passed，前端模型目录/UI 为 15 passed。这是源码与隔离合同证据，不自动证明真实 provider 或 Science live 行为。
- Tauri 首轮 app 外层 seal 不完整，正确被 `codesign --verify --deep --strict` 阻断且未上传。最终 app 经 Gateway→外层 app 的 ad-hoc seal，并从一次性空 staging 生成 DMG。
- 最终 DMG 的大小、SHA-256、GitHub digest、重新下载逐字节一致性、只读挂载根目录精确白名单、arm64 executable identity 与 app seal 已建立；根目录只有一个正式 app 和 Applications 链接。
- 最终 app 已经授权覆盖安装；UI 显示 v0.8.1、DeepSeek Flash/Pro 角色分工和 Kimi K3 默认，并列出 OpenCode Go 双协议、Grok 与 Gemini。旧 profile 未被 preset 静默迁移。
- 最终公开 artifact 没有执行真实 Grok/Gemini/OpenCode/Kimi/DeepSeek/Codex 推理，没有构造真实多组织 Sign out 恢复，也没有证明 Claude Science 自身 SSH 前置解析器接受 `Include` bridge。
- `#63` 与 `#8` 已回复并关闭；`#56`、`#12`、`#55`、`#19`、`#11` 已回复并保持打开。
- 最终 app 为 ad-hoc seal；没有建立 Developer ID、notarization、stapled ticket 或 Gatekeeper acceptance 证据。

## 证据入口

- 当前发布与附件：[v0.8.1 release evidence](../../docs/evidence/releases/v0.8.1.md)
- Acceptance 候选：[2026-07-17 browser-only Acceptance](../../docs/evidence/investigations/2026-07-17-codex-browser-only-acceptance.md)
- 历史版本：[发布证据索引](../../docs/evidence/releases/README.md)

本文件不保存本机 worktree 路径、临时 artifact 位置或可漂移的工作区数量；这些状态必须实时查询。
