import json
import pathlib
import re
import sys
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
CATALOG = ROOT / "catalog" / "capabilities.v1.json"
SECTIONS = [
    "providers",
    "tool_rules",
    "mcp_servers",
    "skills",
    "science_versions",
    "transport_rules",
]

REQUIRED_FIELDS = {
    "id",
    "scope",
    "match",
    "status",
    "action",
    "reason",
    "evidence",
    "tests",
}

ALLOWED_SCOPES = {
    "provider",
    "model",
    "tool",
    "mcp",
    "skill",
    "science_version",
    "transport",
}

ALLOWED_STATUS = {
    "supported",
    "limited",
    "unsupported",
    "unknown",
}

ALLOWED_ACTIONS = {
    "none",
    "normalize",
    "drop",
    "disable",
    "degrade",
    "diagnose",
    "document",
}

REQUIRED_RULE_IDS = {
    "provider.relay.force-model-shell",
    "provider.kimi.relay-thinking-enabled",
    "provider.dashscope.responses-tools-cap",
    "tool.kimi.web_search.server-tool-filter",
    "tool.relay.input-schema-normalize",
    "tool.deepseek.forced-tool-choice-disable-thinking",
    "tool.dashscope.responses.web_search-drop",
    "tool.siliconflow.forced-named-to-any",
    "transport.connect.anthropic-fastfail-401",
    "transport.connect.non-anthropic-direct-tunnel",
}

RUNTIME_OBSERVABILITY_RULE_IDS = {
    "provider.relay.force-model-shell",
    "provider.kimi.relay-thinking-enabled",
    "provider.dashscope.responses-tools-cap",
    "tool.kimi.web_search.server-tool-filter",
    "tool.relay.input-schema-normalize",
    "tool.deepseek.forced-tool-choice-disable-thinking",
    "tool.dashscope.responses.web_search-drop",
    "tool.siliconflow.forced-named-to-any",
}

EXPECTED_SKILL_CONDITIONS = {
    ("baseline", "satisfied"),
    ("sandbox", "unavailable"),
    ("sandbox", "unknown"),
    ("deployment", "not_deployed"),
    ("deployment", "pending"),
    ("deployment", "restart_required"),
    ("deployment", "failed"),
    ("discovery", "unknown"),
    ("discovery", "not_discovered"),
    *{
        (requirement, state)
        for requirement in ("network", "ssh", "mcp", "local_command")
        for state in ("requirement_unknown", "availability_unknown", "unavailable")
    },
    *{
        (requirement, state)
        for requirement in ("binary", "environment", "runtime_asset")
        for state in ("requirement_unknown", "availability_unknown", "missing")
    },
    ("platform", "requirement_unknown"),
    ("platform", "mismatch"),
    ("minimum_runtime_version", "requirement_unknown"),
    ("minimum_runtime_version", "missing_runtime_version"),
    ("minimum_runtime_version", "unparseable"),
    ("minimum_runtime_version", "not_met"),
}

UNKNOWN_SKILL_STATES = {
    "requirement_unknown",
    "availability_unknown",
    "unknown",
    "missing_runtime_version",
    "unparseable",
}
LIMITED_SKILL_CONDITIONS = {
    ("deployment", "not_deployed"),
    ("deployment", "pending"),
    ("deployment", "restart_required"),
}


def load_catalog():
    with CATALOG.open(encoding="utf-8") as f:
        return json.load(f)


PRIVATE_CATALOG_PATTERNS = tuple(re.compile(pattern, re.IGNORECASE) for pattern in (
    r"(?:/users/|/home/|/private/var/|/var/folders/|/tmp/|/etc/|~/|[a-z]:\\users\\)",
    r"(?:\.ssh(?:[/\\]|$)|\.claude-science)",
    r"(?:oauth[_-]token|access[_-]token|refresh[_-]token|api[_-]key|private[_-]key|client[_-]secret)",
    r"(?:encryption\.key|-----begin )",
    r"(?:inventory\.v1|installed_at|content_hash|source_ref|source_revision|active-org|/orgs/)",
    r"(?:api key|private key|oauth token|access token|refresh token|client secret)\s*(?::|=)\s*\S+",
))


def private_catalog_shape(text):
    return next((pattern.pattern for pattern in PRIVATE_CATALOG_PATTERNS if pattern.search(text)), None)


