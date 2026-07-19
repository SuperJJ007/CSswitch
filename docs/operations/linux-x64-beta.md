# Linux x64 beta

状态：`080-linux-beta` 有限社区内测合同，技术版本 `v0.8.0-linux-beta.1`。本文说明目标和验收方法，不表示 Ubuntu 真机、真实 Claude Science 或 live Codex 已通过。

## 支持范围

首期只支持 Ubuntu 24.04 x86_64、glibc、原生桌面会话。交付物是 GitHub Actions 生成的单一内部 amd64 `.deb` 与 SHA-256，可发送给明确知晓实验边界的有限测试群体；不创建 tag，不发布 GitHub Release。测试者操作见[Linux 内测指南](linux-x64-beta-testing.md)。

包含 API-key providers、Codex browser OAuth、Gateway、动态/静态模型目录、外部 Skill/MCP、system SSH opt-in，以及由 CSSwitch 隔离 HOME/data-dir 托管的 Science 生命周期。Linux 只提供第三方隔离模式，不显示也不接受 Official Claude。

不包含 WSL/WSLg、ARM64、musl/Alpine、systemd、AppImage、rpm、远程监听、`0.0.0.0` 或 no-sandbox 降级。

## 系统前置

Claude Science 官方要求 Linux x64 glibc、约 5 GB 空间、Bubblewrap 0.8.0+、socat 和可用的非特权 user namespace。CSSwitch 另外需要 lsof 做监听进程强身份核对，并使用 xdg-utils 打开浏览器。

```bash
sudo apt-get update
sudo apt-get install -y bubblewrap socat lsof xdg-utils openssh-client
bwrap --version
```

Ubuntu 24.04 某些镜像会由 kernel 设置或 AppArmor 阻止 user namespace。CSSwitch 会实际运行一个最小 Bubblewrap probe；失败时返回 `environment_blocked` / `userns_unavailable`，不会改用无沙箱模式。应按 Ubuntu/Claude Science 给出的系统级错误修复镜像策略，而不是给 CSSwitch 增加旁路。

