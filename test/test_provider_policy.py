import os
import sys
import unittest

HERE = os.path.dirname(__file__)
sys.path.insert(0, os.path.join(HERE, "..", "proxy"))
import csswitch_proxy as cs          # 复用 PROVIDERS 作为配置真源
import provider_policy as pp


def _state(prov, prov_name, **over):
    return pp.ProviderState(
        policy=pp.policy_from_prov(prov),
        prov_name=prov_name,
        relay_force_model=over.get("relay_force_model"),
        relay_models=over.get("relay_models", []),
        relay_thinking=over.get("relay_thinking"),
        shim_mode=over.get("shim_mode", "off"),
        nonce_factory=over.get("nonce_factory", pp._default_nonce),
    )


class ResolveModelDeepseek(unittest.TestCase):
    def setUp(self):
        self.st = _state(cs.PROVIDERS["deepseek"], "deepseek")

    def test_selector_id_maps_to_real_deepseek_id(self):
        self.assertEqual(pp.resolve_model("claude-opus-4-8", self.st), "deepseek-v4-pro")
        self.assertEqual(pp.resolve_model("claude-haiku-4-5", self.st), "deepseek-v4-flash")

    def test_empty_falls_back_to_default(self):
        self.assertEqual(pp.resolve_model("", self.st), "deepseek-v4-flash")

    def test_date_suffix_stripped_then_mapped(self):
        self.assertEqual(pp.resolve_model("claude-sonnet-5-20260101", self.st), "deepseek-v4-flash")

    def test_unknown_falls_back_to_default(self):
        self.assertEqual(pp.resolve_model("totally-unknown", self.st), "deepseek-v4-flash")


class ResolveModelRelay(unittest.TestCase):
    def setUp(self):
        self.prov = dict(cs.PROVIDERS["relay"])

    def test_passthrough_keeps_model_name(self):
        st = _state(self.prov, "relay")
        self.assertEqual(pp.resolve_model("claude-opus-4-8", st), "claude-opus-4-8")
        self.assertEqual(pp.resolve_model("some-other-model", st), "some-other-model")

    def test_empty_model_falls_back_to_default(self):
        st = _state(self.prov, "relay")
        self.assertEqual(pp.resolve_model("", st), "claude-opus-4-8")

    def test_snaps_bare_id_to_upstream_dated_id(self):
        st = _state(self.prov, "relay",
                    relay_models=["claude-haiku-4-5-20251001", "claude-opus-4-8"])
        self.assertEqual(pp.resolve_model("claude-haiku-4-5", st), "claude-haiku-4-5-20251001")
        self.assertEqual(pp.resolve_model("claude-opus-4-8", st), "claude-opus-4-8")
        self.assertEqual(pp.resolve_model("claude-sonnet-5", st), "claude-sonnet-5")

    def test_force_model_overrides_everything(self):
        st = _state(self.prov, "relay", relay_force_model="mimo-v2.5-pro")
        self.assertEqual(pp.resolve_model("claude-opus-4-8", st), "mimo-v2.5-pro")
        self.assertEqual(pp.resolve_model("claude-haiku-4-5", st), "mimo-v2.5-pro")
        self.assertEqual(pp.resolve_model("", st), "mimo-v2.5-pro")


class ResolveModelOpenAICustom(unittest.TestCase):
    def test_force_model_overrides_claude_shell(self):
        prov = dict(cs.PROVIDERS["openai-custom"])
        prov["default_model"] = "glm-4.5"
        st = _state(prov, "openai-custom", relay_force_model="glm-4.5")
        self.assertEqual(pp.resolve_model("claude-opus-4-8", st), "glm-4.5")
        self.assertEqual(pp.resolve_model("", st), "glm-4.5")


class ClampMaxTokens(unittest.TestCase):
    def setUp(self):
        self.ds = _state(cs.PROVIDERS["deepseek"], "deepseek")
        self.qw = _state(cs.PROVIDERS["qwen"], "qwen")
        self.relay = _state(dict(cs.PROVIDERS["relay"]), "relay")

    def test_cap_uses_target_model_entry(self):
        self.assertEqual(pp.clamp_max_tokens(100000, "deepseek-v4-pro", self.ds), 65536)
        self.assertEqual(pp.clamp_max_tokens(100000, "deepseek-v4-flash", self.ds), 32768)

    def test_unknown_model_uses_default_cap(self):
        self.assertEqual(pp.clamp_max_tokens(100000, "who-knows", self.ds), 8192)

    def test_not_clamped_below_request(self):
        self.assertEqual(pp.clamp_max_tokens(500, "deepseek-v4-pro", self.ds), 500)

    def test_none_passthrough(self):
        self.assertIsNone(pp.clamp_max_tokens(None, "deepseek-v4-pro", self.ds))

    def test_qwen_per_model(self):
        self.assertEqual(pp.clamp_max_tokens(100000, "qwen-max", self.qw), 8192)

    def test_relay_no_clamp(self):
        self.assertEqual(pp.clamp_max_tokens(1000000, "claude-opus-4-8", self.relay), 1000000)


