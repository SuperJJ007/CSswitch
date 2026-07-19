# 2026-07-19 CSSwitch 080 Linux Beta 社区内测 artifact

状态：**准确 GitHub Actions source gate、`.deb` 构建、包检查、hosted-runner 安装/Xvfb smoke、独立 digest 展示与四文件 artifact 通过；允许有限社区内测，不是公开 Release，也不是真实 Ubuntu 桌面、真实 Claude Science 或 live Codex OAuth 通过证据。**

## 身份与命名边界

- 内测通道名：`080-linux-beta`。
- 技术版本：`0.8.0-linux-beta.1`。
- 开发分支：`codex/v090-linux-x64`。
- 通过源码：`9806d4cf3230870deb95b270fb5a21511e3fa444`。
- 该开发线最初从 runtime commit `3630aac6350b734bdc775e98511ab932d98226f3` 继续；它是通过源码的祖先。
- 本证据形成时，本地 `v0.8.0^{}` 已由另一工作线指向 `4b163d50178791e7fbf9e085eb06fc2260baed4e`，当前 Linux 分支不包含该后续文档提交。因此 `080-linux-beta` 是内测通道名，不能写成“从当前 `v0.8.0` tag 精确构建”。本阶段没有创建、移动或推送 tag，也没有创建 PR 或 Release。
- 本记录日期：2026-07-19（Asia/Shanghai）。

## Actions 结果

- 运行：[Actions 29678051971](https://github.com/SuperJJ007/CSSwitch/actions/runs/29678051971)，事件 `push`，结论 `success`。
- job：[Ubuntu 24.04 x64 gate and deb / 88169038793](https://github.com/SuperJJ007/CSSwitch/actions/runs/29678051971/job/88169038793)，2026-07-19T07:24:29Z 至 07:37:43Z，结论 `success`。
- source gate、Tauri `.deb` build、包检查/安装 smoke 和 artifact 上传在同一个准确 source SHA 上全部通过。

| 层 | 实际结果 |
|---|---|
| source gate | Ubuntu 24.04 x64 runner 上 `bash test/run_all.sh --require-release-ready` 通过 |
| metadata | package=`cs-switch`、version=`0.8.0-linux-beta.1`、architecture=`amd64` |
| dependency/content | Bubblewrap 0.8+、socat、lsof、xdg-utils、openssh-client、Desktop/Gateway x86-64 ELF、脚本与图标合同通过 |
| checksum | runner 生成 basename-only SHA 文件，`sha256sum -c` 通过，并把完整 digest 写入 log、artifact summary 与独立 job Summary |
| installed smoke | runner 用本地 `.deb` 完成 `apt-get install`；fresh HOME Gateway 状态为脱敏 `state_missing` |
| Xvfb smoke | 进程启动、单实例和信号终止通过；不证明 GUI 可见、显式退出、Wayland/X11 或完整生命周期清理 |

Actions 有一条非阻塞维护 warning：`actions/checkout@v4`、`actions/setup-node@v4` 和 `actions/upload-artifact@v4` 的 Node 20 action runtime 被 GitHub runner 强制使用 Node 24。job 仍为成功；该提示不是 Linux 产品/runtime 通过证据，也不应通过放宽门禁消除。

## Artifact 与独立来源核验

- artifact：[csswitch-080-linux-beta-ubuntu-24.04-amd64](https://github.com/SuperJJ007/CSSwitch/actions/runs/29678051971/artifacts/8439725992)，ID `8439725992`，查询时唯一且 `expired=false`。
- 创建时间：2026-07-19T07:37:40Z；过期时间：2026-08-02T07:37:39Z；大小 `11,947,719` bytes。
- GitHub artifact archive digest：`sha256:406d674ba437a2fd451019f0663437afeade70fb7d047e03e07f397872e65bf8`。这是 GitHub archive digest，不是 `.deb` digest。
- `.deb`：`CSSwitch_0.8.0-linux-beta.1_amd64.deb`。
- `.deb` SHA-256：`ec004c2c8c6f54c1616ab9c2110fbc9d747bbb358200913ba3f00a29fa1cb686`。
- GitHub 运行页面的可见 job Summary 已人工回读，显示准确 source SHA、Debian version、完整 `.deb` SHA-256 和“真实桌面/Science/live OAuth 未建立”的范围说明。
- 下载到新的临时目录后，artifact 根目录恰好有四个文件：`.deb`、`.deb.sha256`、`README-TESTING.md`、`test-summary.txt`；没有旧版深层目录。Mac 本地重算 `.deb` SHA 与 job log/Summary 完全一致，basename checksum 返回 `OK`，artifact 内指南与该 source SHA 的仓库指南字节一致。

`.deb` 与 `.sha256` 位于同一个下载包时只能证明二者自洽，不能单独认证来源。测试者必须打开上述准确 Actions run，把 source SHA 与完整 `.deb` digest 和本地计算结果逐字比较；无法访问独立 run 身份或结果不一致时不得安装。

## 本机转发副本

为便于维护者转发，四个已核验文件被无路径层级地重新压缩为：

- `/private/tmp/CSSwitch-080-linux-beta-ubuntu-24.04-amd64.zip`
- 本机 ZIP SHA-256：`2eff5143869b7b6c79ee686ecfeb8c63f5cd1999a9e40fc2a1af7e66d9f57c01`

该 ZIP 是从已下载 artifact 生成的本机转发容器，不是 GitHub artifact archive 原字节，因此它的 hash 与 GitHub archive digest 不同。内层 `.deb` 字节和上述独立 `.deb` digest 未改变。ZIP 位于 `/private/tmp`，不提交到 git。

## 审查与未建立层

- 版本、workflow、当前合同和群友指南在 push 前由 `gpt-5.6-sol xhigh` 子 agent 独立复审。
- 复审先发现同包 checksum 不能认证来源、卸载前需停止/显式退出以及凭证措辞过宽；修正后最终结论为 P0/P1/P2/P3 全零。
- 本机 macOS 完整五层 release-ready 门禁通过；它与上述 Ubuntu Actions 证据分别记录。
- 群友应按 [Linux 内测指南](../../operations/linux-x64-beta-testing.md)校验、安装、停止、卸载和反馈；不得发送 token、nonce、账号正文、生产 API key、SSH 私钥、完整环境或真实 Science 数据。

下列层仍未建立：真实 Ubuntu GUI 可见与中文输入、分别的 X11/Wayland、真实 `xdg-open` 成功/失败路径、真实 Bubblewrap/AppArmor/userns runtime、真实 Claude Science lifecycle、crash/journal/无残留矩阵、live Codex OAuth/模型/最小推理、公开 tag/Release/长期分发。群友结果必须逐台、逐层记录，不能把若干人的局部通过合并成完整 beta 验收。
