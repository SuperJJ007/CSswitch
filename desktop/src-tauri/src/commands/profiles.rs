use serde_json::json;
use tauri::State;

use crate::runtime::operation::{OperationKind, OperationStage, OperationTrace};
use crate::runtime::profile::{
    build_get_config, build_list_templates, clear_profile_key_inner, create_profile_inner,
    delete_profile_inner, nonactive_probe_verdict, probe_kind_for, update_profile_connection_inner,
    update_profile_metadata_inner, ConnectionEdit,
};
use crate::runtime::provider::{
    assert_format_supported, is_native_adapter, proxy_args_for,
    reject_openai_custom_anthropic_base, relay_missing_base_url, relay_missing_model,
    should_scratch_candidate,
};
use crate::runtime::system::{asset_root, kill_child};
use crate::runtime::transaction::{
    decide_switch, rollback_status_clause, skip_scratch_verify, SwitchOutcome,
};
use crate::{
    commands, config, lifecycle, lock, proc, run_blocking, scratch, templates, SharedAppState,
    SharedLifecycle,
};

#[tauri::command]
pub(crate) fn get_config() -> Result<serde_json::Value, String> {
    build_get_config(&config::default_dir())
}

/// 模板注册表交前端铺 UI（新建向导用）。
#[tauri::command]
pub(crate) fn list_templates() -> Vec<serde_json::Value> {
    build_list_templates()
}

// ---------- profile CRUD 命令（薄包装 *_inner，统一经串行器） ----------
#[tauri::command]
pub(crate) fn create_profile(
    lifecycle: State<'_, SharedLifecycle>,
    template_id: String,
    name: String,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<String, String> {
    lifecycle.with_serialized(|| {
        create_profile_inner(
            &config::default_dir(),
            &template_id,
            &name,
            key.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
        )
    })
}

#[tauri::command]
pub(crate) fn update_profile_metadata(
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    name: String,
    notes: Option<String>,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        update_profile_metadata_inner(&config::default_dir(), &id, &name, notes.as_deref())
    })
}

/// 清 key：经串行器；若清的是【生效】profile → bump_generation 作废在途启动 + 停运行中代理
/// （不再拿旧 key 服务，比照 spec §8.2 运行态撤销）。
#[tauri::command]
pub(crate) fn clear_profile_key(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let was_active = config::load_from(&dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        clear_profile_key_inner(&dir, &id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(state.inner());
            kill_child(&mut st.proxy);
            st.provider.clear();
            st.key_fp = 0;
        }
        Ok(())
    })
}

/// 删 profile：经串行器；删的是【生效】profile → active 置空（inner 内）+ bump + 停代理。
#[tauri::command]
pub(crate) fn delete_profile(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let was_active = config::load_from(&dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        delete_profile_inner(&dir, &id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(state.inner());
            kill_child(&mut st.proxy);
            st.provider.clear();
            st.key_fp = 0;
        }
        Ok(())
    })
}

