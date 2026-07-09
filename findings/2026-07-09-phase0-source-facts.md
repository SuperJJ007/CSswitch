# Phase 0 current source facts for #11 / #12 / #15

Date: 2026-07-09

Scope: current `main` source-tree evidence only, after PR #40. This file does not claim real Claude account state, real `~/.claude-science`, GUI E2E, Science/provider live E2E, published DMG parity, signing, notarization, or official hosted capability verification.

## GitHub issue #12: proxy recovery diagnostics

Current source facts:

- `status()` polls the configured proxy port with the path secret and reports a green/amber light, but it does not restart a dead proxy or keep a supervisor loop.
- `one_click_login` and explicit lifecycle commands can call `ensure_proxy`; recovery happens only when the user invokes those command paths.
- The proxy `/health` endpoint is still path-secret protected for normal HTTP requests; PR #13 remains an external open PR and is not part of current `main`.
- `status()` now surfaces config-load failures as a typed `last_error`, but a proxy-dead state is still visible only as an amber light and is not yet classified as a recovery action or typed proxy error.

Evidence anchors:

- `desktop/src-tauri/src/commands/runtime.rs::status`
- `desktop/src-tauri/src/runtime/diagnostics.rs::build_status_response`
- `desktop/src-tauri/src/runtime/proxy_lifecycle.rs::ensure_proxy`
- `desktop/src-tauri/src/runtime/sandbox_session.rs::one_click_login`
- `proxy/csswitch_proxy.py::H.do_GET`

Phase implication: #12 still needs explicit diagnostics/recovery design. A future fix should not be described as closed by health endpoint wording alone; it needs typed proxy state, user-facing recovery guidance, and optionally controlled restart behavior.

## GitHub issue #15: Science auth refresh / logged-out boundary

Current source facts:

- The capability catalog contains `science.auth.refresh-hardcoded-0_1_15`.
- `status().science` exposes virtual OAuth boundary metadata and states that polling does not run Science binary/version probes.
- This is diagnostic surfacing only. Current source does not fix real auth refresh, prove real-account status, or verify the user's real HOME.

Evidence anchors:

- `catalog/capabilities.v1.json` rule `science.auth.refresh-hardcoded-0_1_15`
- `desktop/src-tauri/src/runtime/capability_catalog.rs::diagnostics_for_profile`
- `desktop/src-tauri/src/runtime/diagnostics.rs::science_diagnostics`
- `docs/known-issues.md` section `2a`

Phase implication: Phase 4 should keep auth refresh as an `auth_boundary` / `version_boundary` diagnostic until an isolated non-`8765` reproduction proves a real fix. Do not turn the catalog rule into a success claim.

## GitHub issue #11: Streamable HTTP MCP transport boundary

Current source facts:

- Anthropic/Claude host CONNECT requests are fast-failed with 401 so virtual login paths do not hang on organization switching.
- Non-Anthropic CONNECT currently opens a direct TCP tunnel from the local proxy. It does not use an upstream proxy setting.
- The catalog marks external Streamable HTTP MCP as `limited` and upstream proxy support as `unknown` / planned, not implemented.
- `proxy/http_transport.py` is a helper for proxy outbound HTTP requests; its presence does not mean `--upstream-proxy` or full external MCP recovery is implemented.

Evidence anchors:

- `proxy/csswitch_proxy.py::H.do_CONNECT`
- `catalog/capabilities.v1.json` rules `transport.connect.non-anthropic-direct-tunnel` and `transport.upstream-proxy.planned-for-http-mcp`
- `test/test_proxy_connect.py`
- open PR #14 is still outside current `main`

Phase implication: #11 should remain a transport diagnostics / future upstream proxy item. CSSwitch can diagnose or document hosted/direct-egress boundaries, but must not claim to repair hosted MCP, Directory connectors, or official remote skills.

## Legacy docs note: "bug-list #11" model selector

`docs/known-issues.md` also contains a historical user bug-list row numbered `11` for the Science model selector showing `claude`/`opus`. That is separate from GitHub issue #11 above.

Current source facts for that legacy row:

- Formal relay/OpenAI proxy force mode returns a single Science-visible shell model and puts the configured real model in `display_name`.
- Formal launch env force-overrides to the configured real model when `CSSWITCH_RELAY_MODEL` / `CSSWITCH_OPENAI_MODEL` is set; the proxy maps the relay env into its internal force-model state.
- App-side `fetch_models` uses a temporary proxy without force model and is meant for discovery only.

Evidence anchors:

- `proxy/csswitch_proxy.py::build_models_response`
- `proxy/provider_policy.py::resolve_model`
- `desktop/src-tauri/src/runtime/model_discovery.rs::fetch_models`
- `findings/2026-07-08-issue-26-custom-selector-evidence.md`
- `findings/2026-07-08-issue-26-science-ui-evidence.md`

Phase implication: Phase 3 should preserve this split: "set current profile" chooses the active profile; "pin upstream model" is formal proxy launch state; Science selector shell/display is not the authoritative source of the active upstream route.
