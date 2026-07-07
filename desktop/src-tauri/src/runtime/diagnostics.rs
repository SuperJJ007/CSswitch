pub(crate) struct StatusProbeInput {
    pub(crate) proxy_ok: bool,
    pub(crate) sandbox_ok: bool,
    pub(crate) upstream_ok: bool,
}

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

#[cfg(test)]
mod tests {
    use super::{status_lights, StatusProbeInput};

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
}
