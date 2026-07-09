# CSSwitch Architecture Boundaries

This document is the public-safe boundary map for current `main`. It records how the app is split today and what future refactors must preserve. It does not claim real Claude account state, real `~/.claude-science` verification, Science GUI E2E, live provider E2E, published DMG parity, signing, notarization, or official hosted capability support.

## Authority Layers

| Layer | Current authority | What it owns | What it must not own |
|---|---|---|---|
| Frontend panel | `desktop/src/main.js` | User intent, form state, command calls, visible status rendering | Final truth for active profile, proxy process, sandbox identity, upstream route, or key validity |
| Rust command/runtime layer | `desktop/src-tauri/src/commands/`, `desktop/src-tauri/src/runtime/` | Profile transactions, config reads/writes, proxy and sandbox lifecycle, diagnostics shape | User inference payload transformation or hosted Claude capability repair |
| Python proxy gateway | `proxy/csswitch_proxy.py` and imported proxy modules | Data-plane request auth stripping, provider auth injection, model shell/force routing, Anthropic/OpenAI/Responses compatibility, CONNECT behavior | Profile persistence, UI state, real Claude account state, official hosted MCP/Directory/remote skill availability |
| Scripts | `scripts/*.sh` | Sandbox launch/stop support, local doctor checks, bundle cleanup | Long-lived runtime state ownership or real `~/.claude-science` mutation |
| Tests and findings | `test/`, `findings/`, `docs/known-issues.md` | Evidence, regression gates, known boundaries | Replacing current source/runtime verification |

The frontend and backend are separated by Tauri commands, but they are not fully independent products. The frontend is a command consumer and status renderer; Rust remains the source of truth for profile/config/runtime transactions; the Python proxy remains the current data-plane gateway until a future Rust proxy replaces it.

## Three Runtime Planes

### Control Plane

The Tauri app coordinates saved profile state, the local proxy process, and the isolated Science sandbox. Control-plane commands include creating profiles, editing connection metadata, setting the active profile, starting/stopping proxy and sandbox processes, and returning status diagnostics.

Key invariant: control-plane state changes must go through serialized Rust command paths. UI state alone cannot make a profile active or a proxy route authoritative.

### Inference Data Plane

Sandboxed Science sends Anthropic-shaped inference requests to `ANTHROPIC_BASE_URL`, which points at the local CSSwitch proxy path containing the path secret. The proxy strips inbound Science `Authorization` / `x-api-key`, injects the configured third-party key, applies the provider compatibility policy, and sends the request to the selected upstream provider.

Key invariant: only this data plane may transform inference payloads. Runtime/status/docs changes must not silently mutate outbound payload shape.

### Science External Plane

The sandbox also has outbound network behavior outside inference. Current proxy HTTPS `CONNECT` behavior fast-fails Anthropic/Claude hosts with 401 and directly tunnels non-Anthropic HTTPS CONNECT targets. Ordinary HTTP proxying and general upstream proxy support remain future work. This helps virtual-login startup avoid hangs, but it does not make hosted MCP, Directory connectors, or official remote skills available.

Key invariant: diagnostics can explain these boundaries, but CSSwitch must not claim it fully repairs official hosted capabilities.

## State Model

Future refactors should keep these state concepts separate:

| State | Meaning | Current anchors |
|---|---|---|
| `ProfileConfig` | Persisted profile list, active id, ports, path secret, masked key return | `config.rs`, `runtime/profile.rs`, `commands/profiles.rs` |
| `ActiveProfile` | The selected profile id and connection fields after validation/commit | `set_active_profile_txn`, `update_profile_connection` active path |
| `ProxyRuntime` | Running proxy child, port, path secret, provider adapter, key fingerprint, generation token | `AppState`, `runtime/proxy_lifecycle.rs`, `runtime/proxy.rs` |
| `SandboxSession` | Isolated Science process/data-dir/url/port identity | `runtime/science.rs`, `runtime/sandbox_session.rs`, launch/stop scripts |
| `DiscoveryProbe` | Temporary proxy/model probe used by fetch-models or connection validation | `runtime/model_discovery.rs`, `scratch.rs` |
| `DiagnosticsSnapshot` | Read-only status view derived from config/runtime probes/catalog rules | `commands/runtime.rs::status`, `runtime/diagnostics.rs`, `runtime/capability_catalog.rs` |