class ThinkingNormalization(unittest.TestCase):
    def test_deepseek_forced_tool_choice_disables_thinking(self):
        body = {"tool_choice": {"type": "any"}, "thinking": {"type": "auto"}}
        rule_ids = []
        self.assertEqual(
            pp.normalize_thinking(body, "deepseek", rule_ids=rule_ids)["thinking"],
            {"type": "disabled"},
        )
        self.assertEqual(rule_ids, [pp.RULE_TOOL_DEEPSEEK_FORCED_TOOL_CHOICE_DISABLE_THINKING])

    def test_relay_forced_tool_choice_not_disabled(self):
        body = {"tool_choice": {"type": "tool", "name": "x"}, "thinking": {"type": "auto"}}
        out = pp.normalize_thinking(body, "relay")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_relay_forced_without_thinking_not_injected(self):
        out = pp.normalize_thinking({"tool_choice": {"type": "any"}}, "relay")
        self.assertNotIn("thinking", out)

    def test_deepseek_auto_becomes_adaptive(self):
        out = pp.normalize_thinking({"thinking": {"type": "auto"}}, "deepseek")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_relay_auto_becomes_adaptive(self):
        out = pp.normalize_thinking({"thinking": {"type": "auto"}}, "relay")
        self.assertEqual(out["thinking"]["type"], "adaptive")

    def test_relay_non_auto_thinking_preserved(self):
        body = {"thinking": {"type": "enabled", "budget_tokens": 1024}}
        self.assertEqual(pp.normalize_thinking(body, "relay")["thinking"],
                         {"type": "enabled", "budget_tokens": 1024})

    def test_noop_when_no_thinking_and_no_forcing(self):
        self.assertNotIn("thinking", pp.normalize_thinking({"messages": []}, "relay"))

    def test_relay_enabled_policy_auto_becomes_enabled(self):
        body = {"thinking": {"type": "auto"}, "max_tokens": 2048}
        out = pp.normalize_thinking(body, "relay", "enabled")
        self.assertEqual(out["thinking"]["type"], "enabled")
        self.assertGreater(out["thinking"]["budget_tokens"], 0)
        self.assertLess(out["thinking"]["budget_tokens"], 2048)

    def test_relay_enabled_policy_injects_when_missing(self):
        out = pp.normalize_thinking({"max_tokens": 2048}, "relay", "enabled")
        self.assertEqual(out["thinking"]["type"], "enabled")

    def test_relay_enabled_policy_drops_forced_tool_choice(self):
        body = {"tool_choice": {"type": "tool", "name": "lookup_weather"},
                "tools": [{"name": "lookup_weather"}],
                "max_tokens": 2048}
        out = pp.normalize_thinking(body, "relay", "enabled")
        self.assertNotIn("tool_choice", out)
        self.assertEqual(out["tools"], [{"name": "lookup_weather"}])
        self.assertEqual(out["thinking"]["type"], "enabled")

    def test_relay_enabled_policy_preserves_existing_enabled(self):
        body = {"thinking": {"type": "enabled", "budget_tokens": 512}, "max_tokens": 2048}
        out = pp.normalize_thinking(body, "relay", "enabled")
        self.assertEqual(out["thinking"], {"type": "enabled", "budget_tokens": 512})

    def test_relay_enabled_budget_stays_below_max_tokens(self):
        body = {"thinking": {"type": "auto"}, "max_tokens": 100}
        out = pp.normalize_thinking(body, "relay", "enabled")
        self.assertLess(out["thinking"]["budget_tokens"], 100)
        self.assertGreaterEqual(out["thinking"]["budget_tokens"], 1)

    def test_deepseek_unaffected_by_relay_thinking_arg(self):
        body = {"tool_choice": {"type": "any"}, "thinking": {"type": "auto"}}
        self.assertEqual(pp.normalize_thinking(body, "deepseek", "enabled")["thinking"],
                         {"type": "disabled"})


class PolicyFromProv(unittest.TestCase):
    def test_extracts_policy_fields_without_runtime_fields(self):
        pol = pp.policy_from_prov(cs.PROVIDERS["deepseek"])
        self.assertTrue(hasattr(pol, "passthrough"))
        self.assertEqual(pol.default_model, "deepseek-v4-flash")
        self.assertEqual(pol.model_caps["deepseek-v4-pro"], 65536)
        # 骨架 / 启动字段不泄进 Policy。
        self.assertFalse(hasattr(pol, "url"))
        self.assertFalse(hasattr(pol, "auth_style"))
        self.assertFalse(hasattr(pol, "key_env"))

    def test_extracts_force_model_override(self):
        self.assertTrue(pp.policy_from_prov(cs.PROVIDERS["openai-custom"]).force_model_override)
        self.assertTrue(pp.policy_from_prov(cs.PROVIDERS["relay"]).force_model_override)
        self.assertFalse(pp.policy_from_prov(cs.PROVIDERS["qwen"]).force_model_override)


if __name__ == "__main__":
    unittest.main()