/// 对候选连接做一次上游 scratch 校验（非 active 编辑用，P2-d）。起临时代理探完即杀，
/// **绝不碰 config / AppState / 正在服务的正式代理**。返回是否【已通过上游校验】（供调用方据实措辞）：
/// 空 key / relay 家族空 base_url → `Ok(false)`（无从预检，标记未校验）；
/// native(deepseek/qwen) 即便 base_url 空也【会】走各自官方端点探测（修真机 P1）；
/// 明确接受(200) → `Ok(true)`；明确拒绝 → `Err(hint)`；无法确认 → `Ok(false)`（见 [`nonactive_probe_verdict`]）。
fn scratch_validate_candidate(
    app: &tauri::AppHandle,
    candidate: &config::Profile,
) -> Result<bool, String> {
    let launch = proxy_args_for(candidate);
    let trace = OperationTrace::start(
        OperationKind::ValidateConnection,
        format!(
            "profile_id={} template_id={} adapter={}",
            candidate.id, candidate.template_id, launch.adapter
        ),
    );
    if !should_scratch_candidate(&launch.adapter, &launch.key, &launch.base_url) {
        trace.finish("skipped reason=missing_key_or_base");
        return Ok(false); // 跳过 = 未校验（如实标记）
    }
    let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
    let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
    let script = root.join("proxy/csswitch_proxy.py");
    let res = scratch::scratch_probe(
        &py,
        &script,
        &scratch::ScratchTarget {
            provider: &launch.adapter,
            key_env: launch.key_env,
            base_url: &launch.base_url,
            key: &launch.key,
            model: Some(&launch.model),
            relay_thinking: launch.thinking_policy,
        },
        probe_kind_for(&launch.adapter, &launch.model),
        Some(&trace),
    );
    let outcome = scratch::classify(res.status);
    trace.finish(format!("outcome={outcome:?}"));
    nonactive_probe_verdict(&outcome)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn update_profile_connection(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        update_profile_connection_inner_cmd(
            app, state, lifecycle, id, base_url, api_format, model, key,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn update_profile_connection_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
        // 未命中 id → Err（不静默 Ok）。
        let mut candidate = cfg
            .profile_by_id(&id)
            .cloned()
            .ok_or_else(|| format!("找不到 profile：{id}"))?;
        // 生效【后】的候选连接（None=不改则沿用旧值），active/非 active 共用一份。
        let edit = ConnectionEdit::new(
            base_url.clone(),
            api_format.clone(),
            model.clone(),
            key.clone(),
        );
        edit.apply(&mut candidate);
        reject_openai_custom_anthropic_base(&candidate.template_id, &candidate.base_url)?;
        // 保存前守卫（修 P2）：relay/自定义端点清空 base_url → 不可用连接（激活必失败）。
        // 校验生效后的 base_url，空则拒绝落盘、绝不谎报「已保存」；native 走硬编码端点，空无妨。
        if relay_missing_base_url(
            templates::adapter_for(&candidate.template_id),
            &candidate.base_url,
        ) {
            return Err("中转 / 自定义端点必须填写连接地址（base_url），连接未保存。".to_string());
        }
        // 保存前守卫（修 #9 P1-a）：relay/自定义端点空 model → 无 force → 退回 passthrough（显示 claude）。
        if relay_missing_model(
            templates::adapter_for(&candidate.template_id),
            &candidate.model,
        ) {
            return Err("中转 / 自定义端点必须选择或填写一个模型，连接未保存。".to_string());
        }
        if cfg.active_id == id {
            // active（有正在服务的代理）：validate-before-persist —— 新连接作【内存候选】喂进
            // 切换事务（校验→起正式→健康），探活健康【才】连同落盘；失败则磁盘连接零改动、
            // 仍跑旧连接（杜绝「盘新运行旧」，修 P1-4）。
            let v =
                set_active_profile_txn(&app, &state, lifecycle.as_ref(), &id, false, Some(&edit))?;
            // 连接编辑：committed:false（scratch 分类失败）也如实作为错误上抛（磁盘未改、代理仍跑旧的）。
            if v.get("committed").and_then(|b| b.as_bool()) == Some(false) {
                let hint = v
                    .get("hint")
                    .and_then(|h| h.as_str())
                    .unwrap_or("连接校验未通过，连接未保存。")
                    .to_string();
                return Err(hint);
            }
            // active：已连同起正式代理探活并落盘，视为已校验。
            Ok(json!({ "validated": true }))
        } else {
            // 非 active：无正在服务的代理。先对候选做上游 scratch 校验（仅明确拒绝才拦，其余
            // best-effort 落盘并如实标记「未校验」，修 P2-d：贴合设计「校验候选后提交」+ 如实报告），
            // 再落盘（inner 内含格式门 + 覆盖前留底）。
            let validated = scratch_validate_candidate(&app, &candidate)?;
            update_profile_connection_inner(
                &dir,
                &id,
                base_url.as_deref(),
                api_format.as_deref(),
                model.as_deref(),
                key.as_deref(),
            )?;
            Ok(json!({ "validated": validated }))
        }
    })
}

/// 一键切生效 profile：经串行器走 [`set_active_profile_txn`] 切换事务。
#[tauri::command]
pub(crate) async fn set_active_profile(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_active_profile_inner_cmd(app, state, lifecycle, id, skip_verify)).await
}

fn set_active_profile_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        set_active_profile_txn(&app, &state, lifecycle.as_ref(), &id, skip_verify, None)
    })
}

