import os
import sys
import json
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


class ResponsesMapping(unittest.TestCase):
    def setUp(self):
        cs.PROV = dict(cs.PROVIDERS["openai-responses"])
        cs.PROV_NAME = "openai-responses"
        cs.PROV["default_model"] = "gpt-5.2"
        cs.KEY = "sk-openai"
        cs.RELAY_FORCE_MODEL = "gpt-5.2"

    def tearDown(self):
        cs.RELAY_FORCE_MODEL = None

    def test_responses_request_maps_prompt_tools_and_limits(self):
        req = {
            "model": "claude-opus-4-8",
            "system": [{"type": "text", "text": "be brief"}],
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 999999,
            "temperature": 0.2,
            "top_p": 0.9,
            "tools": [{"name": "lookup", "description": "search", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "lookup"},
        }
        out = cs.anthropic_to_openai_responses(req)
        self.assertEqual(out["model"], "gpt-5.2")
        self.assertEqual(out["instructions"], "be brief")
        self.assertEqual(out["input"], [{"role": "user", "content": "hi"}])
        self.assertEqual(out["max_output_tokens"], 65536)
        self.assertEqual(out["temperature"], 0.2)
        self.assertEqual(out["top_p"], 0.9)
        self.assertEqual(out["tools"][0]["type"], "function")
        self.assertEqual(out["tools"][0]["name"], "lookup")
        self.assertEqual(out["tool_choice"], "auto")

    def test_responses_forced_tool_choice_string_degrades_to_auto(self):
        out = cs.anthropic_to_openai_responses({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "tool_choice": "required",
        })
        self.assertEqual(out["tool_choice"], "auto")

    def test_dashscope_responses_tool_requests_use_conservative_cap(self):
        old_url = cs.PROV.get("url")
        cs.PROV["url"] = "https://dashscope.aliyuncs.com/compatible-mode/v1/responses"
        try:
            out = cs.anthropic_to_openai_responses({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 999999,
                "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            })
        finally:
            cs.PROV["url"] = old_url
        self.assertEqual(out["max_output_tokens"], 8192)
        self.assertEqual(out["tool_choice"], "auto")

    def test_dashscope_responses_drops_incompatible_web_search_tool(self):
        old_url = cs.PROV.get("url")
        cs.PROV["url"] = "https://dashscope.aliyuncs.com/compatible-mode/v1/responses"
        try:
            out = cs.anthropic_to_openai_responses({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "web_search", "input_schema": {"type": "object"}},
                    {"name": "lookup", "input_schema": {"properties": {"q": {"type": "string"}}}},
                ],
            })
        finally:
            cs.PROV["url"] = old_url
        self.assertEqual([t["name"] for t in out["tools"]], ["lookup"])
        self.assertEqual(out["tools"][0]["parameters"]["type"], "object")

    def test_non_dashscope_responses_keeps_web_search_tool(self):
        out = cs.anthropic_to_openai_responses({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "web_search", "input_schema": {"type": "object"}}],
        })
        self.assertEqual(out["tools"][0]["name"], "web_search")

    def test_responses_tool_choice_none_passthrough(self):
        out = cs.anthropic_to_openai_responses({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "none"},
        })
        self.assertEqual(out["tool_choice"], "none")

    def test_responses_request_maps_tool_results(self):
        req = {
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"q": "x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [{"type": "text", "text": "found"}]},
                ]},
            ],
        }
        out = cs.anthropic_to_openai_responses(req)
        self.assertEqual(out["input"][0], {"role": "assistant", "content": "checking"})
        self.assertEqual(out["input"][1]["type"], "function_call")
        self.assertEqual(out["input"][1]["call_id"], "call_1")
        self.assertEqual(json.loads(out["input"][1]["arguments"]), {"q": "x"})
        self.assertEqual(out["input"][2], {"type": "function_call_output", "call_id": "call_1", "output": "found"})

    def test_responses_response_maps_text_and_function_call(self):
        resp = {
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hello"}]},
                {"type": "function_call", "call_id": "call_2", "name": "lookup", "arguments": "{\"q\":\"y\"}"},
            ],
            "usage": {"input_tokens": 4, "output_tokens": 5},
        }
        out = cs.openai_responses_to_anthropic(resp, "claude-opus-4-8")
        self.assertEqual(out["content"][0], {"type": "text", "text": "hello"})
        self.assertEqual(out["content"][1], {"type": "tool_use", "id": "call_2", "name": "lookup", "input": {"q": "y"}})
        self.assertEqual(out["stop_reason"], "tool_use")
        self.assertEqual(out["usage"], {"input_tokens": 4, "output_tokens": 5})


