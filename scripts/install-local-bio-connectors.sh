#!/bin/zsh
# Register local stdio replacements for the hosted HCLS directory connectors
# that require a real Anthropic session in Claude Science.
#
# What it does, idempotently:
#   1. Adds local-stdio connectors for PubMed, ClinicalTrials.gov, ChEMBL, bioRxiv.
#   2. Attaches those local connectors to the target agent (default: OPERON).
#   3. Detaches the failing hosted bundled connectors from that agent so tool
#      routing does not prefer the remote *.mcp.claude.com entries.
#
# It only talks to the CSSwitch sandbox daemon API and refuses the real
# ~/.claude-science data-dir.

set -euo pipefail

BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
PORT=8990
DATA_DIR="$HOME/.csswitch/sandbox/home/.claude-science"
AGENT="OPERON"

usage() {
  cat <<EOF
Usage: scripts/install-local-bio-connectors.sh [options]

Options:
  --data-dir DIR      Claude Science sandbox data-dir (default: $DATA_DIR)
  --port PORT         Sandbox daemon port (default: $PORT)
  --agent NAME        Agent to attach local connectors to (default: $AGENT)
  --bin PATH          claude-science binary path (default: $BIN)
  -h, --help          Show this help

Run this after CSSwitch has started its Claude Science sandbox.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data-dir) DATA_DIR="$2"; shift 2;;
    --port) PORT="$2"; shift 2;;
    --agent) AGENT="$2"; shift 2;;
    --bin) BIN="$2"; shift 2;;
    -h|--help) usage; exit 0;;
    *) echo "未知参数: $1" >&2; usage >&2; exit 2;;
  esac
done

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "缺少命令: $1" >&2
    exit 127
  }
}

need curl
need python3

