#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: linux_x64_package_smoke.sh <installed-bin> <temporary-home>" >&2
  exit 2
fi

APP_BIN="$1"
SMOKE_HOME="$2"
FIRST_PID=""

cleanup() {
  if [[ -n "$FIRST_PID" ]] && kill -0 "$FIRST_PID" 2>/dev/null; then
    kill -TERM "$FIRST_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

test -x "$APP_BIN"
mkdir -p "$SMOKE_HOME"
chmod 700 "$SMOKE_HOME"

HOME="$SMOKE_HOME" "$APP_BIN" &
FIRST_PID=$!

attempt=0
while [[ $attempt -lt 20 ]]; do
  kill -0 "$FIRST_PID" 2>/dev/null || {
    echo "first CSSwitch instance exited before initialization" >&2
    exit 1
  }
  attempt=$((attempt + 1))
  sleep 0.1
done

test ! -e "$SMOKE_HOME/.claude-science"

set +e
timeout --signal=TERM 5s env HOME="$SMOKE_HOME" "$APP_BIN"
SECOND_RC=$?
set -e
if [[ $SECOND_RC -ne 0 ]]; then
  echo "second CSSwitch instance did not exit cleanly: rc=$SECOND_RC" >&2
  exit 1
fi
kill -0 "$FIRST_PID"

kill -TERM "$FIRST_PID"
if ! timeout 10s tail --pid="$FIRST_PID" -f /dev/null; then
  echo "first CSSwitch instance did not terminate within 10 seconds" >&2
  exit 1
fi
wait "$FIRST_PID" 2>/dev/null || true
FIRST_PID=""

test ! -e "$SMOKE_HOME/.claude-science"