class RelayProvider(unittest.TestCase):
    """中转站 provider：透传（不重映射）+ 贴合 + 双鉴权头 + 不夹 max_tokens + /v1/models 归一化。"""

    def setUp(self):
        cs.PROV = dict(cs.PROVIDERS["relay"])
        cs.PROV_NAME = "relay"
        cs.PROV["url"] = "https://relay.test/claude/v1/messages"
        cs.PROV["models_url"] = "https://relay.test/claude/v1/models"
        cs.KEY = "cr_testkey"
        cs.RELAY_MODELS = []
        cs.RELAY_FORCE_MODEL = None

    def test_auth_headers_both(self):
        h = cs._upstream_auth_headers()
        self.assertEqual(h.get("x-api-key"), "cr_testkey")
        self.assertEqual(h.get("Authorization"), "Bearer cr_testkey")

    def test_deepseek_auth_headers_default_xapikey(self):
        # 回归：deepseek 未设 auth_style → 仍只带 x-api-key（不误引入 Bearer）。
        cs.PROV = cs.PROVIDERS["deepseek"]
        cs.KEY = "sk-ds"
        self.assertEqual(cs._upstream_auth_headers(), {"x-api-key": "sk-ds"})

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


class OpenAICustomProvider(unittest.TestCase):
    def setUp(self):
        cs.PROV = dict(cs.PROVIDERS["openai-custom"])
        cs.PROV_NAME = "openai-custom"
        cs.PROV["url"] = "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        cs.PROV["models_url"] = "https://open.bigmodel.cn/api/paas/v4/models"
        cs.PROV["default_model"] = "glm-4.5"
        cs.KEY = "sk-openai"
        cs.RELAY_FORCE_MODEL = "glm-4.5"

    def tearDown(self):
        cs.RELAY_FORCE_MODEL = None

    def test_openai_base_root_normalization(self):
        root = "https://open.bigmodel.cn/api/paas/v4"
        self.assertEqual(cs.normalize_openai_base(root + "/chat/completions"), root)
        self.assertEqual(cs.normalize_openai_base(root + "/responses"), root)
        self.assertEqual(cs.normalize_openai_base(root + "/models"), root)
        self.assertEqual(cs.openai_endpoint(root + "/chat/completions", "/models"), root + "/models")
        self.assertEqual(
            cs.openai_endpoint(root + "/models", "/chat/completions"),
            root + "/chat/completions",
        )
        self.assertEqual(
            cs.openai_endpoint(root + "/responses", "/responses"),
            root + "/responses",
        )
        self.assertEqual(
            cs.openai_endpoint("https://api.siliconflow.cn", "/chat/completions"),
            "https://api.siliconflow.cn/v1/chat/completions",
        )
        self.assertEqual(
            cs.openai_endpoint("https://api.siliconflow.cn/v1/models", "/chat/completions"),
            "https://api.siliconflow.cn/v1/chat/completions",
        )

    def test_force_model_enters_openai_translation(self):
        req = {
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1000000,
        }
        out = cs.anthropic_to_openai(req)
        self.assertEqual(out["model"], "glm-4.5")
        self.assertEqual(out["max_tokens"], 1000000, "custom OpenAI 不做通用 token 夹取")

    def test_auth_headers_bearer_only(self):
        self.assertEqual(cs._upstream_auth_headers(), {"Authorization": "Bearer sk-openai"})

    def test_models_uses_bearer_without_anthropic_version(self):
        seen = {}
        orig = cs.http_get_json

        def fake(url, headers):
            seen["url"] = url
            seen["headers"] = headers
            return {"data": [{"id": "glm-4.5"}]}

        cs.http_get_json = fake
        try:
            out = cs.fetch_relay_models()
        finally:
            cs.http_get_json = orig
        self.assertEqual(seen["url"], "https://open.bigmodel.cn/api/paas/v4/models")
        self.assertEqual(seen["headers"].get("Authorization"), "Bearer sk-openai")
        self.assertNotIn("anthropic-version", seen["headers"])
        self.assertEqual([m["id"] for m in out], ["glm-4.5"])

    def test_models_discovery_works_without_forced_model(self):
        cs.RELAY_FORCE_MODEL = None
        orig = cs.http_get_json
        cs.http_get_json = lambda url, headers: {"data": [{"id": "glm-4.5"}]}
        try:
            code, body = cs.build_models_response()
        finally:
            cs.http_get_json = orig
        self.assertEqual(code, 200)
        self.assertEqual([m["id"] for m in body["data"]], ["glm-4.5"])


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
        cs.fetch_relay_models = lambda runtime=None: [{"type": "model", "id": "glm-4.6"}]
        try:
            code, body = cs.build_models_response()
        finally:
            cs.fetch_relay_models = saved_fetch
        self.assertEqual(code, 200)
        self.assertEqual(body["data"][0]["id"], "glm-4.6")


if __name__ == "__main__":
    unittest.main()
