use std::fs::{File, OpenOptions};
use std::path::Path;

use csswitch_skill_install_core::{
    attach_skill, install_local_package, update_agent_skills, verify_attach_control_ready,
    AttachResult, BundleCommit, InstallCommit, InstallError, InstalledPackage, LocalArchiveInput,
    ScienceHostContext, SCHEMA_VERSION,
};
use serde_json::{json, Value};
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::runtime::science::{probe_known_runtime, SandboxScienceState};
use crate::{config, lock, run_blocking, SharedAppState};

#[tauri::command]
pub(crate) async fn install_local_skill_package(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
) -> Result<Value, String> {
    let state = state.inner().clone();
    let before = match current_science_context(&state) {
        Ok(context) => context,
        Err(message) => return Ok(not_ready(&message)),
    };
    if let Err(error) = verify_attach_control_ready(&before) {
        return Ok(not_ready(&error.message));
    }
    let picker_app = app.clone();
    let selected = run_blocking(move || {
        Ok(picker_app
            .dialog()
            .file()
            .set_title("导入 Skill 包")
            .add_filter("Skill package", &["zip", "skill"])
            .blocking_pick_file())
    })
    .await?;
    let Some(selected) = selected else {
        return Ok(json!({
            "schema_version": SCHEMA_VERSION,
            "status": "CANCELLED",
            "skill_name": null,
            "source_kind": "local_zip",
            "directory_commit": false,
            "attach_attempted": false,
            "attach_required": false,
            "attach_verified": false,
            "load_verification_required": false,
            "content_sha256": null,
            "source_digest_sha256": null,
            "message": "已取消导入 Skill 包"
        }));
    };
    let path = match selected.into_path() {
        Ok(path) => path,
        Err(_) => return Ok(local_error("INVALID_ARCHIVE_PATH", "选择结果不是本地文件")),
    };
    let after = match current_science_context(&state) {
        Ok(context) if context == before => context,
        Ok(_) => return Ok(not_ready("选择文件期间 Science runtime 已变化")),
        Err(message) => return Ok(not_ready(&message)),
    };
    if let Err(error) = verify_attach_control_ready(&after) {
        return Ok(not_ready(&error.message));
    }
    run_blocking(move || install_selected_path(&path, &after)).await
}

fn current_science_context(state: &SharedAppState) -> Result<ScienceHostContext, String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let (runtime, state_port) = {
        let st = lock(state);
        (st.science_runtime.clone(), st.sandbox_port)
    };
    let runtime = runtime.ok_or("Science 未运行或 runtime 身份尚未确认")?;
    let port = if state_port == 0 {
        cfg.sandbox_port
    } else {
        state_port
    };
    if port != cfg.sandbox_port
        || probe_known_runtime(port, &runtime) != SandboxScienceState::RunningHealthy
    {
        return Err("Science 未处于 RunningHealthy，未导入任何文件".into());
    }
    runtime.skill_install_host_context(port)
}

fn install_selected_path(path: &Path, context: &ScienceHostContext) -> Result<Value, String> {
    let archive_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("选择的 Skill 包文件名不是 UTF-8")?;
    let mut file = open_regular_nofollow(path).map_err(|message| message.to_string())?;
    let commit = match install_local_package(
        &context.data_dir,
        LocalArchiveInput {
            file: &mut file,
            archive_name,
        },
    ) {
        Ok(commit) => commit,
        Err(error) => return Ok(install_error_payload(error)),
    };
    Ok(match commit {
        InstalledPackage::Skill(commit) => attach_result_payload(context, commit),
        InstalledPackage::Bundle(commit) => attach_bundle_result_payload(context, commit),
    })
}

fn open_regular_nofollow(path: &Path) -> Result<File, &'static str> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| "无法安全打开所选 Skill 包")?;
    let metadata = file.metadata().map_err(|_| "无法读取所选 Skill 包元数据")?;
    if !metadata.is_file() {
        return Err("所选 Skill 包不是普通文件");
    }
    Ok(file)
}

fn attach_result_payload(context: &ScienceHostContext, commit: InstallCommit) -> Value {
    let attach = attach_skill(context, &commit.skill_name, &commit.active_org);
    let (status, message, attach_required, attach_verified) = match attach.as_ref() {
        Ok(AttachResult::Attached | AttachResult::AlreadyAttached) => (
            "INSTALLED_ATTACHED_VERIFY_REQUIRED",
            "文件已安装并绑定 OPERON；当前 Agent 会话仍需调用 skill(skill_name) 验证加载。"
                .to_string(),
            false,
            true,
        ),
        Err(error) if error.uncertain => (
            "ATTACH_STATE_UNCERTAIN",
            format!("文件已安装，但 OPERON 绑定结果不确定：{}", error.message),
            true,
            false,
        ),
        Err(error) => (
            "FILES_COMMITTED_ATTACH_REQUIRED",
            format!(
                "文件已安装，但自动绑定失败：{}。可重新导入同一包重试。",
                error.message
            ),
            true,
            false,
        ),
    };
    let attach_error = attach.err();
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "skill_name": commit.skill_name,
        "source_kind": "local_zip",
        "directory_commit": commit.directory_commit,
        "install_action": commit.action.as_str(),
        "attach_attempted": true,
        "attach_required": attach_required,
        "attach_verified": attach_verified,
        "load_verification_required": attach_verified,
        "content_sha256": commit.content_sha256,
        "source_digest_sha256": commit.source_digest_sha256,
        "dependency_scan": commit.dependency_scan,
        "agent_name": "OPERON",
        "attach_method": "csswitch_auto_attach",
        "source_resolution": true,
        "content_fetch": true,
        "science_discovery": if attach_verified { "ATTACHED" } else { "FILES_VISIBLE_NOT_ATTACHED" },
        "skill_trigger": "NOT_VERIFIED",
        "function_run": "NOT_VERIFIED",
        "restart_required": false,
        "new_conversation_required": false,
        "import_origin_written": true,
        "attach_error": attach_error,
        "message": message
    })
}

