"""CSSwitch DSML 兜底 shim：把 DeepSeek 泄漏成纯文本的 DSML 工具调用还原成 tool_use。
纯函数分段器（本文件）+ 流式状态机 + 字节检测器（后续 Task）。不依赖第三方。"""
import codecs
import json
import os
import re

DSML_MARKER_BYTES = (
    "｜DSML｜".encode("utf-8"),
    "｜｜DSML｜｜".encode("utf-8"),
)


def shim_mode(prov_name, prov):
    """off | detect | rewrite。本轮 relay 恒 off（deepseek-only）；deepseek 且 dsml_capable 才读环境变量。"""
    if prov_name == "relay":
        return "off"
    if not (prov or {}).get("dsml_capable"):
        return "off"
    m = os.environ.get("CSSWITCH_TOOLUSE_SHIM", "").lower()
    return m if m in ("detect", "rewrite") else "off"


class DsmlDetector:
    """detect 模式：只判定「本响应是否出现 DSML 泄漏标记」，不改一个字节。
    阶段一遥测用（统计检测发生率，不写盘不改写不宣称修复）。跨 chunk 用小尾缓冲防漏。"""

    _K = max(len(m) for m in DSML_MARKER_BYTES)   # 最长标记的字节数

    def __init__(self):
        self.found = False
        self._tail = b""

    def feed(self, data):
        if self.found or not data:
            return
        buf = self._tail + data
        if any(mk in buf for mk in DSML_MARKER_BYTES):
            self.found = True
            self._tail = b""
            return
        # 只保留末尾可能是「半个标记」的字节，供下个 chunk 拼接判断。
        self._tail = buf[-(self._K - 1):] if len(buf) >= self._K else buf

# 分隔符：一到两个全角竖线 U+FF5C（vLLM 文档单、issue #8 实测双）。
_P = r"[｜]{1,2}"
_WRAP = r"(?:tool_calls|function_calls)"
_OPEN_RE = re.compile(r"<" + _P + r"DSML" + _P + _WRAP + r">")
_TOOLCALLS_RE = re.compile(
    r"<" + _P + r"DSML" + _P + _WRAP + r">(.*?)</" + _P + r"DSML" + _P + _WRAP + r">", re.S)
_INVOKE_RE = re.compile(
    r"<" + _P + r'DSML' + _P + r'invoke\s+name="([^"]+)"\s*>(.*?)</' + _P + r"DSML" + _P + r"invoke>",
    re.S)
_PARAM_RE = re.compile(
    r"<" + _P + r'DSML' + _P + r'parameter\s+name="([^"]+)"(?:\s+string="(true|false)")?\s*>'
    + r"(.*?)</" + _P + r"DSML" + _P + r"parameter>", re.S)


def _coerce_param(pname, string_attr, raw, prop_schema):
    """string="true" → 原始字符串；string="false"/缺 → 按 schema type 转型，失败退 json.loads 再退字符串。"""
    if string_attr == "true":
        return raw
    typ = (prop_schema or {}).get("type")
    if typ == "string":
        return raw
    try:
        if typ == "integer":
            return int(raw)
        if typ == "number":
            return float(raw)
        if typ == "boolean":
            low = raw.strip().lower()
            if low in ("true", "1", "yes"):
                return True
            if low in ("false", "0", "no"):
                return False
            # 不认识的布尔字面量（如 "maybe"）：不臆断为 False，留原字符串，
            # 交 _type_ok 判非法 → _validate_input 返回 False → 整块作废（保守，宁可放行为文本）。
            return raw
        if typ in ("object", "array"):
            return json.loads(raw)
    except (ValueError, TypeError, json.JSONDecodeError):
        pass
    try:
        return json.loads(raw)
    except (ValueError, TypeError, json.JSONDecodeError):
        return raw


