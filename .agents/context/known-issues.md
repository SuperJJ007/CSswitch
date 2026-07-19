# 当前已知问题与证据缺口

最后整理：2026-07-19。已解决历史放入 CHANGELOG 或 dated evidence，不在这里重复。

## v0.9.0-beta.1 Linux 开发线

- Ubuntu 24.04 x64 适配、内部 deb workflow 和安装 smoke 已进入源码，但 workflow 尚未执行，也没有 `.deb` SHA-256、Ubuntu 安装态、X11/Wayland、Bubblewrap/AppArmor 或真实 Science 证据；不得写成 Linux 已发布或 runtime 已通过。
- Linux beta 不包含 WSL/WSLg、ARM64、musl、systemd、AppImage/rpm、远程监听或 no-sandbox 降级。
- Linux Codex 编译路径与私有文件/loopback合同已开放，但 live OAuth、动态模型和最小推理仍需用户授权测试账号与 Ubuntu 真机；macOS mock/单测不能替代。

## 分发

- v0.7.0 公开附件只有 ad-hoc seal，没有 Developer ID 团队身份、notarization 或 stapled ticket；Gatekeeper 对包内 app 的评估为 `rejected`。首次打开可能需要用户右键选择“打开”。

## Codex

- 公开 v0.7.0 中 Codex 是默认关闭的实验能力，边界为单账号、浏览器登录、macOS Apple Silicon。v0.9.0-beta.1 源码新增 Ubuntu x64 编译/运行合同，但上述 Linux live 证据尚未建立。上游账号权限、动态模型目录和 Responses 协议可能变化。
- 不支持设备码、多账号、代理认证、PAC、自定义 CA、系统代理自动发现或 TUN 检测；Finder 启动的环境变量与终端可能不同，`direct` 也可能仍由系统 TUN 接管。
- v0.7.0 的浏览器失败页在有效 callback 之后仍可能只显示 `Codex sign-in could not be completed safely.`，不能区分 token exchange 与本地认证提交。当前没有足够受影响样本确认代理、上游响应或本地提交中的具体根因。
- Acceptance 候选已有真实 CSSwitch OAuth、模型和 Science 最小文本成功证据，但最终公开 v0.7.0 DMG 没有重新执行真实 OAuth / 模型 / 推理；两者不能合并为同一层证据。
- v3 配置回滚到 v0.6.0 前必须先在 v0.7.0 导出并降级到 v2，或停止全部 CSSwitch 进程后恢复 v2 备份；删除 Codex profile 本身不会降低 schema。

## Science / Skill / SSH

- 安装、attach、load 与重启持久化不证明任一 Skill 的脚本、资产、网络、依赖或领域功能可用。
- 仅给名称时的来源搜索由 provider / Agent 能力决定；私有仓库、更新 / 覆盖、永久删除、恢复 UI 和 bundle 成员级物理删除不受支持。
- route attachment、nonce / CSRF control plane 与 `OPERON` Skill 绑定是观察到的 Science 合同；Science App 更新后必须重跑聚焦兼容性验证。
- Agent 控制面配置是多个顺序请求，不是原子事务；失败只降级为 warning，已完成步骤不会自动回滚。
- 系统 SSH 默认关闭；opt-in 后 config / wrapper 校验 fail closed，未对特定用户的真实 SSH server 做连通性验证。

## 测试

- 真机验收矩阵描述应执行的场景，不表示最终 v0.7.0 DMG 已逐项全部执行。每次验收必须绑定 exact artifact，并把通过、失败、环境阻塞与未执行分开记录。
