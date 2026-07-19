# 2026-07-19 Linux x64 Actions 与内部 artifact 证据

状态：**GitHub Actions 源码门禁、`.deb` 构建、包检查、安装态 Xvfb smoke 与内部 artifact 通过；不是真实 Ubuntu 桌面、真实 Claude Science 或 live Codex OAuth 通过证据。**

## 身份

- 冻结起点：`v0.8.0@3630aac6350b734bdc775e98511ab932d98226f3`；本轮核对时 tag 仍指向该 commit，未移动或覆盖。
- 开发分支：`codex/v090-linux-x64`。
- 通过源码：`436f88243fd131245ff440f96f0255506157ad1c`。
- 工作流：`.github/workflows/linux-x64-internal.yml`，由该分支的窄范围 `push` 触发。
- 运行：[Actions 29676582825](https://github.com/SuperJJ007/CSSwitch/actions/runs/29676582825)，事件 `push`，结论 `success`。
- job：[Ubuntu 24.04 x64 gate and deb / 88164991774](https://github.com/SuperJJ007/CSSwitch/actions/runs/29676582825/job/88164991774)，运行于 2026-07-19 06:31:42Z 至 06:44:26Z。
- 本记录日期：2026-07-19（Asia/Shanghai）。未创建 tag、PR 或 Release。

## Actions 实际通过范围

同一个准确 source SHA 上，下列步骤全部返回 `success`：

| 层 | 实际证明 |
|---|---|
| source gate | `bash test/run_all.sh --require-release-ready` 在 Ubuntu 24.04 hosted x64 runner 通过 |
| build | `npm run tauri build -- --bundles deb` 成功生成唯一 `.deb` |
| metadata | Debian package=`cs-switch`、version=`0.9.0-beta.1`、architecture=`amd64` |
| dependency contract | `bubblewrap (>= 0.8)`、`socat`、`lsof`、`xdg-utils`、`openssh-client` 检查通过 |
| package contents | Desktop Entry 精确解析出的 Desktop 与唯一 Gateway 均为可执行 x86-64 ELF；脚本与 SSH wrapper 存在且可执行，图标存在 |
| checksum | runner 生成 basename-only SHA-256 文件并立即用 `sha256sum -c` 复核 |
| installed smoke | runner 用本地 `.deb` 完成 `apt-get install`；fresh HOME 的 Gateway Codex 状态为脱敏 `state_missing` |
| Xvfb smoke | 只证明 Desktop 进程启动、单实例与信号终止，不证明 GUI 可见或显式退出 |
| artifact | 唯一内部 artifact 上传成功，保留期 14 天 |

该 hosted runner 没有下载或执行约 5 GB 的真实 Claude Science，也没有使用真实 Claude Science、Codex、Provider 或用户凭证。

## Artifact 独立核验

- artifact：`csswitch-v0.9.0-beta.1-ubuntu-24.04-amd64`，ID `8439191268`。
- 创建时间：2026-07-19T06:44:23Z；过期时间：2026-08-02T06:44:22Z；查询时 `expired=false`。
- archive 大小：`11,945,650` bytes。GitHub 返回的 archive digest 为 `sha256:b99e2f7ff6334d92f0c27b1478155f47a21d474291dc94bd6509396a91605d21`；它是 artifact archive digest，不是下述 `.deb` digest。
- `.deb`：`CSSwitch_0.9.0-beta.1_amd64.deb`。
- `.deb` SHA-256：`8a47faaf5bdf5aaf93bf3e555fd5db5cff7c155d1ba5eb332521fb09bf463cc9`。
- SHA 文件只记录 `.deb` basename；下载到新的 Mac 临时目录后，从 SHA 文件所在目录执行 `shasum -a 256 -c`，结果为 `OK`。
- 下载内容恰好是一份 `.deb`、一份 `.sha256` 和一份脱敏摘要。GitHub 下载时保留了上传输入的目录层级，这不改变文件身份或 checksum。
- 摘要中的 `source_sha` 精确等于通过源码；另记录 `runner=ubuntu-24.04`、`architecture=amd64`、source gate、metadata、fresh-HOME Gateway 与 Xvfb smoke 为 `pass`。

下载和复核只发生在 `/private/tmp/csswitch-linux-artifact-29676582825.hfTr5M`；该临时副本不是仓库构建产物，也不提交到 git。

## 同日失败运行的历史

以下运行用于追踪门禁如何暴露问题；它们不组成或替代最终通过证据：

| run / source | 停止层 | 根因与处置 |
|---|---|---|
| [29674443164](https://github.com/SuperJJ007/CSSwitch/actions/runs/29674443164) / `0f83d0b` | source gate | 暴露 Python 3.12 本地 `test` package 导入、GNU/BSD `stat`、缺失 lsof 测试确定性及 Linux Rust cfg/clippy 可移植性问题；逐项修复，没有放宽 fail-closed |
| [29674993156](https://github.com/SuperJJ007/CSSwitch/actions/runs/29674993156) / `4ae31bb` | source gate | 暴露已安装 controller 的 macOS lsof 固定路径与两个 mock request 提交竞态；改为平台固定受信任路径和请求计数同步 |
| [29675546191](https://github.com/SuperJJ007/CSSwitch/actions/runs/29675546191) / `7a58f7b` | package inspect | `.deb` 已构建，但 workflow 错把实际 Debian package `cs-switch` 预期为 `csswitch`；收紧为实际精确 metadata 合同 |
| [29676149677](https://github.com/SuperJJ007/CSSwitch/actions/runs/29676149677) / `a0d0014` | installed smoke | source gate、build、metadata、内容和 SHA 均通过；`apt-get` 把不带 `./` 的相对 `.deb` 路径当仓库包名。改为唯一产物的规范化绝对路径 |
| [29676582825](https://github.com/SuperJJ007/CSSwitch/actions/runs/29676582825) / `436f882` | 无 | 同一准确 SHA 的完整流程全部通过并上传 artifact |

每项实际内容或架构修复均在进入下一次 push 前由 `gpt-5.6-sol xhigh` 子 agent 独立只读复审；最终两项 CI 路径修复的复审结论均为 P0/P1/P2/P3 为零。

## 尚未建立

| 证据层 | 状态与边界 |
|---|---|
| Ubuntu 24.04 真实安装态 | `NEEDS-REAL-MACHINE`；hosted runner/Xvfb 不能替代临时云 VM |
| GUI 可见、中文输入、隐藏/显示、显式退出 | `NEEDS-REAL-MACHINE` |
| Xorg 与真实 Wayland | `NEEDS-REAL-MACHINE`，两者必须分别记录 |
| `xdg-open` 成功、失败 fallback/retry | `NEEDS-REAL-MACHINE` |
| Bubblewrap/AppArmor/user namespace runtime | 只完成包依赖与测试合同；真实 sandbox 尚未执行 |
| 端口/身份/crash/journal/无残留矩阵 | 源码测试已通过；真实安装态 runtime 尚未执行 |
| 真实 Claude Science | 未安装、未执行；验收当日须重新核对 stable pointer/manifest |
| Codex live OAuth、动态模型、最小推理 | 未执行；必须先取得用户对测试账号的明确授权 |
| tag / Release / 公开分发 | 未创建、未发布 |

因此本证据只把上一份[源码实施记录](2026-07-19-linux-x64-beta-implementation.md)中的 Actions、`.deb` 与 hosted-runner smoke 缺口升级为通过。它不把完整 `v0.9.0-beta.1` 真机验收记为通过。
