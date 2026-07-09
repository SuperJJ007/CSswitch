use serde_json::json;

pub(crate) struct StatusProbeInput {
    pub(crate) proxy_ok: bool,
    pub(crate) sandbox_ok: bool,
    pub(crate) upstream_ok: bool,
}

pub(crate) struct ScienceDiagnosticsInput {
    pub(crate) sandbox_port: u16,
    pub(crate) sandbox_ok: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct StatusLights {
    pub(crate) proxy: &'static str,
    pub(crate) sandbox: &'static str,
    pub(crate) upstream: &'static str,
}

fn light(ok: bool) -> &'static str {
    if ok {
        "green"
    } else {
        "amber"
    }
}

/// Preserve the current status contract: each light is either `green` or `amber`.
pub(crate) fn status_lights(input: StatusProbeInput) -> StatusLights {
    StatusLights {
        proxy: light(input.proxy_ok),
        sandbox: light(input.sandbox_ok),
        upstream: light(input.upstream_ok),
    }
}

pub(crate) fn science_diagnostics(input: ScienceDiagnosticsInput) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "sandbox": {
            "port": input.sandbox_port,
            "health": light(input.sandbox_ok),
        },
        "auth": {
            "mode": "virtual_oauth",
            "real_account_verified": false,
            "real_home_verified": false,
            "known_boundary_rule_ids": [
                "science.auth.virtual-oauth-scope-boundary",
                "science.auth.refresh-hardcoded-0_1_15",
            ],
        },
        "version": {
            "status_probe": "not_run_in_status_poll",
            "known_rule_ids": [
                "science.version.0_1_15_dev.route-diff",
                "science.auth.refresh-hardcoded-0_1_15",
            ],
            "note": "status() does not run claude-science binary/version probes; use isolated HOME and non-8765 ports before making Science-version or real-account claims.",
        },
    })
}

pub(crate) fn build_status_response(
    lights: StatusLights,
    active_profile: serde_json::Value,
    gateway_kind: &str,
    shim_mode: &str,
    catalog: serde_json::Value,
    science: serde_json::Value,
    last_error: Option<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "proxy": lights.proxy,
        "sandbox": lights.sandbox,
        "upstream": lights.upstream,
        "active_profile": active_profile,
        "runtime": {
            "gateway_kind": gateway_kind,
            "shim_mode": shim_mode,
        },
        "catalog": catalog,
        "science": science,
        "last_error": last_error.unwrap_or(serde_json::Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_status_response, science_diagnostics, status_lights, ScienceDiagnosticsInput,
        StatusProbeInput,
    };
    use serde_json::json;

    #[test]
    fn status_lights_map_bools_to_existing_strings() {
        let all_green = status_lights(StatusProbeInput {
            proxy_ok: true,
            sandbox_ok: true,
            upstream_ok: true,
        });
        assert_eq!(all_green.proxy, "green");
        assert_eq!(all_green.sandbox, "green");
        assert_eq!(all_green.upstream, "green");

        let all_amber = status_lights(StatusProbeInput {
            proxy_ok: false,
            sandbox_ok: false,
            upstream_ok: false,
        });
        assert_eq!(all_amber.proxy, "amber");
        assert_eq!(all_amber.sandbox, "amber");
        assert_eq!(all_amber.upstream, "amber");
    }

    #[test]
    fn status_response_preserves_legacy_lights_and_adds_route_contract() {
        let lights = status_lights(StatusProbeInput {
            proxy_ok: true,
            sandbox_ok: false,
            upstream_ok: true,
        });
        let v = build_status_response(
            lights,
            json!({
                "id": "p1",
                "name": "GLM",
                "template_id": "glm",
                "api_format": "anthropic",
                "model": "glm-5.2",
            }),
            "python",
            "off",
            json!({
                "schema_version": 1,
                "status": "loaded",
                "active_rules": [],
                "boundary_rules": [],
            }),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 8990,
                sandbox_ok: false,
            }),
            None,
        );
        assert_eq!(v["proxy"], "green");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "green");
        assert_eq!(v["active_profile"]["template_id"], "glm");
        assert_eq!(v["runtime"]["gateway_kind"], "python");
        assert_eq!(v["runtime"]["shim_mode"], "off");
        assert_eq!(v["catalog"]["schema_version"], 1);
        assert_eq!(v["science"]["schema_version"], 1);
        assert_eq!(v["science"]["sandbox"]["port"], 8990);
        assert_eq!(v["science"]["sandbox"]["health"], "amber");
        assert_eq!(v["science"]["auth"]["real_account_verified"], false);
        assert_eq!(
            v["science"]["version"]["known_rule_ids"][1],
            "science.auth.refresh-hardcoded-0_1_15"
        );
        assert!(v["last_error"].is_null());
    }

    #[test]
    fn status_response_can_surface_typed_last_error() {
        let v = build_status_response(
            status_lights(StatusProbeInput {
                proxy_ok: false,
                sandbox_ok: false,
                upstream_ok: false,
            }),
            serde_json::Value::Null,
            "python",
            "off",
            json!({"schema_version": 1}),
            science_diagnostics(ScienceDiagnosticsInput {
                sandbox_port: 8990,
                sandbox_ok: false,
            }),
            Some(json!({
                "type": "config_error",
                "message": "config unreadable",
            })),
        );
        assert_eq!(v["last_error"]["type"], "config_error");
        assert_eq!(v["last_error"]["message"], "config unreadable");
    }
}