def _type_ok(val, typ):
    """基础类型宽松校验：明显冲突才判 False（第三轮 P2）。"""
    if typ in (None, "string"):
        return isinstance(val, str) if typ == "string" else True
    if typ == "integer":
        return (isinstance(val, int) and not isinstance(val, bool)) or \
               (isinstance(val, str) and val.strip().lstrip("+-").isdigit())
    if typ == "number":
        if isinstance(val, bool):
            return False
        if isinstance(val, (int, float)):
            return True
        try:
            float(val)
            return True
        except (ValueError, TypeError):
            return False
    if typ == "boolean":
        return isinstance(val, bool) or (isinstance(val, str)
                and val.strip().lower() in ("true", "false", "1", "0", "yes", "no"))
    if typ == "object":
        return isinstance(val, dict)
    if typ == "array":
        return isinstance(val, list)
    return True


def _validate_input(inp, schema):
    """required 齐 + 各值基础类型相容；不过返回 False（调用方整段按文本放行）。"""
    schema = schema or {}
    for req in schema.get("required") or []:
        if req not in inp:
            return False
    props = schema.get("properties") or {}
    for k, v in inp.items():
        if k in props and not _type_ok(v, props[k].get("type")):
            return False
    return True


def _parse_invoke(name, body, known_tools):
    """解析一个 invoke → {"name","input"}；参数不合 schema 返回 None（调用方整段作废）。"""
    schema = known_tools.get(name) or {}
    schema_props = schema.get("properties") or {}
    inp = {}
    for pn, sattr, raw in _PARAM_RE.findall(body):
        inp[pn] = _coerce_param(pn, sattr, raw, schema_props.get(pn))
    # wrapper 解包：单个名为 arguments/input 的参数、且非工具真实字段 → 解包其对象
    if len(inp) == 1:
        only = next(iter(inp))
        if only in ("arguments", "input") and only not in schema_props:
            val = inp[only]
            if isinstance(val, str):
                try:
                    val = json.loads(val)
                except (ValueError, json.JSONDecodeError):
                    val = None
            if isinstance(val, dict):
                inp = val
    if not _validate_input(inp, schema):
        return None
    return {"name": name, "input": inp}


def parse_dsml_tool_calls(wrapper_region, known_tools):
    """解析 tool_calls 段。任一工具名未声明或参数不合 schema → 返回 []（保守整块）。"""
    known_tools = known_tools or {}
    out = []
    for m in _TOOLCALLS_RE.finditer(wrapper_region):
        invokes = _INVOKE_RE.findall(m.group(1))
        if not invokes:
            return []
        for name, body in invokes:
            if name not in known_tools:
                return []
            call = _parse_invoke(name, body, known_tools)
            if call is None:      # 参数不合 schema → 整块作废
                return []
            out.append(call)
    return out


def segment_dsml_text(text, known_tools):
    """把文本按 DSML tool_calls 段切成有序分段，保留交错。无 DSML → 单 text 分段。"""
    if not text:
        return []
    known_tools = known_tools or {}
    segs = []
    pos = 0
    for m in _TOOLCALLS_RE.finditer(text):
        calls = parse_dsml_tool_calls(m.group(0), known_tools)
        if not calls:
            continue           # 未知工具/坏格式：不切，整段留作文本（下面按文本收）
        if m.start() > pos:
            segs.append({"type": "text", "text": text[pos:m.start()]})
        for c in calls:
            segs.append({"type": "tool_use", "name": c["name"], "input": c["input"]})
        pos = m.end()
    if pos < len(text):
        tail = text[pos:]
        if tail:
            segs.append({"type": "text", "text": tail})
    if not segs:
        return [{"type": "text", "text": text}]
    return segs