fn attach_bundle_result_payload(context: &ScienceHostContext, commit: BundleCommit) -> Value {
    let attach = update_agent_skills(context, &commit.skill_names, &[], &commit.active_org);
    let (status, message, attach_required, attach_verified) = match attach.as_ref() {
        Ok(_) => (
            "BUNDLE_INSTALLED_ATTACHED",
            format!(
                "bundle 文件已安装，OPERON 已回读确认绑定 {} 个 Skill。",
                commit.skill_names.len()
            ),
            false,
            true,
        ),
        Err(error) if error.uncertain => (
            "ATTACH_STATE_UNCERTAIN",
            format!(
                "bundle 文件已安装，但 OPERON 批量绑定结果不确定：{}",
                error.message
            ),
            true,
            false,
        ),
        Err(error) => (
            "FILES_COMMITTED_ATTACH_REQUIRED",
            format!("bundle 文件已安装，但自动批量绑定失败：{}", error.message),
            true,
            false,
        ),
    };
    let attach_error = attach.as_ref().err().cloned();
    let skills = commit
        .members
        .iter()
        .map(|member| {
            json!({
                "skill_name": member.skill_name,
                "content_sha256": member.content_sha256,
                "install_action": member.install_action,
                "attach_verified": attach_verified,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "package_kind": "bundle",
        "bundle_id": commit.bundle_id,
        "bundle_name": commit.bundle_name,
        "skill_name": commit.skill_names.first(),
        "skill_names": commit.skill_names,
        "support_paths": commit.support_paths,
        "skills": skills,
        "source_kind": "local_zip",
        "directory_commit": commit.directory_commit,
        "install_action": commit.action.as_str(),
        "attach_attempted": true,
        "attach_required": attach_required,
        "attach_verified": attach_verified,
        "load_verification_required": false,
        "content_sha256": commit.bundle_content_sha256,
        "source_digest_sha256": commit.source_digest_sha256,
        "dependency_scan": "BEST_EFFORT",
        "agent_name": "OPERON",
        "attach_method": "csswitch_batch_auto_attach",
        "source_resolution": true,
        "content_fetch": true,
        "science_discovery": if attach_verified { "BATCH_ATTACHED" } else { "FILES_VISIBLE_NOT_ATTACHED" },
        "skill_trigger": "NOT_REQUIRED_FOR_BUNDLE_ACCEPTANCE",
        "function_run": "NOT_VERIFIED",
        "restart_required": false,
        "new_conversation_required": false,
        "import_origin_written": true,
        "attach_error": attach_error,
        "message": message
    })
}

fn install_error_payload(error: InstallError) -> Value {
    let status = match error.code.as_str() {
        "SKILL_NAME_CONFLICT"
        | "INSTALLED_CONTENT_CHANGED"
        | "UNSUPPORTED_SHARED_DEPENDENCY"
        | "LEGACY_INTEGRITY_UNVERIFIED"
        | "MULTIPLE_BUNDLE_CANDIDATES"
        | "BUNDLE_STRUCTURE_UNSUPPORTED"
        | "BUNDLE_LIMIT_EXCEEDED"
        | "BUNDLE_PATH_CONFLICT"
        | "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY" => error.code.as_str(),
        _ => "INSTALL_FAILED",
    };
    let message = error.message.clone();
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "skill_name": null,
        "source_kind": "local_zip",
        "directory_commit": error.directory_commit,
        "attach_attempted": false,
        "attach_required": false,
        "attach_verified": false,
        "load_verification_required": false,
        "content_sha256": null,
        "source_digest_sha256": null,
        "restart_required": false,
        "error": error,
        "message": message
    })
}

fn not_ready(message: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": "SCIENCE_NOT_READY",
        "skill_name": null,
        "source_kind": "local_zip",
        "directory_commit": false,
        "attach_attempted": false,
        "attach_required": false,
        "attach_verified": false,
        "load_verification_required": false,
        "content_sha256": null,
        "source_digest_sha256": null,
        "message": message
    })
}

fn local_error(code: &str, message: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": "INSTALL_FAILED",
        "skill_name": null,
        "source_kind": "local_zip",
        "directory_commit": false,
        "attach_attempted": false,
        "attach_required": false,
        "attach_verified": false,
        "load_verification_required": false,
        "content_sha256": null,
        "source_digest_sha256": null,
        "error": {"code": code, "message": message, "phase": "picker"},
        "message": message
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_ready_is_non_committing() {
        let value = not_ready("not running");
        assert_eq!(value["status"], "SCIENCE_NOT_READY");
        assert_eq!(value["directory_commit"], false);
    }
}
