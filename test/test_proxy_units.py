import os
import json
import sys
import unittest
import io

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "proxy"))
import csswitch_proxy as cs


class ToolChoiceMapping(unittest.TestCase):
    def setUp(self):
        cs.PROV = cs.PROVIDERS["qwen"]

    def test_tool_named_maps_to_function(self):
        out = cs.map_tool_choice({"type": "tool", "name": "grade"}, [{"name": "grade"}])
        self.assertEqual(out, {"type": "function", "function": {"name": "grade"}})

    def test_any_single_tool_names_that_function(self):
        out = cs.map_tool_choice({"type": "any"}, [{"name": "only"}])
        self.assertEqual(out, {"type": "function", "function": {"name": "only"}})

    def test_any_multi_tool_falls_back_to_required(self):
        out = cs.map_tool_choice({"type": "any"}, [{"name": "a"}, {"name": "b"}])
        self.assertEqual(out, "required")

    def test_auto_and_none_passthrough(self):
        self.assertEqual(cs.map_tool_choice({"type": "auto"}, []), "auto")
        self.assertEqual(cs.map_tool_choice({"type": "none"}, []), "none")

    def test_missing_returns_none(self):
        self.assertIsNone(cs.map_tool_choice(None, []))

    def test_translation_carries_tool_choice_stop_top_p(self):
        req = {
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "grade", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "grade"},
            "stop_sequences": ["STOP"],
            "top_p": 0.5,
        }
        out = cs.anthropic_to_openai(req)
        self.assertEqual(out["tool_choice"], {"type": "function", "function": {"name": "grade"}})
        self.assertEqual(out["stop"], ["STOP"])
        self.assertEqual(out["top_p"], 0.5)


class MaxTokensPerModel(unittest.TestCase):
    def setUp(self):
        cs.PROV = cs.PROVIDERS["deepseek"]

    def test_cap_uses_target_model_entry(self):
        self.assertEqual(cs.clamp_max_tokens(100000, "deepseek-v4-pro"), 65536)
        self.assertEqual(cs.clamp_max_tokens(100000, "deepseek-v4-flash"), 32768)

    def test_unknown_model_uses_default_cap(self):
        self.assertEqual(cs.clamp_max_tokens(100000, "who-knows"), 8192)

    def test_not_clamped_below_request(self):
        self.assertEqual(cs.clamp_max_tokens(500, "deepseek-v4-pro"), 500)

    def test_none_passthrough(self):
        self.assertIsNone(cs.clamp_max_tokens(None, "deepseek-v4-pro"))

    def test_qwen_per_model(self):
        cs.PROV = cs.PROVIDERS["qwen"]
        self.assertEqual(cs.clamp_max_tokens(100000, "qwen-max"), 8192)