Claude Science 由用户按[官方 Linux 安装说明](https://claude.com/docs/claude-science/get-started)安装，默认 executable 是 `$HOME/.local/bin/claude-science`。CSSwitch 不运行官方安装脚本，不下载或升级 Science，也不从 PATH 猜测替代 binary。

## Runtime 合同

Science binary 选择顺序为：

1. 显式绝对 `SCIENCE_BIN`；无效时 fail closed；
2. `$HOME/.local/bin/claude-science`；
3. 版本可确认且由用户仅本次授权的 `cached_once`。

所有 Science `--version`、`status`、`url`、`serve`、`stop` 命令共用 `$HOME/.csswitch/sandbox/home` 这个隔离 HOME 和其下同一个 `.claude-science` data-dir。Linux 启动时清空宿主环境，再将 HOME、XDG config/data/cache/state/runtime 与 TMPDIR 全部指向权限 `0700` 的隔离子目录；PATH 固定为受信任系统目录。宿主的 provider/API 变量、Git/SSH 变量和 SSH agent 默认不转交。不得读取、复制或修复用户真实 `$HOME/.claude-science`。Gateway 与 Science UI/preview 均只监听 `127.0.0.1`。

Linux 严格采用 browser-first，固定调用 `/usr/bin/xdg-open`，开发用 WebView spike 环境变量在 Linux 编译路径无效。后端只接受 `http://127.0.0.1:<当前端口>` 或 `http://localhost:<当前端口>`，拒绝远程 host、错端口、userinfo、fragment、控制字符和超长 URL。exact nonce URL 只在后端内存中交给 opener，不返回前端、不进日志、不写 clipboard。opener 失败不停止已健康服务；UI 只显示 `http://127.0.0.1:<port>/…`，并提供“再次打开”，由后端重新获取一条新 URL。

system SSH 仍默认关闭。启用后只把无符号链接的真实 `~/.ssh/config` 交给 packaged wrapper；仅当宿主 `SSH_AUTH_SOCK` 是存在的绝对 Unix socket 时才转交 agent。无效 config 或 agent socket 均 fail closed。

Linux beta 的目录初始化会拒绝静态 leaf/parent symlink，并将隔离 HOME、XDG 与 TMPDIR 目标目录收紧为 `0700`。当前实现仍使用路径式检查/创建，不宣称闭合“另一个同 UID 进程恰在检查与使用之间替换路径组件”的 TOCTOU 竞态；这是首个 beta 的已知 P2 residual。若后续威胁模型要求抵御已能以同一用户运行并并发改写 CSSwitch 隔离 HOME 的进程，应在 stable 前改为 fd-anchored `openat`/`mkdirat`/`O_NOFOLLOW` walk，并重新做独立安全复审。

Codex OAuth 使用 CSSwitch data root 下的私有文件，不引入 Secret Service/libsecret。父目录和文件分别保持 `0700/0600`，拒绝 symlink，原子提交并校验 generation/CAS。callback 只绑定 `127.0.0.1:1455` / `1457`。sidecar 清空环境后只转交固定 HOME、网络路由和 `DISPLAY`、`WAYLAND_DISPLAY`、`XDG_RUNTIME_DIR`、`DBUS_SESSION_BUS_ADDRESS`。

## 内部 `.deb`

工作流位于 [`.github/workflows/linux-x64-internal.yml`](../../.github/workflows/linux-x64-internal.yml)，使用标准 `ubuntu-24.04` hosted runner：

1. 安装 [Tauri 2 Linux 构建依赖](https://v2.tauri.app/start/prerequisites/)及 runtime 前置；
2. `npm ci`；
3. `bash test/run_all.sh --require-release-ready`；
4. `npm run tauri build -- --bundles deb`；
5. 核对 amd64 元数据与依赖，从 Desktop Entry 精确定位 Desktop，唯一定位 Gateway，并检查二者均为可执行 x86-64 ELF；
6. 核对 doctor/launch/stop/verify/SSH wrapper 与图标，安装 `.deb`，在临时 HOME 验证 Gateway 脱敏空状态，并在 Xvfb 下做进程启动、单实例和信号终止 smoke；
7. 上传 `.deb` 与 SHA-256，保留 14 天。

CI 不下载真实 Science，不使用真实 Claude Science、Codex、Provider 或用户凭证，也不执行 live provider。Xvfb smoke 不证明 GUI 可见、显式退出或完整生命周期清理。Actions 仅由 `codex/v090-linux-x64` 的窄范围 push、PR 或手动触发产生 artifact；本地存在 workflow 文件不等于 CI 已通过。

## Ubuntu 真机验收

必须使用临时 Ubuntu 24.04 x86_64 云 VM、独立非特权用户和可观察的 X11/Wayland 桌面。开始前记录 VM image、kernel、desktop session、`.deb` SHA-256、源码 commit 与 Claude Science `--version`。

验收分层记录：

| 层 | 最低判据 |
|---|---|
| Artifact | `.deb` 为 amd64；Desktop、Gateway、脚本、图标齐全；SHA-256 与 Actions 一致 |
| 安装态 | GUI 启动、单实例、退出、卸载；X11/Wayland、中文输入、`xdg-open` 分开记录 |
| 隔离 | 真实 `.claude-science` sentinel 字节与元数据不变；CSSwitch 只写自己的隔离 HOME/data-dir |
| Mock runtime | Gateway/Science 双 loopback，主端口/preview 端口，一键启动、模型目录、最小文本、Skill/MCP、SSH 默认关闭与 opt-in |
| 生命周期 | stop、明确退出、重开、模拟 Gateway/Science crash、journal recovery、身份不符 fail closed、无归属残留进程 |
| 真实 Science | 使用当前 stable Linux executable 完成 start/url/browser/stop；记录版本和脱敏状态，不记录 nonce 或真实数据 |
| Codex live | 用户明确授权测试账号后完成一次浏览器 OAuth、动态目录和最小推理；只记录 authenticated、expiry bucket、account hash 等脱敏状态 |

在真实外层 HOME 的 `.claude-science` 放置不可修改 sentinel 时，只能通过事前/事后 hash、权限和 stat 摘要证明未改；不得为了“确认没读取”而打开真实凭证文件。更完整的通用护栏见[真机验收](real-machine-acceptance.md)。

## 通过标准

只有 macOS 原门禁、Linux Actions、内部 artifact 检查、Ubuntu 安装态、真实 Science 和用户授权的 Codex live 层分别完成，才可把对应项目标为通过。未运行 Actions 时写“workflow 未执行”；没有 Ubuntu VM 时写 `NEEDS-REAL-MACHINE`；没有用户 OAuth 授权时写“未执行”，不能由 mock 推断。

## 后续

原生 Linux 内测通过后才规划 WSL2 browser-first。WSL 数据默认留在 Linux 文件系统，不复制 Windows 凭证，不默认访问 `/mnt/c`；普通 GitHub Windows hosted runner 不能作为 WSL2 runtime 通过证据。
