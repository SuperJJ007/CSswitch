# 当前已知问题与证据缺口

最后整理：2026-07-20。已解决历史放入 CHANGELOG 或 dated evidence，不在这里重复。

## v0.8.1 已发布后的边界

- OpenCode Go、Grok 与 Gemini 的 0.8.1 门禁覆盖文本、多轮、tools / `tool_choice`、模型发现或手填、标题和 classifier；图片、厂商专有 reasoning、原生流式与结构化输出仍为 limited，Gemini native API 不在本版范围。
- 当前已建立 clean source、单测、隔离 mock / loopback、最终安装 UI 和公开附件回读。最终 artifact 没有执行 OpenCode Go、Grok、Gemini、Kimi、DeepSeek 或 Codex 的用户 key / OAuth live 推理，不能由本地门禁替代。
- 多历史组织恢复已有精确 marker / choice / identity 测试，最终安装版没有构造真实多组织 Sign out 数据集；SSH bridge 已通过系统 OpenSSH 测试，尚未证明 Claude Science 自身前置解析器接受该 `Include`。

## 分发

- v0.8.1 公开附件只有经过完整性验证的 ad-hoc seal，没有建立 Developer ID、notarization、stapled ticket 或 Gatekeeper acceptance 证据。首次打开可能需要用户右键选择“打开”。

## Codex

- Codex 是默认关闭的实验能力。上游账号权限、动态模型目录和 Responses 协议可能变化；单账号、浏览器登录、macOS Apple Silicon 是当前边界。
- 不支持设备码、多账号、代理认证、PAC、自定义 CA、系统代理自动发现或 TUN 检测；Finder 启动的环境变量与终端可能不同，`direct` 也可能仍由系统 TUN 接管。
- v0.7.0 曾观察到浏览器失败页在有效 callback 后只显示通用安全错误；v0.8.0 增加了结构化通知与浏览器 fallback，但最终公开 DMG 未重跑该真实账号失败路径，不能据此宣布上游或本地提交根因已被穷尽。
- 历史 Acceptance 候选已有真实 CSSwitch OAuth、模型和 Science 最小文本成功证据，但最终公开 v0.8.0 DMG 没有重新执行真实 OAuth / 模型 / 推理；两者不能合并为同一层证据。
- v4 配置回滚到 v0.7.0 或更早版本前，必须先在 v0.8.0 导出并降级到 v2，或停止全部 CSSwitch 进程后恢复兼容备份；删除 profile 本身不会降低 schema。

## Science / Skill / SSH

- 安装、attach、load 与重启持久化不证明任一 Skill 的脚本、资产、网络、依赖或领域功能可用。
- 仅给名称时的来源搜索由 provider / Agent 能力决定；私有仓库、更新 / 覆盖、永久删除、恢复 UI 和 bundle 成员级物理删除不受支持。
- route attachment、nonce / CSRF control plane 与 `OPERON` Skill 绑定是观察到的 Science 合同；Science App 更新后必须重跑聚焦兼容性验证。
- Agent 控制面配置是多个顺序请求，不是原子事务；失败只降级为 warning，已完成步骤不会自动回滚。
- 系统 SSH 默认关闭；opt-in 后 config / wrapper 校验 fail closed，未对特定用户的真实 SSH server 做连通性验证。

## 测试

- 真机验收矩阵描述应执行的场景，不表示最终 v0.8.1 DMG 已逐项全部执行。每次验收必须绑定 exact artifact，并把通过、失败、环境阻塞与未执行分开记录。