These are related but not interchangeable. In particular, `fetch_models` is discovery; it must not become a hidden active-profile switch or upstream pin. A green local proxy health check is not proof of upstream key validity. A Science selector label is not the authoritative upstream route.

## Set Current vs Pin Upstream

`set_active_profile` means: validate the candidate profile unless the user explicitly chooses skip verification after an ambiguous probe, start a formal proxy for that candidate, verify local proxy health, then commit the profile id and any active connection edit.

Pinning the upstream model is a formal proxy launch property. Relay and custom OpenAI paths pass the selected model through environment variables such as `CSSWITCH_RELAY_MODEL` or `CSSWITCH_OPENAI_MODEL`. The formal proxy then returns a Science-visible shell model from `/v1/models` and force-routes outbound inference requests to the configured real upstream model.

`fetch_models` is different: it uses a temporary probe without the formal force-model environment so the app can discover real model ids from the upstream. It must not alter config, `AppState`, the active proxy, or the running sandbox.

## Diagnostics Boundaries

Diagnostics should answer "what state do we know, and what recovery action is appropriate?" without widening claims.

Current status surfaces already include legacy green/amber lights, catalog rules, Science diagnostics, and typed `last_error.type=config_error` for config load failures. Future cleanup should converge those surfaces on this target vocabulary:

| Category | Meaning |
|---|---|
| `config_error` | Config could not be read, parsed, or trusted; do not silently default to a clean state |
| `proxy_unhealthy` | Local proxy health failed or the process is absent |
| `sandbox_identity_unknown` | Something responds on the sandbox port, but identity/data-dir ownership is not proven |
| `upstream_unreachable` | Configured provider authority is not reachable from this machine |
| `auth_boundary` | Virtual OAuth or hosted account capability boundary |
| `version_boundary` | Science version/route behavior needs isolated non-`8765` verification |
| `packaging_error` | Packaged resources are missing or cannot import the active proxy path |

Status polling should remain lightweight and must not run expensive Science binary probes, use real credentials, read real `~/.claude-science`, or turn boundary catalog entries into success claims. The sandbox status light is a local HTTP health indicator only; sandbox identity proof is reserved for startup/reuse boundaries that call `sandbox_running_ours`. `doctor.sh` also skips real HOME probing by default; checking whether `$HOME/.claude-science` exists requires explicit opt-in.

## Provider And Capability Boundaries

Provider compatibility is driven by current source and tests, not by broad provider branding:

- Anthropic-compatible native paths can preserve Anthropic tool blocks better than translated paths, but may still need provider-specific policy filters.
- OpenAI Chat and OpenAI Responses paths translate request/response shapes and are compatibility slices, not full native parity claims.
- Hosted MCP, Directory connectors, and official remote skills are claude.ai hosted capabilities. CSSwitch can diagnose and offer local alternatives, but virtual OAuth does not grant those hosted permissions.
- External Streamable HTTP MCP depends on transport egress. Current non-Anthropic CONNECT is direct tunneling; upstream proxy support remains future work until implemented and tested.

The static capability catalog and provider matrix should describe these facts, not drive unreviewed behavior changes.

## Reference Projects

CC Switch and CS Native are reference projects for control-plane and provider-switching ideas only. Do not vendor large blocks of code or assume their runtime model applies to Claude Science. CSSwitch has a Science-specific virtual-login, sandbox, and transport boundary that those references do not remove.

## Refactor Discipline

- Keep public docs, status responses, tests, and release notes aligned with current source facts.
- Keep public/private boundaries explicit; do not publish local-only architecture evidence or real-machine secrets.
- Prefer narrow tests around changed contracts: Rust for command/runtime state, Python unittest for proxy behavior, script tests for sandbox/doctor guards, frontend syntax checks for UI state changes.
- Treat `env-blocked`, `needs-real-machine`, and `current-env clean` as separate outcomes. Do not collapse them into release-ready unless the relevant gate actually passed.