class RelayProvider(unittest.TestCase):
    """中转站 provider：透传（不重映射）+ 贴合 + 双鉴权头 + 不夹 max_tokens + /v1/models 归一化。"""

    def setUp(self):
        cs.PROV = dict(cs.PROVIDERS["relay"])
        cs.PROV["url"] = "https://relay.test/claude/v1/messages"
        cs.PROV["models_url"] = "https://relay.test/claude/v1/models"
        cs.KEY = "cr_testkey"
        cs.RELAY_MODELS = []
        cs.RELAY_FORCE_MODEL = None

    def test_passthrough_keeps_model_name(self):
        # 中转站原生认 claude-*，模型名原样透传（不走 model_map）。
        self.assertEqual(cs.resolve_model("claude-opus-4-8"), "claude-opus-4-8")
        self.assertEqual(cs.resolve_model("claude-sonnet-4-6"), "claude-sonnet-4-6")
        self.assertEqual(cs.resolve_model("some-other-model"), "some-other-model")

    def test_empty_model_falls_back_to_default(self):
        self.assertEqual(cs.resolve_model(""), "claude-opus-4-8")

    def test_snaps_bare_id_to_upstream_dated_id(self):
        cs.RELAY_MODELS = ["claude-haiku-4-5-20251001", "claude-opus-4-8"]
        # 裸 id（如标题 agent 的）贴合到中转站带日期的真实 id。
        self.assertEqual(cs.resolve_model("claude-haiku-4-5"), "claude-haiku-4-5-20251001")
        # 精确命中不改。
        self.assertEqual(cs.resolve_model("claude-opus-4-8"), "claude-opus-4-8")
        # 缓存里没有的原样透传（交中转站处理别名/报错）。
        self.assertEqual(cs.resolve_model("claude-sonnet-5"), "claude-sonnet-5")

    def test_force_model_overrides_everything(self):
        # 选了模型：无论 Science 发什么（含裸 claude-*、空名），都强制成选中的上游模型。
        cs.RELAY_FORCE_MODEL = "mimo-v2.5-pro"
        try:
            self.assertEqual(cs.resolve_model("claude-opus-4-8"), "mimo-v2.5-pro")
            self.assertEqual(cs.resolve_model("claude-haiku-4-5"), "mimo-v2.5-pro")
            self.assertEqual(cs.resolve_model(""), "mimo-v2.5-pro")
        finally:
            cs.RELAY_FORCE_MODEL = None

    def test_no_force_model_keeps_passthrough(self):
        # 留空：维持 PR #4 透传（贴合到真实 id / 原样）。
        cs.RELAY_FORCE_MODEL = None
        self.assertEqual(cs.resolve_model("claude-opus-4-8"), "claude-opus-4-8")
        self.assertEqual(cs.resolve_model(""), "claude-opus-4-8")  # 空名兜底 default_model

    def test_auth_headers_both(self):
        h = cs._upstream_auth_headers()
        self.assertEqual(h.get("x-api-key"), "cr_testkey")
        self.assertEqual(h.get("Authorization"), "Bearer cr_testkey")

    def test_non_claude_force_model_uses_openai_fallback(self):
        cs.PROV_NAME = "relay"
        cs.RELAY_FORCE_MODEL = "gpt5.5"
        cs.RELAY_OPENAI_URL = "https://relay.test/v1/chat/completions"
        calls = []
        orig_post = cs.http_post
        try:
            def fake_post(url, data, headers):
                calls.append((url, json.loads(data), headers))
                return json.dumps({
                    "id": "chatcmpl_mock",
                    "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                }).encode(), "application/json"

            cs.http_post = fake_post
            handler = object.__new__(cs.H)
            sent = []
            handler._send_json = lambda code, obj: sent.append((code, obj))
            handler._handle_anthropic({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}],
            })
        finally:
            cs.http_post = orig_post
            cs.RELAY_FORCE_MODEL = None
            cs.RELAY_OPENAI_URL = None
            cs.PROV_NAME = None

        self.assertEqual(calls[0][0], "https://relay.test/v1/chat/completions")
        self.assertEqual(calls[0][1]["model"], "gpt5.5")
        self.assertEqual(calls[0][2]["x-api-key"], "cr_testkey")
        self.assertEqual(calls[0][2]["Authorization"], "Bearer cr_testkey")
        self.assertEqual(sent[0][0], 200)
        self.assertEqual(sent[0][1]["content"][0]["text"], "ok")

    def test_openai_stream_fallback_replays_anthropic_sse(self):
        cs.PROV_NAME = "relay"
        cs.RELAY_FORCE_MODEL = "gpt5.5"
        cs.RELAY_OPENAI_URL = "https://relay.test/v1/chat/completions"
        frames = [
            {"id": "chatcmpl_s", "choices": [{"delta": {"role": "assistant"}, "finish_reason": None}]},
            {"id": "chatcmpl_s", "choices": [{"delta": {"content": "ok"}, "finish_reason": None}]},
            {"id": "chatcmpl_s", "choices": [{"delta": {}, "finish_reason": "stop"}]},
        ]
        sse = "".join("data: " + json.dumps(x) + "\n\n" for x in frames) + "data: [DONE]\n\n"
        orig_stream = cs.open_stream
        try:
            class FakeResp:
                def __iter__(self):
                    return iter([sse.encode()])
                def __enter__(self):
                    return self
                def __exit__(self, *args):
                    return False

            def fake_stream(url, data, headers):
                return FakeResp(), b"", "text/event-stream"

            cs.open_stream = fake_stream
            handler = object.__new__(cs.H)
            out = io.BytesIO()
            handler.wfile = out
            handler.send_response = lambda code: None
            handler.send_header = lambda k, v: None
            handler.end_headers = lambda: None
            handler._handle_anthropic({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            })
        finally:
            cs.open_stream = orig_stream
            cs.RELAY_FORCE_MODEL = None
            cs.RELAY_OPENAI_URL = None
            cs.PROV_NAME = None

        body = out.getvalue().decode("utf-8", "replace")
        self.assertIn("event: message_start", body)
        self.assertIn("text_delta", body)
        self.assertIn("ok", body)
        self.assertIn("event: message_stop", body)

    def test_deepseek_auth_headers_default_xapikey(self):
        # 回归：deepseek 未设 auth_style → 仍只带 x-api-key（不误引入 Bearer）。
        cs.PROV = cs.PROVIDERS["deepseek"]
        cs.KEY = "sk-ds"
        self.assertEqual(cs._upstream_auth_headers(), {"x-api-key": "sk-ds"})

    def test_no_max_tokens_clamp(self):
        # relay default_cap=None、model_caps 空 → 尊重中转站真实上限，不夹取。
        self.assertEqual(cs.clamp_max_tokens(1000000, "claude-opus-4-8"), 1000000)

    def test_models_normalized_and_cache_refreshed(self):
        # 用假 http_get_json 复刻中转站 OpenAI 风格返回，验证归一化成 Anthropic 格式
        # + RELAY_MODELS 缓存刷新（离线，不触网）。
        orig = cs.http_get_json
        cs.http_get_json = lambda url, headers: {
            "data": [
                {"id": "claude-opus-4-8", "object": "model"},
                {"id": "claude-3-5-sonnet-20241022", "object": "model"},
                {"no_id": True},  # 缺 id 的条目应被跳过
            ]
        }
        try:
            out = cs.fetch_relay_models()
        finally:
            cs.http_get_json = orig
        self.assertEqual([m["id"] for m in out], ["claude-opus-4-8", "claude-3-5-sonnet-20241022"])
        self.assertTrue(all(m["type"] == "model" for m in out))
        self.assertEqual(out[0]["display_name"], "claude-opus-4-8")
        self.assertEqual(cs.RELAY_MODELS, ["claude-opus-4-8", "claude-3-5-sonnet-20241022"])

    def test_models_carry_supports_tools_from_supported_parameters(self):
        orig = cs.http_get_json
        cs.http_get_json = lambda url, headers: {"data": [
            {"id": "glm-4.6", "supported_parameters": ["tools", "temperature"]},  # 含 tools → True
            {"id": "glm-lite", "supported_parameters": ["temperature"]},          # 有字段无 tools → False
            {"id": "glm-x"},                                                        # 无字段 → None（不臆测）
        ]}
        try:
            out = cs.fetch_relay_models()
        finally:
            cs.http_get_json = orig
        got = {m["id"]: m["supports_tools"] for m in out}
        self.assertEqual(got, {"glm-4.6": True, "glm-lite": False, "glm-x": None})

    def test_build_models_response_success(self):
        orig = cs.http_get_json
        cs.http_get_json = lambda url, headers: {"data": [{"id": "glm-4.6"}]}
        try:
            code, body = cs.build_models_response()
        finally:
            cs.http_get_json = orig
        self.assertEqual(code, 200)
        self.assertEqual([m["id"] for m in body["data"]], ["glm-4.6"])

    def test_build_models_response_preserves_upstream_status(self):
        import urllib.error, io
        def boom(url, headers):
            raise urllib.error.HTTPError(url, 401, "Unauthorized", {}, io.BytesIO(b"nope"))
        orig = cs.http_get_json
        cs.http_get_json = boom
        try:
            code, body = cs.build_models_response()
        finally:
            cs.http_get_json = orig
        self.assertEqual(code, 401)                    # 不再吞成 200 + 静态
        self.assertEqual(body["upstream_status"], 401)
        self.assertNotIn("data", body)                 # 失败不回静态列表

    def test_build_models_response_network_error_502(self):
        def boom(url, headers):
            raise ConnectionError("dns fail")
        orig = cs.http_get_json
        cs.http_get_json = boom
        try:
            code, body = cs.build_models_response()
        finally:
            cs.http_get_json = orig
        self.assertEqual(code, 502)
        self.assertEqual(body["error_kind"], "network")

    def test_build_models_response_non_relay_returns_static(self):
        # deepseek/qwen（无 models_url）→ 仍回静态选择器列表，行为不变。
        cs.PROV = cs.PROVIDERS["deepseek"]
        code, body = cs.build_models_response()
        self.assertEqual(code, 200)
        ids = [m["id"] for m in body["data"]]
        self.assertIn("claude-opus-4-8", ids)