[[ "$PORT" =~ ^[0-9]+$ ]] || { echo "拒绝：端口不是整数：$PORT" >&2; exit 2; }
if (( 10#$PORT == 8765 )); then
  echo "拒绝：端口 8765 是真实 Claude Science 实例保留端口。" >&2
  exit 2
fi

DATA_DIR="${DATA_DIR:A}"
REAL_DIR="$HOME/.claude-science"
if [[ "$DATA_DIR" == "${REAL_DIR:A}" ]]; then
  echo "拒绝：data-dir 指向真实 ~/.claude-science。" >&2
  exit 2
fi
if [[ ! -d "$DATA_DIR" ]]; then
  echo "找不到 data-dir：$DATA_DIR" >&2
  echo "请先在 CSSwitch 里启动 Claude Science 沙箱。" >&2
  exit 1
fi
if [[ ! -x "$BIN" ]]; then
  echo "找不到 claude-science 二进制：$BIN" >&2
  exit 1
fi

SANDBOX_HOME="${DATA_DIR:h}"
API_BASE="http://127.0.0.1:$PORT/api"
ORIGIN="http://localhost:$PORT"

if ! curl -fsS --max-time 3 "http://127.0.0.1:$PORT/health" >/dev/null; then
  echo "Claude Science 沙箱未在端口 $PORT 响应。请先启动 CSSwitch。" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/csswitch-local-bio.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

first_http_url() {
  python3 -c 'import re,sys; s=sys.stdin.read(); m=re.search(r"https?://\S+", s); print(m.group(0) if m else "")'
}

echo "获取本地 daemon 会话 cookie（single-use URL 只在本脚本内消费）…"
URL_OUT="$(HOME="$SANDBOX_HOME" "$BIN" url --data-dir "$DATA_DIR")"
URL="$(printf '%s\n' "$URL_OUT" | first_http_url)"
if [[ -z "$URL" ]]; then
  echo "无法从 claude-science url 输出中解析入口 URL：" >&2
  printf '%s\n' "$URL_OUT" >&2
  exit 1
fi

curl -fsS -D "$TMP_DIR/headers.txt" -o "$TMP_DIR/login.html" "$URL"

read AUTH CSRF < <(python3 - "$TMP_DIR/headers.txt" <<'PY'
import re, sys
headers = open(sys.argv[1], encoding="utf-8", errors="replace").read()
def cookie(name):
    m = re.search(rf"(?im)^set-cookie:\s*{re.escape(name)}=([^;]+)", headers)
    return m.group(1) if m else ""
print(cookie("operon_auth"), cookie("operon_csrf"))
PY
)

if [[ -z "$AUTH" || -z "$CSRF" ]]; then
  echo "未取得 operon_auth / operon_csrf cookie；本地登录链接可能已被消费。" >&2
  exit 1
fi

api() {
  local method="$1"
  local api_path="$2"
  local data="${3:-}"
  local -a args
  args=(-fsS --max-time 120 -X "$method"
    -H "Cookie: operon_auth=$AUTH; operon_csrf=$CSRF"
    -H "x-operon-csrf: $CSRF"
    -H "Origin: $ORIGIN"
    -H "Referer: $ORIGIN/"
    -H "Content-Type: application/json")
  if [[ -n "$data" ]]; then
    args+=(--data "$data")
  fi
  command curl "${args[@]}" "$API_BASE$api_path"
}

json_body() {
  local name="$1" pkg="$2" desc="$3" py="$4" run="$5"
  python3 - "$name" "$pkg" "$desc" "$py" "$run" <<'PY'
import json, sys
name, pkg, desc, py, run = sys.argv[1:]
print(json.dumps({
    "name": name,
    "command": py,
    "args": [run, pkg],
    "env": {},
    "description": desc,
}, ensure_ascii=False))
PY
}

urlenc() {
  python3 - "$1" <<'PY'
import sys, urllib.parse
print(urllib.parse.quote(sys.argv[1], safe=""))
PY
}

connector_exists() {
  local name="$1" file="$2"
  python3 - "$name" "$file" <<'PY'
import json, sys
name, file = sys.argv[1:]
items = json.load(open(file))
sys.exit(0 if any(c.get("name") == name for c in items) else 1)
PY
}

RUNTIME_DIR="$(find "$DATA_DIR/runtime" -maxdepth 1 -type d -name '*release' | head -n1)"
PYTHON_BIN="$DATA_DIR/conda/envs/operon-mcp/bin/python"
RUN_SERVER="$RUNTIME_DIR/mcp-servers/bio-tools/run_server.py"

if [[ -z "$RUNTIME_DIR" || ! -f "$RUN_SERVER" ]]; then
  echo "找不到 bio-tools run_server.py；data-dir 可能未完成初始化：$DATA_DIR" >&2
  exit 1
fi
if [[ ! -x "$PYTHON_BIN" ]]; then
  echo "找不到 operon-mcp Python：$PYTHON_BIN" >&2
  exit 1
fi

echo "读取现有 connector 列表…"
api GET "/mcp-servers/connectors" > "$TMP_DIR/connectors-before.json"

typeset -a CONNECTORS
CONNECTORS=(
  "pubmed-local|mcp_pubmed|Local PubMed connector backed by NCBI E-utilities and Europe PMC full text"
  "clinical-trials-local|mcp_clinical_trials|Local ClinicalTrials.gov connector"
  "chembl-local|mcp_chembl|Local ChEMBL connector"
  "biorxiv-local|mcp_biorxiv|Local bioRxiv/medRxiv connector"
)

for row in "${CONNECTORS[@]}"; do
  IFS='|' read -r name pkg desc <<< "$row"
  if connector_exists "$name" "$TMP_DIR/connectors-before.json"; then
    echo "已存在：$name"
  else
    echo "创建本地 connector：$name"
    body="$(json_body "$name" "$pkg" "$desc" "$PYTHON_BIN" "$RUN_SERVER")"
    api POST "/mcp-servers/local" "$body" >/dev/null
  fi

  local_id="local:$name"
  encoded_id="$(urlenc "$local_id")"
  echo "挂载到 agent：$AGENT <- $local_id"
  api POST "/mcp-servers/$encoded_id/attach" "$(python3 - "$AGENT" <<'PY'
import json, sys
print(json.dumps({"agent_names": [sys.argv[1]]}))
PY
)" >/dev/null
done

typeset -a REMOTES
REMOTES=(
  "bundled:pubmed"
  "bundled:clinical-trials"
  "bundled:chembl"
  "bundled:biorxiv"
)

for remote_id in "${REMOTES[@]}"; do
  encoded_id="$(urlenc "$remote_id")"
  echo "从 agent 卸载官方远端 connector（保留目录记录）：$AGENT -/-> $remote_id"
  api DELETE "/mcp-servers/$encoded_id/agents/$AGENT" >/dev/null || true
done

echo
echo "验证 $AGENT 的本地 bio connector 工具清单…"
api GET "/agents/$AGENT/mcp-servers?include_tools=true" > "$TMP_DIR/agent-mcp.json"
python3 - "$TMP_DIR/agent-mcp.json" <<'PY'
import json, sys
items = json.load(open(sys.argv[1]))
wanted = {"pubmed-local", "clinical-trials-local", "chembl-local", "biorxiv-local"}
seen = {}
for item in items:
    name = item.get("name")
    if name in wanted:
        seen[name] = item

missing = sorted(wanted - seen.keys())
if missing:
    print("缺少本地 connector:", ", ".join(missing), file=sys.stderr)
    sys.exit(1)

bad = []
for name in sorted(wanted):
    item = seen[name]
    tools = item.get("tools") or []
    ok = item.get("is_connected") is True and len(tools) > 0
    print(f"{name}: connected={item.get('is_connected')} tools={len(tools)}")
    if not ok:
        bad.append(name)

if bad:
    print("以下 connector 未连接或工具为空:", ", ".join(bad), file=sys.stderr)
    sys.exit(1)
PY

echo
echo "完成。Directory 页面里的官方远端 PubMed/ChEMBL 等仍可能显示 Failed；"
echo "这是虚拟登录下的预期 UI 状态，不影响 $AGENT 使用上面的本地 *-local 工具。"