/// 切换事务本体（spec §7）：scratch 校验候选 → 起正式代理探活 → 探活健康【才】提交 active_id；
/// 任一步失败杀候选 + 恢复旧代理，`active_id` 不动，**不停沙箱**（path-secret 持久，端口+secret
/// 不变，沙箱链路不断，停沙箱只会扩大失败面）。**本函数不取串行器锁**（调用方命令已持有）。
fn set_active_profile_txn(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    skip_verify: bool,
    conn_edit: Option<&ConnectionEdit>,
) -> Result<serde_json::Value, String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let mut candidate = cfg
        .profile_by_id(id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    // active 连接编辑：把新连接字段套到【内存候选】做校验（validate-before-persist）——
    // 磁盘此刻仍是旧连接；只有探活健康提交时才落盘（见下方 Commit 分支）。
    if let Some(edit) = conn_edit {
        edit.apply(&mut candidate);
    }
    reject_openai_custom_anthropic_base(&candidate.template_id, &candidate.base_url)?;
    let is_edit = conn_edit.is_some();
    // 失败措辞：连接编辑说「未保存/仍在用原配置运行」，普通切换说「未切换/当前配置不变」。
    let (verb, tail): (&str, &str) = if is_edit {
        ("未保存", "仍在用原配置运行")
    } else {
        ("未切换", "当前配置不变")
    };
    assert_format_supported(&candidate)?;
    let launch = proxy_args_for(&candidate);
    if launch.key.is_empty() {
        return Err(format!("「{}」还没填 API key，请先填写。", candidate.name));
    }
    let native = is_native_adapter(&launch.adapter);
    if !native && launch.base_url.is_empty() {
        return Err("该配置需要填 base_url（http:// 或 https:// 开头）。".into());
    }
    // 守卫（修 #9 P1-a）：relay/自定义端点空 model 无法激活（无 force → 退回 passthrough 显示 claude）。
    if relay_missing_model(&launch.adapter, &candidate.model) {
        return Err(
            "该配置需要选择或填写一个模型（中转/自定义端点必填），请在连接编辑里补上。".into(),
        );
    }
    // 快照旧 active（回滚锚点）：旧 profile 仍在盘上未动、active_id 未改，恢复据它重起旧代理。
    let old_active = cfg.active_id.clone();
    let trace = OperationTrace::start(
        if is_edit {
            OperationKind::UpdateActiveConnection
        } else {
            OperationKind::ActivateProfile
        },
        format!(
            "profile_id={} template_id={} adapter={} skip_verify={}",
            candidate.id, candidate.template_id, launch.adapter, skip_verify
        ),
    );

    // 1) scratch 校验候选（临时端口+secret+候选 key，避开 8765；绝不碰正式链路）。
    //    所有 adapter 都预检：native(deepseek/qwen) 用各自官方端点 + Message 探测（其 /v1/models 静态，
    //    探不出坏 key）；只有用户显式 skip_verify 才跳过（修真机 P1：原生免校验会让无效 key 提交为
    //    active 并谎报「已切到」，首个真实推理才 401）。分类失败保留结构化提示（committed:false/can_skip）。
    let scratch_ok = if skip_scratch_verify(native, skip_verify) {
        trace.stage(OperationStage::ScratchUpstreamProbe, "skipped_by_user");
        true
    } else {
        let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
        let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
        let script = root.join("proxy/csswitch_proxy.py");
        let res = scratch::scratch_probe(
            &py,
            &script,
            &scratch::ScratchTarget {
                provider: &launch.adapter,
                key_env: launch.key_env,
                base_url: &launch.base_url,
                key: &launch.key,
                model: Some(&launch.model),
                relay_thinking: launch.thinking_policy,
            },
            probe_kind_for(&launch.adapter, &launch.model),
            Some(&trace),
        );
        let outcome = scratch::classify(res.status);
        trace.stage(
            OperationStage::ScratchUpstreamProbe,
            format!("outcome={outcome:?}"),
        );
        match outcome {
            scratch::ProbeOutcome::Ok => true,
            scratch::ProbeOutcome::Auth(code) => {
                trace.finish(format!("rejected status={code}"));
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝（{code}），key/权限有误，{verb}（{tail}）。") }));
            }
            scratch::ProbeOutcome::ModelError(code) => {
                trace.finish(format!("model_error status={code}"));
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝该模型（{code}），{verb}。请换一个模型或核对 base_url。") }));
            }
            scratch::ProbeOutcome::Ambiguous(_)
            | scratch::ProbeOutcome::NoResponse
            | scratch::ProbeOutcome::Unsupported(_) => {
                trace.finish("ambiguous can_skip=true");
                return Ok(json!({ "committed": false, "can_skip": true,
                    "hint": format!("无法确认（网络/上游繁忙），{verb}。可重试，或用「跳过验证」。") }));
            }
        }
    };

    // 2/3) 用候选起【正式代理】并探活。bump_generation 使并发中的旧启动（如同时的 verify_key）作废。
    lifecycle.bump_generation();
    let real_healthy = scratch_ok
        && commands::runtime::start_proxy_for(app, state, lifecycle, &candidate, Some(&trace))
            .is_ok();

    match decide_switch(scratch_ok, real_healthy) {
        SwitchOutcome::Commit => {
            // 探活健康【才】落盘：连接编辑连同 active_id 一起提交（validate-before-persist），
            // 盘上与运行态一致，杜绝「盘新运行旧」。
            if is_edit {
                config::write_rolling_backup(&dir).ok(); // 覆盖连接前留底（仅编辑路径需要）
            }
            if let Err(e) = config::update(&dir, |c| {
                c.active_id = id.to_string();
                if let Some(edit) = conn_edit {
                    if let Some(p) = c.profile_by_id_mut(id) {
                        edit.apply(p);
                    }
                }
            }) {
                // spec §7 步 5：config 提交失败也要回滚进程——正式代理已起，若不回滚就成「运行新/盘旧」。
                // 恢复旧 active 代理，active_id 仍为旧值，用户可重试。
                trace.stage(OperationStage::Rollback, "reason=config_write_failed");
                let restored = restore_proxy_for_active(app, state, lifecycle, &cfg, &old_active);
                trace.finish(format!("error=config_write_failed restored={restored}"));
                return Err(format!(
                    "校验通过、代理已起，但写盘失败（{e}），{}。请检查磁盘空间/权限后重试。",
                    rollback_status_clause(restored)
                ));
            }
            let hint = if is_edit {
                format!("已保存并应用「{}」的新连接。", candidate.name)
            } else {
                format!("已切到「{}」。", candidate.name)
            };
            trace.stage(OperationStage::Commit, "ok");
            trace.finish("committed=true");
            Ok(json!({ "committed": true, "active_id": id, "hint": hint }))
        }
        SwitchOutcome::RollbackToOld => {
            // 候选正式代理起/探活失败：恢复旧代理，active_id 不动，连接不落盘，不停沙箱。
            trace.stage(OperationStage::Rollback, "reason=proxy_unhealthy");
            let restored = restore_proxy_for_active(app, state, lifecycle, &cfg, &old_active);
            let clause = rollback_status_clause(restored);
            trace.finish(format!("rollback restored={restored}"));
            if is_edit {
                Err(format!(
                    "连接已校验通过，但正式代理启动/探活失败，连接未保存，{clause}。"
                ))
            } else {
                Err(format!(
                    "候选配置校验通过，但正式代理启动/探活失败，{clause}。"
                ))
            }
        }
        SwitchOutcome::AbortBeforeStart => {
            // scratch 校验未过；旧态零改动、连接不落盘。（明确拒绝/含糊态在上面已 committed:false 早返，
            // 此分支是 scratch_ok=false 的兜底措辞。）
            trace.finish("aborted_before_start");
            if is_edit {
                Err("连接上游校验失败（key/base_url/网络？），连接未保存。".into())
            } else {
                Err("候选上游校验失败（key/base_url/网络？），未切换。".into())
            }
        }
    }
}

/// 切换失败回滚：按【旧 active】重起旧代理（旧 profile 仍在盘上）；best-effort，失败则代理暂停、
/// active_id 仍为旧值，用户可重试。旧 active 为空（此前未配置生效）→ 不复活，保持代理停着。
/// 返回是否已把旧代理恢复到位（供调用方据实措辞，修 P2-e）。
fn restore_proxy_for_active(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    cfg: &config::Config,
    old_active: &str,
) -> bool {
    if old_active.is_empty() {
        return true; // 此前无生效配置 → 本就无代理可恢复，状态与切换前一致
    }
    match cfg.profile_by_id(old_active) {
        Some(old) => {
            lifecycle.bump_generation();
            commands::runtime::start_proxy_for(app, state, lifecycle, old, None).is_ok()
        }
        None => false, // 旧 active 指向已不存在的 profile（罕见）→ 无法恢复，代理已停
    }
}
