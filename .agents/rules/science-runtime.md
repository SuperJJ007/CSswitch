# Science runtime 规则

- Science executable、持久 data-dir、版本 runtime 资源、组织数据和监听进程是不同事实。
- 新启动通常使用用户当前安装的官方 Claude Science executable，并复用 CSSwitch 隔离 data-dir；macOS 使用固定 App 路径，Linux 使用外层真实 `$HOME/.local/bin/claude-science`。
- `SCIENCE_BIN` 仅是显式开发 override；无效时 fail closed。历史缓存绝不能隐式回退。
- 不从真实 `~/.claude-science` 复制 runtime 资产，不下载或升级 Science；保持 `--no-auto-update`，除非产品合同另行批准。
- Linux 必须先确认 x86_64、Bubblewrap 0.8.0+、可用非特权 user namespace、socat 与 lsof；任一 blocker 都 fail closed，不提供 no-sandbox 降级。
- Linux 的所有 Science 子命令必须清空宿主环境，共用隔离 HOME/data-dir/XDG/TMPDIR，目录权限 `0700`，只转交必要 locale、loopback proxy 与经验证的显式授权变量；不得继承 provider/API、Git/SSH 或默认 SSH agent 环境。
- 不从不受控 PATH 猜测 Science、Bash 或系统工具；runtime 脚本固定 `/bin/bash`，平台 opener、lsof、ps、id、kill 使用代码中固定的受信任绝对路径。
- Linux Science UI 严格 browser-first；exact nonce URL 只能在后端内存中交给固定 `/usr/bin/xdg-open`。前端仅可见脱敏 origin 和重试动作，不得获得、复制或持久化 exact URL。
- Science 与 CSSwitch Gateway 均绑定 loopback；引入或暗示 `0.0.0.0` 需要单独的安全和产品决策。
- 端口占用或 `status` 成功不能单独证明 runtime 身份；需结合 executable、data-dir、监听 PID 和受管启动身份。
- 已健康 daemon 不因版本探测或可选功能漂移而强制重启。
- 外部 Skill route / connector 配置失败只降级该可选功能，不阻断普通 Science 启动。
- 系统 SSH 默认关闭；一旦用户启用，真实 config 与 packaged wrapper 的安全校验属于 fail-closed 启动条件，不能当作 warning 略过。
