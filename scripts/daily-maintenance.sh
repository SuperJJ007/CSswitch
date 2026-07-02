#!/bin/bash
# CSSwitch 每日维护巡检 —— 由 launchd 每天 09:00 / 21:00 触发（Asia/Shanghai）。
# 无人值守地跑一个受限 `claude -p` session：只读仓库 + 抓公开网页 + 把巡检报告写进
# findings/auto-maint/。绝不改代码、不提交、不推送、不碰 ~/.claude-science、不启动任何 Science。
# 详见 scripts/daily-maintenance.prompt.md。
#
# 手动测试：bash scripts/daily-maintenance.sh
# 安装/卸载定时：scripts/install-maintenance.sh {install|uninstall|status}
set -euo pipefail

REPO="/Users/superjj/ccproj/CSswitch"
PROMPT_FILE="$REPO/scripts/daily-maintenance.prompt.md"
OUT_DIR="$REPO/findings/auto-maint"
LOG_DIR="$OUT_DIR/logs"
LOCK="$OUT_DIR/.run.lock"

# launchd 给的环境极简，自己补 PATH（claude 在 ~/.local/bin）
export PATH="/Users/superjj/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export HOME="/Users/superjj"

mkdir -p "$LOG_DIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
LOG="$LOG_DIR/run-$STAMP.log"

# 防止两次运行叠在一起（比如手动测试撞上定时）
if ! mkdir "$LOCK" 2>/dev/null; then
  echo "[$STAMP] 已有一次巡检在跑（$LOCK 存在），本次跳过。" >>"$LOG"
  exit 0
fi
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

cd "$REPO"

CLAUDE_BIN="$(command -v claude || echo /Users/superjj/.local/bin/claude)"
PROMPT="$(cat "$PROMPT_FILE")"

{
  echo "===== CSSwitch daily-maintenance @ $STAMP ====="
  echo "repo=$REPO branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?') claude=$CLAUDE_BIN"
  echo "-----------------------------------------------"
} >>"$LOG"

# 超时保护：无人值守时若卡在权限弹窗等，最多跑 TIMEOUT 秒后强杀，避免永久挂起。
TIMEOUT="${MAINT_TIMEOUT:-900}"
run_with_timeout() { perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$@"; }

# 受限无头运行：
#  - acceptEdits：允许写文件（报告落盘）不弹窗
#  - allowedTools：只放行只读命令 + WebFetch/WebSearch + 写文件；不放行任意 Bash
#  - disallowedTools：硬禁危险 git 操作、rm，以及对 ~/.claude-science 的读写（deny 优先级最高）
set +e
run_with_timeout "$TIMEOUT" "$CLAUDE_BIN" -p "$PROMPT" \
  --permission-mode acceptEdits \
  --allowedTools \
    "Read Grep Glob Write Edit WebFetch WebSearch \
     Bash(git status:*) Bash(git log:*) Bash(git branch:*) Bash(git diff:*) Bash(git rev-parse:*) \
     Bash(plutil:*) Bash(date:*) Bash(ls:*) Bash(mkdir:*) Bash(grep:*) Bash(cat:*) \
     Bash(head:*) Bash(tail:*) Bash(wc:*) Bash(find:*) Bash(sed:*)" \
  --disallowedTools \
    "Bash(rm:*) Bash(git commit:*) Bash(git push:*) Bash(git add:*) Bash(git checkout:*) \
     Bash(git switch:*) Bash(git reset:*) Bash(git stash:*) Bash(git branch -D:*) \
     Read(/Users/superjj/.claude-science/**) Write(/Users/superjj/.claude-science/**) Edit(/Users/superjj/.claude-science/**)" \
  >>"$LOG" 2>&1
RC=$?
set -e

echo "----- claude exit=$RC @ $(date +%Y%m%d-%H%M%S) -----" >>"$LOG"
exit "$RC"
