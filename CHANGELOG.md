# Changelog

CSSwitch follows semantic versioning. Older release notes remain available on the [GitHub Releases page](https://github.com/SuperJJ007/CSSwitch/releases).

## [0.4.2] — 2026-07-12

### Added

- Added a local Skill Manager that discovers compatible Skills from configured real-HOME sources, imports immutable copies into CSSwitch-managed storage, tracks inventory, and deploys selected Skills into the isolated Science sandbox.
- Added requirement inspection and compatibility reporting for Skill files, scripts, assets, references, static trees, and external Python, R, network, or tool dependencies.
- Expanded the capability catalog and installation-matrix checks used by runtime diagnostics.

### Safety and lifecycle

- Imported Skills remain in managed storage if their original source disappears, and sandbox reconstruction restores deployed Skills from that store.
- Skill discovery and deployment do not read or copy real Science credentials. The existing sandbox lifecycle and local-only runtime boundaries remain in force.
- Preserved 0.4.1's exact legacy Python proxy ownership checks: cleanup still requires the expected listener port, UID, Python identity, bundled legacy script path, and provider arguments; unknown processes fail closed.

### Upgrade notes

- Replace the existing app with `CSSwitch_0.4.2_aarch64.dmg`; v2 profiles remain compatible.
- Existing imported Skills are retained under CSSwitch-managed data. Back up `~/.csswitch/` before upgrading or rolling back.
- This release remains Apple Silicon only, ad-hoc signed, and not notarized.

## [0.4.1] — 2026-07-11

### Fixed

- Fixed upgrades from Python-based releases leaving an orphaned CSSwitch proxy on the configured port and blocking “Start.”
- CSSwitch now stops a legacy listener only when the listening PID, current user, Python process name, exact previous bundle script path, provider argument, and configured port all match.
- Unknown listeners, unrelated Python processes, and unverified stale gateways remain untouched and continue to fail closed.

### Upgrade notes

- Replace the existing app with `CSSwitch_0.4.1_aarch64.dmg`; v2 profiles remain compatible.
- The first start may take a moment while an exact legacy CSSwitch Python proxy exits and the Rust gateway takes ownership of the port.
- If CSSwitch cannot prove that a listener belongs to the legacy bundle, it will still ask you to choose a free port or stop that process manually.

## [0.4.0] — 2026-07-11

### Added

- A bundled Rust inference gateway for DeepSeek, Qwen, Anthropic-compatible relays, custom OpenAI Chat Completions, and OpenAI Responses.
- Stronger gateway health identity using provider, compatibility mode, and launch identity.
- Broader provider compatibility coverage for model mapping, tool calls, streaming, retries, and error handling.

### Changed

- Production inference, profile validation, and model discovery now use the bundled Rust gateway.
- The production app no longer ships a Python inference proxy or Python fallback.
- Provider compatibility behavior is centralized in the Rust gateway and capability catalog.
- Configuration schema remains v2, so normal in-place upgrades preserve existing profiles.

### Fixed

- Reduced the chance of accepting an unrelated or stale local listener as the active gateway.
- Improved owned-process cleanup during failed startup, stop, and application exit.
- Aligned scratch validation, model discovery, activation, and status reporting with the active profile’s adapter.

### Upgrade notes

- Download `CSSwitch_0.4.0_aarch64.dmg` and replace the existing app in Applications.
- Back up `~/.csswitch/config.json` before upgrading.
- Rollback requires reinstalling the previous stable app; there is no runtime Python-backend switch.
- The macOS build is Apple Silicon only, ad-hoc signed, and not notarized. First launch may require right-clicking the app and choosing “Open.”
- See [Upgrade and rollback](docs/upgrade-and-rollback.md).

## Previous releases

See [GitHub Releases](https://github.com/SuperJJ007/CSSwitch/releases) for notes and downloadable artifacts for v0.3.6 and earlier.
