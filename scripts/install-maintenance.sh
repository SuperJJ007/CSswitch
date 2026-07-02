#!/bin/bash
# 安装/卸载/查看 CSSwitch 每日维护巡检 launchd agent。
#   scripts/install-maintenance.sh install     # 拷 plist 到 ~/Library/LaunchAgents 并加载
#   scripts/install-maintenance.sh uninstall   # 卸载并删除
#   scripts/install-maintenance.sh status      # 查看是否已加载 + 最近日志
#   scripts/install-maintenance.sh run         # 立刻手动跑一次（走 launchd）
set -euo pipefail

LABEL="com.csswitch.maintenance"
REPO="/Users/superjj/ccproj/CSswitch"
SRC="$REPO/scripts/com.csswitch.maintenance.plist"
DST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

cmd="${1:-status}"
case "$cmd" in
  install)
    mkdir -p "$HOME/Library/LaunchAgents" "$REPO/findings/auto-maint/logs"
    cp "$SRC" "$DST"
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    launchctl bootstrap "$DOMAIN" "$DST"
    launchctl enable "$DOMAIN/$LABEL" 2>/dev/null || true
    echo "已安装并加载：${LABEL}（每天 09:00 / 21:00 Asia/Shanghai）"
    launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E "state|program|run" | head || true
    ;;
  uninstall)
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    rm -f "$DST"
    echo "已卸载并删除：$LABEL"
    ;;
  status)
    if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
      echo "== 已加载 =="
      launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E "state|program arguments|run at load|next fire" -A2 | head -20 || true
    else
      echo "== 未加载 =="
    fi
    echo "== 最近报告 =="
    ls -t "$REPO/findings/auto-maint"/report-*.md 2>/dev/null | head -3 || echo "（还没有报告）"
    echo "== 最近日志 =="
    ls -t "$REPO/findings/auto-maint/logs"/run-*.log 2>/dev/null | head -3 || echo "（还没有日志）"
    ;;
  run)
    launchctl kickstart -k "$DOMAIN/$LABEL"
    echo "已触发一次运行，几秒后看 findings/auto-maint/logs/ 与 report-*.md"
    ;;
  *)
    echo "用法：$0 {install|uninstall|status|run}" >&2
    exit 2
    ;;
esac