class DsmlStreamRewriter:
    """流式 SSE 改写状态机。Task 4：透明重映射（自管下游索引、通用 delta/stop 映射、增量 UTF-8）。
    Task 5 在此基础上加 text_delta 的 DSML 检测与 tool_use 合成。"""

    def __init__(self, known_tools, nonce=""):
        self.known_tools = known_tools or {}
        self.nonce = nonce or "x"
        self._dec = codecs.getincrementaldecoder("utf-8")()
        self._buf = ""            # 已解码、未成帧的文本
        self.next_out = 0
        self.cur_out = None       # 当前打开的下游块索引
        self.cur_type = None      # 当前上游块类型
        self.synthesized = False
        self.tool_n = 0
        # Task 5 用：
        self.state = "PASS"
        self.scan_buf = ""
        self.cap_buf = ""

    # ---- 对外 ----
    def feed(self, data):
        self._buf += self._dec.decode(data)
        return self._drain_frames()

    def finalize(self):
        # 冲掉解码器残留 + 未成帧尾巴 + Task 5 的扣留文本
        self._buf += self._dec.decode(b"", final=True)
        out = self._drain_frames(flush_tail=True)
        out += self._finalize_text()      # Task 5 覆盖；Task 4 为 b""
        return out

    # ---- 帧循环 ----
    def _drain_frames(self, flush_tail=False):
        out = []
        while True:
            i_lf = self._buf.find("\n\n")
            i_crlf = self._buf.find("\r\n\r\n")
            cands = [(i, s) for i, s in ((i_lf, 2), (i_crlf, 4)) if i >= 0]
            if not cands:
                break
            idx, sep = min(cands)
            frame = self._buf[:idx]
            self._buf = self._buf[idx + sep:]
            out.append(self._handle_frame(frame))
        # finalize 时：上游最后一帧若无尾随空行（EOF 突然），也要当作完整帧处理，
        # 否则整条 message_stop / 末尾 delta 会被静默吞掉（Codex P1）。
        if flush_tail and self._buf.strip():
            frame = self._buf
            self._buf = ""
            out.append(self._handle_frame(frame))
        return b"".join(out)

    # ---- 单帧处理 ----
    def _handle_frame(self, frame):
        event, obj = self._parse_frame(frame)
        if obj is None or not isinstance(obj, dict):
            return self._raw(frame)              # 注释/未知/非 JSON：原样
        t = obj.get("type")
        if t == "content_block_start":
            self.cur_type = (obj.get("content_block") or {}).get("type")
            self.cur_out = self.next_out
            self.next_out += 1
            return self._emit("content_block_start",
                              {**obj, "index": self.cur_out})
        if t == "content_block_delta":
            dtype = (obj.get("delta") or {}).get("type")
            if self.cur_type == "text" and dtype == "text_delta":
                return self._on_text_delta(obj.get("delta", {}).get("text", ""))
            return self._emit("content_block_delta", {**obj, "index": self.cur_out})
        if t == "content_block_stop":
            return self._on_block_stop()
        if t == "message_delta":
            return self._flush_pending() + self._on_message_delta(obj)
        if t == "message_stop":
            return self._flush_pending() + self._raw(frame)
        # message_start / ping / 其它：原样
        return self._raw(frame)

    # 最长可能的起始标记字符数（<｜｜DSML｜｜function_calls>），用于 PASS 回抜。
    _MAX_OPEN = len("<｜｜DSML｜｜function_calls>")
    _CAP = 256 * 1024

    def _on_text_delta(self, text):
        out = []
        if self.state == "PASS":
            self.scan_buf += text
            out.append(self._pass_scan())
        else:
            self.cap_buf += text
            out.append(self._capture_scan())
        return b"".join(out)

    def _pass_scan(self):
        out = []
        while True:
            m = _OPEN_RE.search(self.scan_buf)
            if m:
                before = self.scan_buf[:m.start()]
                if before:
                    out.append(self._text_delta(before))
                # 关闭当前 text 块
                if self.cur_out is not None:
                    out.append(self._emit("content_block_stop",
                              {"type": "content_block_stop", "index": self.cur_out}))
                    self.cur_out = None
                self.cap_buf = self.scan_buf[m.start():]   # 含 OPEN，供闭标签匹配
                self.scan_buf = ""
                self.state = "CAPTURE"
                out.append(self._capture_scan())
                return b"".join(out)
            # 未命中：发出安全部分，保留末尾 _MAX_OPEN-1 作可能前缀
            keep = self._MAX_OPEN - 1
            if len(self.scan_buf) > keep:
                emit = self.scan_buf[:-keep]
                self.scan_buf = self.scan_buf[-keep:]
                if emit:
                    out.append(self._text_delta(emit))
            return b"".join(out)

    def _capture_scan(self):
        out = []
        cm = _TOOLCALLS_RE.search(self.cap_buf)
        if cm:
            calls = parse_dsml_tool_calls(cm.group(0), self.known_tools)
            if calls:
                for c in calls:
                    out.append(self._tool_use_events(c))
                self.synthesized = True
            else:
                # 未知工具 / 坏格式：把整段当字面文本
                out.append(self._text_as_new_block(cm.group(0)))
            rest = self.cap_buf[cm.end():]
            self.cap_buf = ""
            self.state = "PASS"
            self.cur_out = None
            if rest:
                # 余料回 PASS 继续扫（可能又有 OPEN 或普通文本）
                self.scan_buf = rest
                out.append(self._pass_scan())
            return b"".join(out)
        # 无闭标签：超上限则保守回退
        if len(self.cap_buf) > self._CAP:
            out.append(self._text_as_new_block(self.cap_buf))
            self.cap_buf = ""
            self.state = "PASS"
            self.cur_out = None
        return b"".join(out)

    def _finalize_text(self):
        # 终审契约：兜底 flush 后必须【关闭】它新开/仍开的 text 块（发 content_block_stop），不能只 flush delta。
        out = []
        if self.state == "CAPTURE" and self.cap_buf:
            out.append(self._text_as_new_block(self.cap_buf))   # 自带开+关
            self.cap_buf = ""
            self.state = "PASS"
        if self.scan_buf:
            out.append(self._text_delta(self.scan_buf))         # 懒开
            self.scan_buf = ""
        if self.cur_out is not None:                            # 关掉仍开的块
            out.append(self._emit("content_block_stop",
                      {"type": "content_block_stop", "index": self.cur_out}))
            self.cur_out = None
        return b"".join(out)

    # ---- 边界 flush（第三轮 P0）：收 stop / message 前先吐扣留文本，杜绝丢字与 index=None ----
    def _on_block_stop(self):
        out = []
        if self.state == "CAPTURE":
            if self.cap_buf:
                out.append(self._text_as_new_block(self.cap_buf))
            self.cap_buf = ""
            self.state = "PASS"
        elif self.scan_buf and self.cur_out is not None:
            # PASS 回抜尾巴：块要关了，直接吐进当前开块（不懒开）
            out.append(self._emit("content_block_delta", {"type": "content_block_delta",
                      "index": self.cur_out, "delta": {"type": "text_delta", "text": self.scan_buf}}))
            self.scan_buf = ""
        if self.cur_out is not None:
            out.append(self._emit("content_block_stop",
                      {"type": "content_block_stop", "index": self.cur_out}))
            self.cur_out = None
        return b"".join(out)

    def _flush_pending(self):
        # message_delta/message_stop 前：吐扣留文本并关块，保证无悬空文本、无跨 message 边界开块。
        out = []
        if self.state == "CAPTURE" and self.cap_buf:
            out.append(self._text_as_new_block(self.cap_buf))
            self.cap_buf = ""
            self.state = "PASS"
        elif self.scan_buf:
            out.append(self._text_delta(self.scan_buf))     # 懒开
            self.scan_buf = ""
        if self.cur_out is not None:                        # 无条件关掉仍开的块，镜像 _on_block_stop
            out.append(self._emit("content_block_stop",
                      {"type": "content_block_stop", "index": self.cur_out}))
            self.cur_out = None
        return b"".join(out)

    # ---- 合成辅助 ----
    def _text_delta(self, text):
        if self.cur_out is None:
            head = self._open_text_block()
        else:
            head = b""
        return head + self._emit("content_block_delta", {"type": "content_block_delta",
                      "index": self.cur_out, "delta": {"type": "text_delta", "text": text}})

    def _open_text_block(self):
        self.cur_out = self.next_out
        self.next_out += 1
        self.cur_type = "text"
        return self._emit("content_block_start", {"type": "content_block_start",
                      "index": self.cur_out, "content_block": {"type": "text", "text": ""}})

    def _text_as_new_block(self, text):
        return self._text_delta(text) + self._emit("content_block_stop",
                      {"type": "content_block_stop", "index": self.cur_out}) + self._close_cur()

    def _close_cur(self):
        self.cur_out = None
        return b""

    def _tool_use_events(self, call):
        idx = self.next_out
        self.next_out += 1
        self.tool_n += 1
        tid = f"toolu_dsml_{self.nonce}_{self.tool_n}"
        start = self._emit("content_block_start", {"type": "content_block_start", "index": idx,
                    "content_block": {"type": "tool_use", "id": tid, "name": call["name"], "input": {}}})
        delta = self._emit("content_block_delta", {"type": "content_block_delta", "index": idx,
                    "delta": {"type": "input_json_delta",
                              "partial_json": json.dumps(call["input"], ensure_ascii=False)}})
        stop = self._emit("content_block_stop", {"type": "content_block_stop", "index": idx})
        return start + delta + stop

    def _on_message_delta(self, obj):
        if self.synthesized:
            d = dict(obj.get("delta") or {})
            if d.get("stop_reason") in ("end_turn", "stop", None):
                d["stop_reason"] = "tool_use"
            obj = {**obj, "delta": d}
        return self._emit("message_delta", obj)

    # ---- 工具 ----
    @staticmethod
    def _parse_frame(frame):
        event, data_lines = None, []
        for line in frame.split("\n"):
            line = line.rstrip("\r")
            if line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data_lines.append(line[5:].lstrip())
        if not data_lines:
            return event, None
        try:
            return event, json.loads("\n".join(data_lines))
        except (ValueError, json.JSONDecodeError):
            return event, None

    @staticmethod
    def _emit(event, obj):
        return f"event: {event}\ndata: {json.dumps(obj, ensure_ascii=False)}\n\n".encode("utf-8")

    @staticmethod
    def _raw(frame):
        return (frame + "\n\n").encode("utf-8")


