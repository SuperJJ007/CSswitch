use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{json, Value};

const STATIC_ROUTES_JSON: &str = include_str!("../../../catalog/opencode-go-model-routes.v1.json");
const OFFICIAL_SOURCE_URL: &str = "https://opencode.ai/docs/zh-cn/go/";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RouteSource {
    url: String,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ModelProtocolRoute {
    model_id: String,
    protocol: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RouteCatalog {
    schema_version: u32,
    catalog_id: String,
    source: RouteSource,
    routes: Vec<ModelProtocolRoute>,
}

fn load_catalog() -> Result<RouteCatalog, String> {
    let catalog: RouteCatalog = serde_json::from_str(STATIC_ROUTES_JSON)
        .map_err(|error| format!("OpenCode Go route catalog JSON 解析失败：{error}"))?;
    if catalog.schema_version != 1
        || catalog.catalog_id != "opencode-go-model-routes"
        || catalog.source.url != OFFICIAL_SOURCE_URL
        || catalog.source.updated_at != "2026-07-19"
        || catalog.routes.is_empty()
    {
        return Err("OpenCode Go route catalog 元数据非法".into());
    }
    let mut ids = BTreeSet::new();
    for route in &catalog.routes {
        if route.model_id.trim().is_empty()
            || route.model_id.starts_with("opencode-go/")
            || !ids.insert(route.model_id.as_str())
            || !matches!(route.protocol.as_str(), "openai_chat" | "anthropic")
        {
            return Err("OpenCode Go route catalog 含非法或重复路由".into());
        }
    }
    Ok(catalog)
}

fn protocol_for_template(template_id: &str) -> Option<&'static str> {
    match template_id {
        "opencode-go-openai" => Some("openai_chat"),
        "opencode-go-anthropic" => Some("anthropic"),
        _ => None,
    }
}

pub(crate) fn validate_model_for_template(template_id: &str, model_id: &str) -> Result<(), String> {
    let Some(protocol) = protocol_for_template(template_id) else {
        return Ok(());
    };
    if model_id.starts_with("opencode-go/") {
        return Err("OpenCode Go 必须填写并发送裸 model ID，不能使用 opencode-go/ 前缀".into());
    }
    let catalog = load_catalog()?;
    if let Some(route) = catalog
        .routes
        .iter()
        .find(|route| route.model_id == model_id)
    {
        if route.protocol != protocol {
            return Err(format!(
                "OpenCode Go 模型 `{model_id}` 的官方 transport 是 {}，不能用于 {protocol} 模板",
                route.protocol
            ));
        }
    }
    // Unknown IDs remain manually addable only because the caller already
    // selected one of the two explicit transport templates.
    Ok(())
}

/// 对 OpenCode Go 的 `/models` 结果应用官方 model_id→protocol 路由表。
/// 未知的 live 项默认不进入候选列表；已经由用户在明确 transport 模板中保存的
/// manual route 会保留，避免一次探测静默删除正式配置。
pub(crate) fn filter_discovery_response(
    template_id: &str,
    mut response: Value,
) -> Result<Value, String> {
    let Some(protocol) = protocol_for_template(template_id) else {
        return Ok(response);
    };
    let catalog = load_catalog()?;
    let allowed: BTreeSet<&str> = catalog
        .routes
        .iter()
        .filter(|route| route.protocol == protocol)
        .map(|route| route.model_id.as_str())
        .collect();
    let models = response
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or("模型发现响应缺少 models 数组")?;
    let before = models.len();
    let known: BTreeSet<&str> = catalog
        .routes
        .iter()
        .map(|route| route.model_id.as_str())
        .collect();
    models.retain(|model| {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("");
        let manual = model.get("origin").and_then(Value::as_str) == Some("manual");
        allowed.contains(id) || (manual && !known.contains(id) && !id.starts_with("opencode-go/"))
    });
    for model in models.iter_mut() {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("");
        model["route_known"] = json!(allowed.contains(id));
        model["transport"] = json!(protocol);
    }
    response["filtered_unknown_count"] = json!(before.saturating_sub(models.len()));
    response["route_catalog"] = json!({
        "id": catalog.catalog_id,
        "schema_version": catalog.schema_version,
        "source_url": catalog.source.url,
        "updated_at": catalog.source.updated_at,
        "transport": protocol,
    });
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_route_catalog_is_versioned_exact_and_uses_bare_ids() {
        let catalog = load_catalog().unwrap();
        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.source.url, OFFICIAL_SOURCE_URL);
        assert!(catalog.routes.iter().all(|route| {
            !route.model_id.starts_with("opencode-go/")
                && matches!(route.protocol.as_str(), "openai_chat" | "anthropic")
        }));
        assert_eq!(
            catalog
                .routes
                .iter()
                .filter(|route| route.protocol == "openai_chat")
                .count(),
            10
        );
        assert_eq!(
            catalog
                .routes
                .iter()
                .filter(|route| route.protocol == "anthropic")
                .count(),
            6
        );
    }

    #[test]
    fn discovery_intersects_protocol_and_keeps_only_explicit_manual_unknowns() {
        let response = json!({"models": [
            {"id": "kimi-k3", "origin": "discovered"},
            {"id": "minimax-m3", "origin": "discovered"},
            {"id": "future-model", "origin": "discovered"},
            {"id": "manual-future", "origin": "manual"},
            {"id": "minimax-m3", "origin": "manual"},
            {"id": "opencode-go/kimi-k3", "origin": "discovered"}
        ]});
        let filtered = filter_discovery_response("opencode-go-openai", response).unwrap();
        let models = filtered["models"].as_array().unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["kimi-k3", "manual-future"]
        );
        assert_eq!(models[0]["route_known"], true);
        assert_eq!(models[1]["route_known"], false);
        assert_eq!(filtered["filtered_unknown_count"], 4);
        assert_eq!(filtered["route_catalog"]["transport"], "openai_chat");
    }

    #[test]
    fn explicit_transport_allows_unknown_manual_ids_but_rejects_known_mismatches_and_prefixes() {
        assert!(validate_model_for_template("opencode-go-openai", "future-model").is_ok());
        assert!(validate_model_for_template("opencode-go-openai", "kimi-k3").is_ok());
        assert!(validate_model_for_template("opencode-go-openai", "minimax-m3").is_err());
        assert!(validate_model_for_template("opencode-go-anthropic", "kimi-k3").is_err());
        assert!(
            validate_model_for_template("opencode-go-anthropic", "opencode-go/minimax-m3").is_err()
        );
    }
}
