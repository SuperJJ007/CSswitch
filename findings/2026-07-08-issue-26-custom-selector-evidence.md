# Issue #26 custom model selector evidence

Date: 2026-07-08

Scope: local isolated proxy/mock evidence for issue #26. This file does not claim real Claude account state, real `~/.claude-science`, Science GUI E2E, published DMG, signing, notarization, or official hosted capability verification.

## What is covered

- `openai-custom` formal proxy with a selected model returns a single Science-visible shell model from `/{secret}/v1/models`:
  - `id`: `claude-opus-4-8`
  - `display_name`: the configured real model, such as `glm-4.5`
- `openai-responses` formal proxy follows the same selector contract with `display_name` set to the configured real model, such as `gpt-5.2`.
- In force mode, the selector response does not call the mock upstream `/models` endpoint, so raw third-party model ids are not exposed back to Science.
- Proxy logs record the selector GET path as a force-shell response while avoiding API key output.

## Evidence commands

```bash
python3 -m unittest test.test_proxy_units test.test_proxy_auth -v
```

The relevant assertions live in [`test/test_proxy_auth.py`](../test/test_proxy_auth.py):

- `ProxyOpenAICustomForcedModelList.test_formal_proxy_returns_claude_shell_for_science_selector`
- `ProxyOpenAIResponses.test_models_returns_claude_shell_for_science_selector`

## Remaining boundary

This is enough to prove the local proxy selector contract for #26 on the current source tree. It is not a real Science UI screenshot and is not proof that a previously published `v0.3.6` DMG contains this fix. Closing #26 as a product bug should either cite this as source-level evidence only or add a separate isolated Science UI/published-artifact verification note.
