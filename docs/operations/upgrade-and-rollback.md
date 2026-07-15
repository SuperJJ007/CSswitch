# CSSwitch 0.6.0 升级与回滚 / Upgrade and rollback

本说明适用于 macOS Apple Silicon 的 CSSwitch 0.6.0。此版本继续复用 Science 持久化 data-dir，把 v0.5.0 外部 Skill 路由迁移为支持 GitHub bundle、本地 ZIP 和整包确认卸载的合并 connector，并保留用户 MCP 配置、未知字段和精确的旧 proxy 清理。

This guide applies to CSSwitch 0.6.0 for macOS Apple Silicon. It keeps reusing Science's persistent data-dir, migrates the v0.5.0 external-Skill route to a combined connector for GitHub bundles, local ZIPs, and confirmed whole-bundle removal, preserves user MCP entries and unknown fields, and retains exact legacy-proxy cleanup.

## 升级前 / Before upgrading

1. 在 CSSwitch 中停止当前第三方链路，然后退出 CSSwitch。
2. 备份整个 `~/.csswitch/`，包括配置、日志和 Skill Manager store/inventory。
3. 不要删除 `~/.csswitch/sandbox/`。覆盖安装 app 不应删除该目录，但手工删除会影响隔离 Science 状态与历史数据。
4. 确认下载文件名和目标版本是 `CSSwitch_0.6.0_aarch64.dmg` / `0.6.0`。

1. Stop the active third-party path in CSSwitch, then quit CSSwitch.
2. Back up all of `~/.csswitch/`, including configuration, logs, and Skill Manager store/inventory.
3. Do not delete `~/.csswitch/sandbox/`. Replacing the app should not remove it, but manual deletion can remove isolated Science state and history.
4. Confirm that the download and target version are `CSSwitch_0.6.0_aarch64.dmg` / `0.6.0`.

## 覆盖安装 / In-place install

1. 打开 DMG，把 CSSwitch 拖入「应用程序」并选择替换旧版。
2. 首次打开如果被 macOS 阻止，在 Finder 中右键 CSSwitch，选择「打开」。0.6.0 当前为 ad-hoc 签名且未公证；这不等于 Developer ID、notarization 或 Gatekeeper 已验证。
3. 打开 CSSwitch，确认已有 profile 仍存在，再执行一次「设为当前」。
4. 先用最小请求验证常用 provider，再恢复日常工作。

1. Open the DMG, drag CSSwitch into Applications, and replace the older copy.
2. If macOS blocks the first launch, right-click CSSwitch in Finder and choose “Open.” The current 0.6.0 package is ad-hoc signed and not notarized; this is not Developer ID, notarization, or Gatekeeper verification.
3. Open CSSwitch, confirm that existing profiles remain, then run “Set active” once.
4. Send one minimal request through your usual provider before resuming normal work.

## 回滚 / Rollback

0.6.0 保持 v2 配置 schema，但回滚仍应先备份整个 `~/.csswitch/`。退出 CSSwitch，确认 app 与 `csswitch-gateway` 均已停止，然后用上一稳定版 `.dmg` 覆盖 `/Applications/CSSwitch.app`。不要同时运行两个版本，也不要把 0.6.0 的 sidecar 单独复制进旧版 app。回滚不会删除 Science data-dir、已安装 Skill、bundle manifest、隔离回收内容或旧 Skill Manager 数据；旧版可能无法管理 v0.6.0 bundle 元数据。

Version 0.6.0 keeps the v2 configuration schema, but back up all of `~/.csswitch/` before rollback. Quit CSSwitch, confirm that both the app and `csswitch-gateway` have stopped, then replace `/Applications/CSSwitch.app` using the previous stable DMG. Do not run two versions at once or copy the 0.6.0 sidecar into an older app. Rollback does not delete the Science data-dir, installed Skills, bundle manifests, quarantined content, or legacy Skill Manager data; an older app may not understand v0.6.0 bundle metadata.

回滚只替换应用程序，不自动回退或删除 `~/.csswitch` 数据。若旧版无法读取升级后的配置，请退出旧版，把备份的 `config.json` 恢复到原位并保持文件权限为 `0600`。不要在 CSSwitch 或 Science 运行时修改配置文件。

Rollback replaces only the app; it does not automatically revert or delete `~/.csswitch` data. If the older app cannot read the post-upgrade config, quit it, restore the backed-up `config.json`, and keep permissions at `0600`. Do not edit the config while CSSwitch or Science is running.

## 证据边界 / Evidence boundary

本说明描述安全操作步骤，不证明某个具体下载附件已经通过 hash、签名、公证、Gatekeeper、真实账号或 live provider 验证。每个发布附件都应在对应 release evidence 中单独记录。

This guide describes safe operational steps. It does not prove that a particular download passed hash, signing, notarization, Gatekeeper, real-account, or live-provider verification. Each release artifact needs its own release evidence.
