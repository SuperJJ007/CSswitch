# CSSwitch 080 Linux Beta 群友内测指南

版本：`0.8.0-linux-beta.1`。支持范围仅为 Ubuntu 24.04 x86_64/glibc 原生桌面；不支持 WSL/WSLg、ARM64、musl、AppImage 或 rpm。

这是有限社区内测包，不是公开 Release，也不表示真实 Claude Science、Codex OAuth、X11/Wayland 或完整生命周期已经验收。建议在可恢复的测试机或临时用户中使用，不要处理重要 Science 数据，不要使用无法撤销或高价值的生产凭证。

## 收到的文件

完整 artifact 应恰好包含：

- `CSSwitch_0.8.0-linux-beta.1_amd64.deb`
- `CSSwitch_0.8.0-linux-beta.1_amd64.deb.sha256`
- `test-summary.txt`
- `README-TESTING.md`（本指南）

只接受维护者从同一个成功 GitHub Actions run 下载并保持原字节的文件。维护者还必须提供该次准确 Actions run URL、source SHA 和完整 64 位 `.deb` SHA-256。测试者应直接打开该 GitHub run 的 Summary，把本地计算值与其中的独立可信值逐字比对。`.deb` 与 `.sha256` 来自同一个群文件或网盘时，二者自洽只能发现传输损坏，不能单独认证来源；无法访问独立 run 记录、run/source 身份不一致或 digest 不一致时，不要安装。

## 校验与安装

在四个文件所在目录运行：

```bash
sha256sum CSSwitch_0.8.0-linux-beta.1_amd64.deb
sha256sum -c CSSwitch_0.8.0-linux-beta.1_amd64.deb.sha256
sudo apt install ./CSSwitch_0.8.0-linux-beta.1_amd64.deb
```

第一条输出的完整 64 位 digest 必须与准确 GitHub Actions run Summary 中的 `.deb SHA-256` 逐字一致，第二条必须显示 `OK`，满足两项后才能执行第三条。`apt` 会按包声明处理 Bubblewrap 0.8+、socat、lsof、xdg-utils 和 openssh-client 等依赖。不要通过 `dpkg --force-*`、`--no-sandbox` 或放宽系统权限绕过安装/preflight 失败。

Claude Science 必须由测试者按照[官方 Linux 安装说明](https://claude.com/docs/claude-science/get-started)单独安装；CSSwitch 不下载或升级 Science。默认 executable 是 `$HOME/.local/bin/claude-science`，约 5 GB。没有安装 Science 时仍可记录 CSSwitch GUI 是否能启动以及 preflight 的脱敏 blocker，但不能把 runtime 记为通过。

## 建议测试顺序

1. 从桌面应用菜单启动 CSSwitch，记录窗口是否可见、是否只有一个实例以及界面文字是否正常。
2. 确认 Linux 没有“官方 Claude”模式；不要尝试修改配置文件强开该模式。
3. 点击启动前记录 preflight 结果。Bubblewrap 或 user namespace 被系统阻止时，应显示有界 blocker，不应出现 no-sandbox 旁路。
4. 使用低风险测试 provider/profile 验证 Gateway、模型列表和最小文本请求。不要使用生产 API key 或重要 Science 项目。
5. 验证浏览器能否由 `xdg-open` 打开；失败提示只应显示 `http://127.0.0.1:<port>/…`，不得包含 query、nonce 或账号信息。
6. 分别记录 X11 或 Wayland 会话、中文输入、停止、退出、重新打开和异常退出后的恢复。不要把其中一种桌面会话推断为另一种也通过。
7. system SSH 保持默认关闭。只有明确愿意测试时才启用，并使用临时 ssh-agent/config；不要发送 SSH 私钥、agent socket 路径或完整 SSH 配置。
8. Codex OAuth 属于可选 live 测试。只在测试者明确愿意使用自己的测试账号时执行；反馈只写成功/取消/超时、过期区间和脱敏账号 hash，绝不发送 token、授权 URL、PKCE、callback 参数、邮箱或账号正文。

## 反馈模板

可以把下面内容发给维护者或提交到 [GitHub Issues](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml)：

```text
CSSwitch package: 0.8.0-linux-beta.1
SHA-256 前 12 位：
Ubuntu 版本：
架构（应为 x86_64）：
桌面环境：
会话：X11 / Wayland
Claude Science 版本（未安装则写未安装）：
Bubblewrap 版本：

通过：安装 / 启动 / 单实例 / xdg-open / Gateway / Science / 停止 / 重开
未执行：
失败步骤：
最小复现：
实际结果：
预期结果：
```

可以运行 `uname -m`、`bwrap --version` 和读取 `/etc/os-release` 填写环境；不要发送完整 `env`、HOME 目录、`~/.csswitch`、`~/.claude-science`、浏览器地址栏或未经检查的日志。截图前遮盖用户名、路径、模型账号、API key、nonce 和私有 URL。

## 卸载

先在 CSSwitch 中执行“停止”，等待 UI 报告本次受管 runtime 已停止，再使用应用提供的显式退出入口关闭 CSSwitch。停止或退出失败时先记录现象，不要继续卸载，也不要使用宽泛的 `pkill`、`killall` 或按端口猜测并杀进程；把问题报告给维护者确认归属。

确认本次测试会话已收口后再运行：

```bash
sudo apt remove cs-switch
```

卸载包不等于删除测试者数据。不要为了清理而递归删除 HOME、真实 `.claude-science` 或其他用户目录；如需数据清理，先单独确认准确路径和备份策略。

完整支持、安全和证据边界见 [Linux x64 beta 合同](linux-x64-beta.md)。
