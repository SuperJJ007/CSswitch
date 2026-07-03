import os
import sys
import unittest

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

    def test_auth_headers_both(self):
        h = cs._upstream_auth_headers()
        self.assertEqual(h.get("x-api-key"), "cr_testkey")
        self.assertEqual(h.get("Authorization"), "Bearer cr_testkey")

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


if __name__ == "__main__":
    unittest.main()
