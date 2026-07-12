# v0.4.2 release evidence

Captured at `2026-07-12T08:20:35Z` before commit/tag/push/release.

## Source identity

- Baseline HEAD: `85825b21cc6a96492961291ed98f96ac078af9cc`
- Implementation-tree SHA-256: `255092bf6df0d00ca23fc756889be3c84e6a1a25b016b0a7273340b6133678ef`
- Implementation-status SHA-256: `e8f367889451723b22801b14b880a5aa05b11cb98a77daf1574c55c1617eb84e`
- Version: `0.4.2` in npm package/lock, Cargo package/lock, and Tauri configuration.

The two fingerprints intentionally exclude only this evidence file, avoiding a self-referential
hash. The tree fingerprint binds the complete tracked binary diff plus the path and content hash
of every non-ignored untracked implementation file. Reproduce it from the repository root with:

```sh
{ git diff HEAD --binary -- . ':(exclude)docs/release-evidence-v0.4.2.md'; git ls-files --others --exclude-standard | LC_ALL=C sort | while IFS= read -r f; do [ "$f" = docs/release-evidence-v0.4.2.md ] || shasum -a 256 "$f"; done; } | shasum -a 256
```

The status fingerprint binds the HEAD-relative name/status list plus non-ignored untracked paths,
excluding this evidence file. Unlike porcelain status, it is independent of whether reviewed files
are staged:

```sh
{ git diff HEAD --name-status -- . ':(exclude)docs/release-evidence-v0.4.2.md'; git ls-files --others --exclude-standard | LC_ALL=C sort | while IFS= read -r f; do [ "$f" = docs/release-evidence-v0.4.2.md ] || printf '??\t%s\n' "$f"; done; } | shasum -a 256
```

## Gates

| Layer | Command | Exit | Result |
|---|---|---:|---|
| Full repository | `bash test/run_all.sh` | 0 | offline, loopback, scripts, Rust, and frontend all pass; persisted log: `/private/tmp/csswitch-v042-run-all-final.log` |
| Rust format | `cargo fmt --all -- --check` | 0 | pass |
| Rust lint | `cargo clippy --all-targets -- -D warnings` | 0 | pass |
| Working-tree whitespace | `git diff --check` | 0 | pass |
| Legacy upgrade cleanup | `cargo test runtime::legacy_proxy::tests::` | 0 | 5 passed; exact owned legacy listener stopped, unknown Python listener retained |
| Four scientific Skill shapes | explicit ignored `verified_scientific_skill_shapes_scan_import_deploy_and_rebuild` | 0 | scan, automatic import, store/inventory, deploy, source-loss retention, and rebuild restore passed; evidence SHA-256 `74ca0557a0745e4d9dcc14f3feda1b3fbb83aa00385c360286349f2f3b9700c5` |

The four shape fixtures were `bgpt-paper-search`, `arbor`, `citation-management`, and `database-lookup`. Science discovery/trigger and their network, Python-package, MCP, evaluator, or database-backed scientific functions are not claimed by the shape-only run.

## Installed Science E2E

- Discovery and sandbox rebuild: passed, one test, `242.92s`.
  Evidence root: `/private/var/folders/49/dnl__mlx7bv24krr3fhyhptm0000gn/T/csswitch-real-skill-discovery-40988-1783843646305263000`.
- Enabled trigger and disabled rejection: passed, one test, `334.74s`.
  Evidence root: `/private/var/folders/49/dnl__mlx7bv24krr3fhyhptm0000gn/T/csswitch-real-skill-trigger-39159-1783843279399552000`.
- Both runs used temporary HOME/config/data, dynamic non-8765 ports, an `env_clear` child allowlist, and a fake `security` executable present before Science launch.
- Each retained marker is a regular `0600` file with exactly two records, one per Science PID. Every record is `argv=find-generic-password exit=1`; retained stderr contains one corresponding exit-1 line per launch and no `ETIMEDOUT`.
- The earlier invalid E2E runs that reached real `security` and returned `ETIMEDOUT` remain a historical disclosure. They did not read Keychain data, but historical side effects cannot be excluded absolutely. No later valid run used real `security`.

## Final artifact and temporary installation

- Build: `npm run tauri build`, exit 0. Persisted log: `/private/tmp/csswitch-v042-build-final.log`.
- App: `desktop/src-tauri/target/release/bundle/macos/CSSwitch.app`.
- DMG: `desktop/src-tauri/target/release/bundle/dmg/CSSwitch_0.4.2_aarch64.dmg`.
- DMG SHA-256: `1044c107d120d549f64f908463cd09e44ef59aa6086d86ac362d24a1b2f4b697`.
- Read-only DMG verification and mount passed. The app was copied only to `/private/tmp/csswitch-v042-final-installed/CSSwitch.app`; `/Applications/CSSwitch.app` was not overwritten.
- Temporary-copy executable hashes: desktop `c9ba7b0cda8749682289a4f235b66230e9c67653d5073d0e761b9992ce033a2f`; gateway `32de119d3e7ae50168afefb984be8b830d4fd50b1fb25ccc023e90e578c65929`.
- `codesign --verify --deep --strict` passed for the packaged app and temporary copy. The signature is ad-hoc, `spctl` rejects it, and no notarization ticket is stapled.

## Accepted distribution boundary

No non-secret Developer ID signing/notarization configuration was available to the build. The
publisher explicitly accepts distribution of this open-source artifact with its existing ad-hoc
signature and without notarization, a stapled ticket, or Gatekeeper acceptance. This is a release
boundary, not evidence of those checks passing; users may need to right-click the app and choose
Open. No credential store, Keychain, OAuth data, API key, SSH material, or account database was
read while establishing this boundary.
