# Provider Capability Matrix

This matrix records the current source-level provider/runtime contract for `main` after PR #40 and the 2026-07-09 closeout work. It is public-safe: it does not include keys, account state, private paths, or live-provider results.

It is not a live provider certification, real Claude account check, Science GUI E2E result, published DMG check, signing/notarization check, or official hosted capability claim. The authoritative implementation inputs are:

- Templates: `desktop/src-tauri/src/templates.rs`
- Runtime adapters and policy: `desktop/src-tauri/src/runtime/provider.rs`, `proxy/provider_policy.py`
- Capability diagnostics: `catalog/capabilities.v1.json`, `desktop/src-tauri/src/runtime/capability_catalog.rs`
- Status shape: `desktop/src-tauri/src/commands/runtime.rs`, `desktop/src-tauri/src/runtime/diagnostics.rs`

## Runtime Provider Matrix

| Family | Current templates | Runtime adapter | API format | Base URL handling | Model handling | Tool handling | Catalog/status surface | Evidence |
|---|---|---|---|---|---|---|---|---|
| Native Anthropic-compatible | `deepseek` | `deepseek` | `anthropic` | Fixed DeepSeek Anthropic endpoint | Science shell ids map to provider-supported ids where needed | Anthropic tool blocks pass through with DeepSeek-specific thinking/tool-choice normalization | `status().catalog.active_rules` can include `provider.deepseek.anthropic-native`; request-path static catalog also records `tool.deepseek.forced-tool-choice-disable-thinking` | `templates.rs`, `csswitch_proxy.py`, `provider_policy.py`, `test_provider_policy` |
| Relay Anthropic-compatible presets | `glm`, `xiaomi`, `siliconflow`, `kimi`, `minimax`, `openrouter`, `custom` | `relay` | `anthropic` | Preset default or editable user base URL | Formal proxy launch pins selected upstream model through `CSSWITCH_RELAY_MODEL`; proxy maps it to internal `RELAY_FORCE_MODEL`; discovery proxies do not pin | Relay schema normalization; Kimi-specific web-search server-tool filtering and thinking policy | `status().catalog.active_rules` can include `provider.relay.force-model-shell` and `provider.kimi.relay-thinking-enabled`; request-path static catalog also records tool rules such as `tool.kimi.web_search.server-tool-filter` and `tool.relay.input-schema-normalize` | `templates.rs`, `proxy_lifecycle.rs`, `scratch.rs`, `anthropic_compat.py`, `test_proxy_units`, Rust env tests |
| Qwen OpenAI Chat | `qwen` | `qwen` | `openai_chat` | Fixed DashScope compatible-mode root | Qwen adapter resolves configured/default model | Anthropic Messages tools convert to Chat Completions tools and map tool calls back to Anthropic shape | No Qwen-specific active catalog rule yet; status still exposes profile context and gateway `python-proxy` | `templates.rs`, `openai_chat_compat.py`, `provider_policy.py`, proxy unit tests |
| Custom OpenAI Chat | `custom-openai` | `openai-custom` | `openai_chat` | User-provided editable base URL | Formal proxy launch pins selected upstream model through `CSSWITCH_OPENAI_MODEL`; discovery proxies do not pin | Anthropic Messages tools convert to Chat Completions tools and map tool calls back | Status currently reports generic profile/runtime context; provider-specific support remains source/test evidence, not live certification | `templates.rs`, `proxy_lifecycle.rs`, `scratch.rs`, `openai_chat_compat.py`, `test_proxy_auth`, `test_proxy_units` |
| Custom OpenAI Responses | `custom-openai-responses` | `openai-responses` | `openai_responses` | User-provided editable base URL | Formal proxy launch pins selected model; `max_output_tokens` uses Responses policy caps | Anthropic tools convert to Responses function tools; forced tool choices degrade conservatively where needed | DashScope-specific Responses rules are static/request-path catalog entries; they are not automatically active for every Responses profile | `templates.rs`, `responses_compat.py`, `test_proxy_units`, `test_proxy_auth` |

## State And Pinning Contract

| Surface | Authority | Must not imply |
|---|---|---|
| Active profile | Rust config/profile transaction layer | Does not by itself prove upstream model availability or mutate the upstream selection outside the transaction |
| Formal proxy launch | Rust proxy lifecycle env contract | Pinning is runtime launch state, not a profile-list side effect |
| Model discovery | Scratch proxy / discovery probe | Must not set launch pin env such as `CSSWITCH_RELAY_MODEL` or `CSSWITCH_OPENAI_MODEL` |
| Science selector shell | Proxy `/v1/models` response | Science-visible `claude-opus-4-8` shell/display is not the authoritative upstream route |
| Runtime status | Read-only lights + catalog + diagnostics | Green/amber lights are local observations, not live-provider, real-account, GUI, DMG, signing, or notarization proof |

## Diagnostics Alignment

| Diagnostic class | Current source behavior | Catalog/status relationship |
|---|---|---|
| `config_error` | `status()` returns typed `last_error` and fails closed without probing default/stale ports when config load fails | Runtime status field, not a catalog rule |
| `proxy_unhealthy` | Proxy-dead state is still visible as an amber proxy light; no supervisor/restart loop is claimed | Future #12 recovery work should add typed proxy recovery state before claiming closure |
| `sandbox_identity_unknown` | Startup/reuse identity uses `claude-science status --data-dir` plus health; status polling itself only shows local HTTP health | `status().science` explicitly says binary/version probe is not run in polling |
| `upstream_unreachable` | Status TCP probes the parsed endpoint host and port, including custom non-443 ports | Local reachability only; not a provider API success claim |
| `auth_boundary` | Virtual OAuth and hosted account features are boundary diagnostics | Catalog rules such as `science.auth.virtual-oauth-scope-boundary` and hosted MCP/Directory entries |
| `version_boundary` | Science version route/auth differences are documented, not solved by status polling | Catalog rules such as `science.version.0_1_15_dev.route-diff` and `science.auth.refresh-hardcoded-0_1_15` |
| `packaging_error` | Proxy bundle closure is covered by packaging smoke for local proxy modules | Test/gate evidence only; not a DMG or notarization claim |

## Hosted Capability Boundaries

| Capability | Current catalog status | Current product stance |
|---|---|---|
| Anthropic-hosted HCLS MCP | `unsupported` / `diagnose` | CSSwitch can explain the hosted-account boundary; it cannot make official hosted MCP available under virtual OAuth |
| External Streamable HTTP MCP | `limited` / `diagnose` | Direct CONNECT tunneling exists for non-Anthropic hosts, but upstream-proxy routing is not implemented |
| Local stdio bio connectors | `unknown` / `document` | Preferred fallback direction, but install/discovery/restart persistence needs explicit verification before support is claimed |
| Directory connectors | `unsupported` / `diagnose` | Hosted claude.ai capability; may show unavailable/session-expired under virtual login |
| Official remote skills | `unsupported` / `diagnose` | Local/GitHub skills can be documented separately; official remote skills remain hosted-account dependent |

## Relationship To Historical Provider Research

[`docs/provider-support.md`](provider-support.md) is historical research and reference material. It is useful for candidate provider discovery and CC Switch comparison, but the matrix above is the current CSSwitch support boundary for source, catalog, status, docs, and tests.