def rewrite_nonstream_body(body_bytes, known_tools, nonce=""):
    """非流式响应体：把 text content 块里的 DSML 段按分段顺序展开成 text/tool_use 块。保守：坏 JSON 原样返回。"""
    nonce = nonce or "x"
    try:
        obj = json.loads(body_bytes)
    except (ValueError, json.JSONDecodeError):
        return body_bytes
    if not isinstance(obj, dict) or not isinstance(obj.get("content"), list):
        return body_bytes
    new_content = []
    n = 0
    changed = False
    for blk in obj["content"]:
        if isinstance(blk, dict) and blk.get("type") == "text" and isinstance(blk.get("text"), str):
            segs = segment_dsml_text(blk["text"], known_tools)
            if any(s["type"] == "tool_use" for s in segs):
                changed = True
                for s in segs:
                    if s["type"] == "text":
                        new_content.append({"type": "text", "text": s["text"]})
                    else:
                        n += 1
                        new_content.append({"type": "tool_use", "id": f"toolu_dsml_{nonce}_{n}",
                                            "name": s["name"], "input": s["input"]})
                continue
        new_content.append(blk)
    if not changed:
        # 无泄漏：原样返回上游原字节。既保持【逐字】（不 json 往返、不动上游不透明字段
        # 如 thinking.signature），也让上层「字节差 → 判定发生改写」的遥测保持准确
        # （否则任何干净响应都会因 compact↔spaced 再序列化被误报成「已改写」）。
        return body_bytes
    obj["content"] = new_content
    if obj.get("stop_reason") in ("end_turn", "stop", None):
        obj["stop_reason"] = "tool_use"
    return json.dumps(obj, ensure_ascii=False).encode("utf-8")
