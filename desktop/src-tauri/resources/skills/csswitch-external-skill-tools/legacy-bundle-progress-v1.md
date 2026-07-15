---
name: csswitch-external-skill-tools
description: "CSSwitch 外部 Skill 安装与卸载的强制路由。MUST use in CSSwitch-managed Claude Science whenever the user asks to install, import, add, uninstall, remove, or delete an external Skill."
---

# CSSwitch external Skill tools

This Skill only routes external Skill operations. It is not an authoring or
publishing workflow. For every request in scope, use the CSSwitch external Skill
connector and its matching tool. Do not load `customize`.

## Install or import

1. Accept a public GitHub repository root, ref root, Plugin/Skill collection,
   or exact Skill directory URL. Preserve it exactly. If the user supplied only
   a name, ask for the URL; do not search for or guess a source.
2. Load the generated connector Skill `mcp-csswitch-skill-installer`.
3. Follow that connector's API and call `install_external_skill` with the exact
   `source_url`. The equivalent REPL call is:

```python
host.mcp(
    "csswitch-skill-installer",
    "install_external_skill",
    source_url=source_url,
)
```

Never call `host.skills.edit` or `host.skills.publish` as a fallback.
Never use Add Skill ZIP or `marketplace.importSkills` as a fallback.
Never download the Skill yourself, use shell/Python filesystem APIs, use a
GitHub credential, or pass staged files to CSSwitch. The CSSwitch host owns URL
resolution, one-archive download, validation, atomic commit, and OPERON
attachment. Repository and collection URLs may install a Nature-like bundle;
this installs only its Skill collection and support resources, not hooks, MCP,
agents, or a complete Claude Plugin runtime.

## Uninstall, remove, or delete

1. Extract the exact Skill name from the user's current request. There is no
   default or hard-coded Skill name.
2. If no exact name is present, or more than one installed name could apply, ask
   the user. Never infer a filesystem path.
3. Load the generated connector Skill `mcp-csswitch-skill-installer`.
4. Follow that connector's API and call `uninstall_external_skill` with the
   exact `skill_name`. The equivalent REPL call is:

```python
host.mcp(
    "csswitch-skill-installer",
    "uninstall_external_skill",
    skill_name=skill_name,
)
```

Never call `host.skills.delete`, `skills.deleteDraft`, `host.skills.edit`, shell
commands, Python filesystem APIs, or manual filesystem deletion as a fallback.
Do not locate similarly named directories outside the CSSwitch-managed Science
data directory. If the MCP call fails, report that failure and stop.

## Result handling

For `HOST_ACCESS_REQUIRED`, submit the returned `request.payload` exactly once
under its original request ID. Poll only that ID's `response_filename`. While
the final response is absent, read `status_filename`: `PROCESSING` includes a
bounded `phase`, heartbeat timestamps, elapsed time, and `deadline_at`. Never
write the request again and never call the MCP tool again merely because a
bundle is still downloading. Success, failure, timeout, and interrupted-host
recovery all arrive as a final response after `.processing` is cleared.
`REQUEST_INTERRUPTED` is final and retryable through one new MCP call; CSSwitch
will verify any possible committed state before changing files again.

For `INSTALLED_ATTACHED_VERIFY_REQUIRED`, call `skill(skill_name)` in the current
conversation. Report the Skill usable only if that call succeeds. For
`BUNDLE_INSTALLED_ATTACHED`, report that CSSwitch installed and read back the
OPERON binding for the returned number of Skills. Do not call every member with
`skill()` and do not claim that Plugin hooks or MCP servers were installed. For
`FILES_COMMITTED_ATTACH_REQUIRED` or `ATTACH_STATE_UNCERTAIN`, call
`install_external_skill` again with the same URL to let CSSwitch verify the
committed content and retry attachment. Never call `host.agents.attach_skill`
manually.

For single-Skill uninstall, follow the native detach step explicitly returned
by the connector, then verify that `skill(skill_name)` no longer loads. A
`BUNDLE_UNINSTALLED_DETACHED` result already means the entire owning bundle was
batch-detached and quarantined; do not detach its members again. Report every
response faithfully.