class ThinkingNormalization(unittest.TestCase):
    """thinking 归一化的 provider gate（spec v3 §3.1，拆两条独立处理）。

    代理层只有 deepseek / relay 进入 _handle_anthropic；glm/xiaomi/硅基/openrouter/kimi/minimax
    在代理层均为 provider=relay（靠 base_url 区分），故此处对 relay 的断言即覆盖 §3.5 test 2 的
    「4 家 relay 回归门禁」。事实依据：MiniMax 官方 Anthropic 端点认 adaptive/disabled、不认 auto，
    故 auto→adaptive 对 relay 保留；forced→disabled 是 DeepSeek flash 特有，只 gate 到 deepseek。
    """

    # (A) 强制 tool_choice(any/tool) → disabled：仅 deepseek
    def test_deepseek_forced_tool_choice_disables_thinking(self):
        body = {"tool_choice": {"type": "any"}, "thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "deepseek")
        self.assertEqual(out["thinking"], {"type": "disabled"})

    def test_relay_forced_tool_choice_not_disabled(self):
        # relay 不被强注 disabled（各中转站上游自理）；auto 仍归一到 adaptive。
        body = {"tool_choice": {"type": "tool", "name": "x"}, "thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "relay")
        self.assertNotEqual(out["thinking"].get("type"), "disabled")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_relay_forced_without_thinking_not_injected(self):
        # relay + 强制工具 + 无 thinking → 不注入 thinking（回归：不再强注 disabled）。
        body = {"tool_choice": {"type": "any"}}
        out = cs.normalize_thinking(body, "relay")
        self.assertNotIn("thinking", out)

    # (B) auto → adaptive：deepseek 与 relay 都做
    def test_deepseek_auto_becomes_adaptive(self):
        body = {"thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "deepseek")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_relay_auto_becomes_adaptive(self):
        body = {"thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "relay")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    # 回归：非 auto 的 thinking 一律原样保留（不臆改）
    def test_relay_non_auto_thinking_preserved(self):
        body = {"thinking": {"type": "enabled", "budget_tokens": 1024}}
        out = cs.normalize_thinking(body, "relay")
        self.assertEqual(out["thinking"], {"type": "enabled", "budget_tokens": 1024})

    def test_noop_when_no_thinking_and_no_forcing(self):
        body = {"messages": []}
        out = cs.normalize_thinking(body, "relay")
        self.assertNotIn("thinking", out)

    # relay thinking 策略 "enabled"（如 Kimi：模型强制 thinking.type=enabled，真机 §3.5 验证）
    def test_relay_enabled_policy_auto_becomes_enabled(self):
        body = {"thinking": {"type": "auto"}, "max_tokens": 2048}
        out = cs.normalize_thinking(body, "relay", "enabled")
        self.assertEqual(out["thinking"]["type"], "enabled")
        self.assertGreater(out["thinking"]["budget_tokens"], 0)
        self.assertLess(out["thinking"]["budget_tokens"], 2048)

    def test_relay_enabled_policy_injects_when_missing(self):
        # Kimi 连「缺 thinking」都 400，故强制注入 enabled。
        body = {"max_tokens": 2048}
        out = cs.normalize_thinking(body, "relay", "enabled")
        self.assertEqual(out["thinking"]["type"], "enabled")

    def test_relay_enabled_policy_preserves_existing_enabled(self):
        body = {"thinking": {"type": "enabled", "budget_tokens": 512}, "max_tokens": 2048}
        out = cs.normalize_thinking(body, "relay", "enabled")
        self.assertEqual(out["thinking"], {"type": "enabled", "budget_tokens": 512})

    def test_relay_enabled_budget_stays_below_max_tokens(self):
        body = {"thinking": {"type": "auto"}, "max_tokens": 100}
        out = cs.normalize_thinking(body, "relay", "enabled")
        self.assertLess(out["thinking"]["budget_tokens"], 100)
        self.assertGreaterEqual(out["thinking"]["budget_tokens"], 1)

    def test_relay_adaptive_policy_still_auto_to_adaptive(self):
        # 默认策略（adaptive，如 MiniMax）不变。
        body = {"thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "relay", "adaptive")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_deepseek_unaffected_by_relay_thinking_arg(self):
        # relay_thinking 只对 relay 生效；deepseek 行为不因该参数改变。
        body = {"tool_choice": {"type": "any"}, "thinking": {"type": "auto"}}
        out = cs.normalize_thinking(body, "deepseek", "enabled")
        self.assertEqual(out["thinking"], {"type": "disabled"})


class BuildModelsResponse(unittest.TestCase):
    def setUp(self):
        self._saved = (cs.PROV, cs.PROV_NAME, cs.RELAY_FORCE_MODEL)

    def tearDown(self):
        cs.PROV, cs.PROV_NAME, cs.RELAY_FORCE_MODEL = self._saved

    def test_force_returns_single_shell(self):
        # force（Science 常驻代理）：/v1/models 只返回一个壳，display_name=真实模型名。
        cs.PROV = {"models_url": "https://r/v1/models"}
        cs.PROV_NAME = "relay"
        cs.RELAY_FORCE_MODEL = "glm-5.2"
        code, body = cs.build_models_response()
        self.assertEqual(code, 200)
        self.assertEqual(
            body["data"],
            [{
                "type": "model", "id": "claude-opus-4-8",
                "display_name": "glm-5.2", "supports_tools": None,
                "created_at": "2026-01-01T00:00:00Z",
            }],
        )
        self.assertEqual(body["first_id"], "claude-opus-4-8")
        self.assertEqual(body["last_id"], "claude-opus-4-8")
        self.assertFalse(body["has_more"])

    def test_not_forced_falls_back_to_source(self):
        # 未 force（app 获取模型的临时代理）：仍回源拿真实 id 供用户选（两个消费者切分）。
        cs.PROV = {"models_url": "https://r/v1/models"}
        cs.PROV_NAME = "relay"
        cs.RELAY_FORCE_MODEL = None
        saved_fetch = cs.fetch_relay_models
        cs.fetch_relay_models = lambda: [{"type": "model", "id": "glm-4.6"}]
        try:
            code, body = cs.build_models_response()
        finally:
            cs.fetch_relay_models = saved_fetch
        self.assertEqual(code, 200)
        self.assertEqual(body["data"][0]["id"], "glm-4.6")


if __name__ == "__main__":
    unittest.main()