class CapabilityCatalogSchema(unittest.TestCase):
    def test_catalog_json_loads_and_has_v1_shape(self):
        data = load_catalog()
        self.assertEqual(data["schema_version"], 1)
        self.assertEqual(set(data), {"schema_version", *SECTIONS})
        for section in SECTIONS:
            self.assertIsInstance(data[section], list, section)

    def test_entries_have_required_fields_and_valid_enums(self):
        data = load_catalog()
        for section in SECTIONS:
            for entry in data[section]:
                with self.subTest(section=section, rule_id=entry.get("id")):
                    self.assertEqual(set(entry), REQUIRED_FIELDS)
                    self.assertIsInstance(entry["id"], str)
                    self.assertTrue(entry["id"].strip())
                    self.assertIn(entry["scope"], ALLOWED_SCOPES)
                    self.assertIn(entry["status"], ALLOWED_STATUS)
                    self.assertIn(entry["action"], ALLOWED_ACTIONS)
                    self.assertIsInstance(entry["match"], dict)
                    self.assertIsInstance(entry["reason"], str)
                    self.assertTrue(entry["reason"].strip())
                    self.assertIsInstance(entry["evidence"], list)
                    self.assertTrue(entry["evidence"], "evidence must not be empty")
                    self.assertTrue(all(isinstance(x, str) and x.strip() for x in entry["evidence"]))
                    self.assertIsInstance(entry["tests"], list)
                    self.assertTrue(all(isinstance(x, str) and x.strip() for x in entry["tests"]))

    def test_rule_ids_are_unique_and_key_rules_exist(self):
        data = load_catalog()
        ids = [
            entry["id"]
            for section in SECTIONS
            for entry in data[section]
        ]
        self.assertEqual(len(ids), len(set(ids)), "catalog rule ids must be unique")
        self.assertTrue(REQUIRED_RULE_IDS.issubset(set(ids)))

    def test_proxy_observability_rule_ids_are_cataloged(self):
        data = load_catalog()
        ids = {
            entry["id"]
            for section in SECTIONS
            for entry in data[section]
        }
        self.assertTrue(RUNTIME_OBSERVABILITY_RULE_IDS.issubset(ids))

    def test_skill_rules_use_public_match_schema_and_cover_every_condition(self):
        skills = load_catalog()["skills"]
        conditions = set()
        for rule in skills:
            with self.subTest(rule_id=rule["id"]):
                self.assertEqual(rule["scope"], "skill")
                self.assertEqual(set(rule["match"]), {"requirement", "state"})
                condition = (rule["match"]["requirement"], rule["match"]["state"])
                self.assertNotIn(condition, conditions)
                conditions.add(condition)

                if condition == ("baseline", "satisfied"):
                    self.assertEqual((rule["status"], rule["action"]), ("supported", "none"))
                elif condition in LIMITED_SKILL_CONDITIONS:
                    self.assertEqual(rule["status"], "limited")
                    self.assertIn(rule["action"], {"document", "degrade", "diagnose"})
                elif condition[1] in UNKNOWN_SKILL_STATES:
                    self.assertEqual(rule["status"], "unknown")
                    self.assertIn(rule["action"], {"document", "diagnose"})
                else:
                    self.assertEqual((rule["status"], rule["action"]), ("unsupported", "disable"))

        self.assertEqual(conditions, EXPECTED_SKILL_CONDITIONS)

    def test_ssh_bridge_is_a_transport_boundary_not_dynamic_skill_inventory(self):
        data = load_catalog()
        boundary = next(
            rule
            for rule in data["transport_rules"]
            if rule["id"] == "transport.ssh.bridge-not-implemented"
        )
        self.assertEqual(boundary["scope"], "transport")
        self.assertEqual(boundary["match"], {
            "capability": "ssh_bridge",
            "implementation": "not_available",
        })
        self.assertEqual((boundary["status"], boundary["action"]), ("limited", "document"))
        self.assertTrue(any(rule["match"] == {
            "requirement": "ssh",
            "state": "unavailable",
        } for rule in data["skills"]))

    def test_static_catalog_contains_no_inventory_or_sensitive_runtime_evidence(self):
        data = load_catalog()
        for section in SECTIONS:
            for rule in data[section]:
                serialized = json.dumps(rule, ensure_ascii=False)
                with self.subTest(section=section, rule_id=rule["id"]):
                    self.assertIsNone(
                        private_catalog_shape(serialized),
                        f"private catalog shape in {rule['id']}",
                    )
                if section == "skills":
                    self.assertTrue(all(
                        evidence.startswith(("desktop/", "test/", "scripts/"))
                        for evidence in rule["evidence"]
                    ))

        self.assertIsNone(private_catalog_shape(
            "API key and private key support are capability descriptions, not values."
        ))
        self.assertIsNone(private_catalog_shape("API key:"))
        for unsafe in (
            "oauth_token",
            "api_key",
            "private_key",
            "~/.ssh/config",
            "/Users/example/private",
            "/tmp/private",
            ".claude-science/orgs/value",
            "inventory.v1.json content_hash",
            "-----BEGIN PRIVATE KEY-----",
            "API key: sk-secret-value",
        ):
            with self.subTest(unsafe=unsafe):
                self.assertIsNotNone(private_catalog_shape(unsafe))

    def test_dashscope_rules_use_exact_request_shape_hosts(self):
        data = load_catalog()
        rules = {
            entry["id"]: entry
            for section in SECTIONS
            for entry in data[section]
        }
        for rule_id in (
            "provider.dashscope.responses-tools-cap",
            "tool.dashscope.responses.web_search-drop",
        ):
            with self.subTest(rule_id=rule_id):
                match = rules[rule_id]["match"]
                self.assertEqual(match["provider"], "openai-responses")
                self.assertEqual(match["endpoint_hosts"], ["dashscope.aliyuncs.com"])
                self.assertNotIn("base_url_contains", match)

    def test_migrated_rules_include_rust_evidence_and_tests(self):
        data = load_catalog()
        rules = {
            entry["id"]: entry
            for section in SECTIONS
            for entry in data[section]
        }
        migrated = {
            "provider.deepseek.anthropic-native",
            "provider.relay.force-model-shell",
            "provider.kimi.relay-thinking-enabled",
            "provider.dashscope.responses-tools-cap",
            "tool.kimi.web_search.server-tool-filter",
            "tool.relay.input-schema-normalize",
            "tool.siliconflow.forced-named-to-any",
            "tool.deepseek.forced-tool-choice-disable-thinking",
            "tool.dashscope.responses.web_search-drop",
            "tool.dsml.deepseek-tooluse-rewrite",
            "transport.connect.anthropic-fastfail-401",
            "transport.connect.non-anthropic-direct-tunnel",
        }
        for rule_id in migrated:
            with self.subTest(rule_id=rule_id):
                self.assertTrue(
                    any(item.startswith("desktop/gateway/") for item in rules[rule_id]["evidence"]),
                    f"{rule_id} lacks Rust gateway evidence",
                )
                self.assertTrue(
                    any(
                        item.startswith("desktop/gateway/")
                        or "test_gateway_rust" in item
                        for item in rules[rule_id]["tests"]
                    ),
                    f"{rule_id} lacks a Rust test reference",
                )

    def test_local_evidence_uses_stable_paths_without_line_numbers(self):
        data = load_catalog()
        for section in SECTIONS:
            for entry in data[section]:
                for evidence in entry["evidence"]:
                    with self.subTest(rule_id=entry["id"], evidence=evidence):
                        if evidence.startswith(("http://", "https://")):
                            continue
                        suffix = evidence.rpartition(":")[2]
                        self.assertFalse(
                            suffix.isdigit(),
                            "local evidence should use a stable path without a line number",
                        )

    def test_python_unittest_references_resolve(self):
        python_refs = []

        def collect(value):
            if isinstance(value, str):
                if value.startswith("test."):
                    python_refs.append(value)
            elif isinstance(value, dict):
                for item in value.values():
                    collect(item)
            elif isinstance(value, list):
                for item in value:
                    collect(item)

        def test_cases(suite):
            for item in suite:
                if isinstance(item, unittest.TestSuite):
                    yield from test_cases(item)
                else:
                    yield item

        collect(load_catalog())
        refs = sorted(set(python_refs))
        self.assertTrue(refs, "catalog must contain Python unittest references")

        loader = unittest.defaultTestLoader
        failures = []
        for ref in refs:
            errors_before = len(loader.errors)
            suite = loader.loadTestsFromName(ref)
            loader_errors = loader.errors[errors_before:]
            failed_tests = [
                str(case)
                for case in test_cases(suite)
                if isinstance(case, unittest.loader._FailedTest)
            ]
            details = []
            if suite.countTestCases() == 0:
                details.append("loaded zero test cases")
            if failed_tests:
                details.append(f"failed loader cases: {failed_tests}")
            if loader_errors:
                details.append(f"loader errors: {loader_errors}")
            if details:
                failures.append(f"{ref}: {'; '.join(details)}")

        self.assertFalse(
            failures,
            "unloadable catalog unittest references:\n" + "\n".join(failures),
        )


if __name__ == "__main__":
    unittest.main()
