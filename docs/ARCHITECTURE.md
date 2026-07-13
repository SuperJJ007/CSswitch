# CSSwitch architecture

This file is the current architecture contract. Release notes and dated investigations are evidence, not replacements for it.

## Product boundary

CSSwitch is a provider switcher and launcher for Claude Science. It converts a selected provider profile into the Anthropic-compatible local endpoint Science expects, manages the CSSwitch Gateway, prepares the isolated local login state, and starts or reopens Science.

Science owns its product capabilities and data: projects, organizations, native Skills, Add Skill / GitHub import, runtime resources, and upgrades. CSSwitch must not make those features startup prerequisites. In the currently verified Science build, supported external-Skill authoring/import/delete paths query the Anthropic account catalog and may fail in CSSwitch third-party mode; 0.4.4 neither emulates that catalog nor claims to fix external-Skill installation. The unreleased 0.4.5 local test build adds only a user-approved public-GitHub directory bridge: one fixed routing Skill directs natural-language requests to two scoped local MCP connectors, Science owns host-access approval and Agent-Skill `attach_skill`/`detach_skill`, and the existing CSSwitch gateway performs the bounded atomic copy or quarantines its own imports. CSSwitch attaches only that fixed route through Science's loopback one-time-nonce/CSRF UI control plane; it exposes no general control client and adds no Skill Manager, catalog, inventory, or lifecycle gate.

## Runtime flow

```text
CSSwitch provider profile
  -> CSSwitch Gateway
  -> isolated local login state
  -> persistent Science data-dir
  -> start/reuse Science
  -> open Science UI
```

The one-click path must not pass through an external Skill directory, CSSwitch Skill store, inventory, Skill catalog, reconcile, or deploy step.

## Sources of truth and ownership

| Data | Source of truth | Owner |
| --- | --- | --- |
| Provider profiles and CSSwitch settings | `~/.csswitch/` configuration | CSSwitch |
| Gateway lifecycle and local routing | CSSwitch runtime state | CSSwitch |
| Installed Science executable | `/Applications/Claude Science.app/.../claude-science` | User / Science installer |
| Persistent Science state | `~/.csswitch/sandbox/home/.claude-science` | Science |
| Version-specific runtime resources | `<data-dir>/runtime/<version>/` | Science internal implementation |
| Native and imported Skills | `<data-dir>/orgs/<active-org>/skills/<name>/` | Science organization |
| Provider capability metadata | `catalog/capabilities.v1.json` | CSSwitch |
| Legacy Skill store/inventory from 0.4.2/0.4.3 | retained but unused | Neither runtime path |

CSSwitch reuses the persistent Science data-dir across launches and Science upgrades. The normal launch path does not rebuild that directory, synchronize it in either direction, or delete user changes. The separate 0.4.5 conversation bridge may atomically ensure its exact fixed route, may copy one explicitly requested public Skill into the active organization, and may atomically quarantine only a directory carrying its own valid user-import marker. Same-name user or modified route content is never overwritten.

The App executable, persistent data-dir, version-specific runtime resources, and organization Skill directories are different facts. The data-dir provides state continuity; it is not an executable-version pin. CSSwitch normally selects the currently installed Claude Science App. A valid absolute non-symlink `SCIENCE_BIN` remains an explicit development override and fails closed when invalid.

CSSwitch never copies `bin`, `conda`, `runtime`, or `seed-assets` from the real `~/.claude-science` during initialization. Existing historical cache files are retained but are not migrated, deleted, or selected automatically. If the App is absent, a cached binary is eligible only when its version can be read and the user authorizes “use cached once” in the preflight UI. This choice exists only in memory for that launch. When the App is present it always wins.

After launch, CSSwitch records the selected binary path, source, and readable version in memory. URL, status, and stop operations use that identity. After a CSSwitch restart, recovery asks candidate binaries to confirm the same data-dir daemon; it never treats port occupancy alone as identity proof.

Science's `@` output/artifact references are request attachments, not registered Skills. They can provide file or prompt context but do not replace the Skill registry, Agent binding, scripts/resources, or persistent natural-language triggering.

The executable and data directory have different ownership. For a new launch CSSwitch prefers the binary inside the locally installed official `/Applications/Claude Science.app`, while the persistent sandbox directory remains the Science-owned data source of truth. CSSwitch never reads or clones runtime assets from the user's real `~/.claude-science`. A previously retained sandbox binary may be used only after a one-launch user authorization when the App is unavailable; CSSwitch does not download Science, invoke `claude-science update`, overwrite that fallback, persist the choice, or force-restart an already healthy daemon to apply version drift.

## Network exposure boundary

The CSSwitch Gateway binds loopback, and isolated Science is launched with an explicit `--host 127.0.0.1`; CSSwitch does not provide a `0.0.0.0` switch. CSSwitch assigns Science's preview listener explicitly on the port immediately after the UI port; configuration and launch preflight reject overflow, reserved, Gateway, or occupied preview ports. Raw `serve` output is discarded because the official CLI may print a data-dir or Web UI URL; CSSwitch logs only a generic result. Its remote-access helper only generates SSH client commands that forward those two ports from client loopback to server loopback. The commands use `-F /dev/null` and explicit host-key checking, so SSH config aliases and hidden forwards are not loaded. CSSwitch never forwards the Gateway port, starts an SSH process, enables macOS Remote Login, changes `sshd` or firewall configuration, or stores an SSH destination.

The remote-access helper does not request or return a short-lived Science login URL. The local Rust backend obtains separate one-time URLs for its bounded control-plane call and browser opening, keeps them in backend memory, and never serializes them into Tauri responses or status. The UI exposes only secret-free SSH commands and clears them together with the SSH target on mode/port/runtime changes or after three minutes. The second command requests its own URL over SSH after the user runs it, so that token appears only in the access-side terminal and never enters CSSwitch frontend state, configuration, clipboard-by-default, or logs.

## Failure boundary

Provider configuration, Gateway startup, isolated-login preparation, runtime preflight, port ownership, Science launch, and Science health/identity may fail one-click startup. A missing App without an explicitly authorized readable cache is a runtime preflight result, not a provider or Skill error. Skill counts, legacy store conflicts, inventory corruption, missing Skill catalog data, external `~/.claude/skills`, and route/MCP registration failures must not fail or restart Science.

Science version discovery is fail-open with respect to an existing healthy daemon. A missing or non-runnable official App candidate makes a readable retained sandbox binary eligible for explicit one-launch authorization; it is never selected implicitly. Once a newer binary has actually attempted to open the persistent data-dir, CSSwitch must not blindly start an older binary against a potentially migrated directory.

The Skill Manager source remains recoverable from the `v0.4.3` tag and protected development worktrees, but it is not compiled, registered, packaged, or executed in the focused runtime.
