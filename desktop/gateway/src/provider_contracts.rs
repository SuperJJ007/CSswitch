use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const STATIC_PROVIDER_CONTRACTS_JSON: &str =
    include_str!("../../../catalog/provider-contracts.v1.json");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeoutPolicy {
    connect_ms: u64,
    total_ms: u64,
    read_idle_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePolicy {
    normal_ttl_seconds: u64,
    stale_ttl_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderContract {
    id: String,
    template_ids: Vec<String>,
    api_formats: Vec<String>,
    adapter: String,
    auth_mode: String,
    auth_scheme: String,
    credential_sources: Vec<String>,
    default_credential_source: String,
    model_policies: Vec<String>,
    default_model_policy: String,
    model_discovery: String,
    transport: String,
    endpoint_policy: String,
    endpoint_join: String,
    api_key_env: Option<String>,
    scratch_policy: String,
    thinking_policy: String,
    #[serde(default)]
    upstream_client_version: Option<String>,
    timeouts: TimeoutPolicy,
    cache: CachePolicy,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderContractCatalog {
    schema_version: u32,
    contracts: Vec<ProviderContract>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexRuntimeContract {
    pub contract_id: String,
    pub catalog_digest: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub read_idle_timeout: Duration,
    pub normal_ttl_seconds: u64,
    pub stale_ttl_seconds: u64,
    pub model_catalog_client_version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointJoin {
    ManagedOfficial,
    AnthropicV1,
    OpenaiV1,
    OpenaiPath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthScheme {
    AnthropicXApiKey,
    AnthropicDual,
    Bearer,
    CsswitchOauth,
}

impl AuthScheme {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "anthropic_x_api_key" => Ok(Self::AnthropicXApiKey),
            "anthropic_dual" => Ok(Self::AnthropicDual),
            "bearer" => Ok(Self::Bearer),
            "csswitch_oauth" => Ok(Self::CsswitchOauth),
            _ => Err("provider contract auth scheme is unsupported".into()),
        }
    }
}

impl EndpointJoin {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "managed_official" => Ok(Self::ManagedOfficial),
            "anthropic_v1" => Ok(Self::AnthropicV1),
            "openai_v1" => Ok(Self::OpenaiV1),
            "openai_path" => Ok(Self::OpenaiPath),
            _ => Err("provider contract endpoint join is unsupported".into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRuntimeContract {
    pub contract_id: String,
    pub catalog_digest: String,
    pub auth_mode: String,
    pub auth_scheme: AuthScheme,
    pub api_key_env: Option<String>,
    pub transport: String,
    pub endpoint_policy: String,
    pub endpoint_join: EndpointJoin,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub read_idle_timeout: Duration,
    pub normal_ttl_seconds: u64,
    pub stale_ttl_seconds: u64,
    pub upstream_client_version: Option<String>,
}

fn catalog_digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(STATIC_PROVIDER_CONTRACTS_JSON.as_bytes())
    )
}

fn parse_catalog() -> Result<ProviderContractCatalog, String> {
    let catalog: ProviderContractCatalog = serde_json::from_str(STATIC_PROVIDER_CONTRACTS_JSON)
        .map_err(|error| format!("provider contract catalog parse failed: {error}"))?;
    if catalog.schema_version != 1 || catalog.contracts.is_empty() {
        return Err("provider contract catalog schema is unsupported".into());
    }
    let mut ids = BTreeSet::new();
    for contract in &catalog.contracts {
        if contract.id.trim().is_empty() || !ids.insert(contract.id.as_str()) {
            return Err("provider contract catalog contains an invalid id".into());
        }
        if contract.timeouts.connect_ms == 0
            || contract.timeouts.total_ms < contract.timeouts.connect_ms
            || contract.timeouts.read_idle_ms == 0
            || contract.cache.stale_ttl_seconds < contract.cache.normal_ttl_seconds
        {
            return Err("provider contract catalog contains invalid runtime bounds".into());
        }
        EndpointJoin::parse(&contract.endpoint_join)?;
        AuthScheme::parse(&contract.auth_scheme)?;
        if contract.template_ids.is_empty()
            || contract.api_formats.is_empty()
            || contract.credential_sources.is_empty()
            || !contract
                .credential_sources
                .contains(&contract.default_credential_source)
            || contract.model_policies.is_empty()
            || !contract
                .model_policies
                .contains(&contract.default_model_policy)
            || contract.model_discovery.is_empty()
            || contract.scratch_policy.is_empty()
            || !matches!(
                contract.thinking_policy.as_str(),
                "" | "adaptive" | "enabled"
            )
        {
            return Err("provider contract catalog contains an invalid capability shape".into());
        }
    }
    Ok(catalog)
}

pub(crate) fn load_runtime_contract(
    provider: &str,
    expected_id: Option<&str>,
    expected_digest: Option<&str>,
) -> Result<ProviderRuntimeContract, String> {
    let catalog = parse_catalog()?;
    let digest = catalog_digest();
    let contract = match (expected_id, expected_digest) {
        (Some(id), Some(expected)) => {
            if expected != digest {
                return Err("managed provider contract identity mismatch".into());
            }
            catalog
                .contracts
                .iter()
                .find(|contract| contract.id == id)
                .ok_or("managed provider contract is unavailable")?
        }
        (None, None) => {
            let mut matches = catalog
                .contracts
                .iter()
                .filter(|contract| contract.adapter == provider);
            let first = matches.next().ok_or("provider contract is unavailable")?;
            if matches.any(|other| {
                other.auth_mode != first.auth_mode
                    || other.auth_scheme != first.auth_scheme
                    || other.api_key_env != first.api_key_env
                    || other.transport != first.transport
                    || other.endpoint_policy != first.endpoint_policy
                    || other.endpoint_join != first.endpoint_join
                    || other.timeouts.connect_ms != first.timeouts.connect_ms
                    || other.timeouts.total_ms != first.timeouts.total_ms
                    || other.timeouts.read_idle_ms != first.timeouts.read_idle_ms
            }) {
                return Err("provider contract identity is required for this adapter".into());
            }
            first
        }
        _ => return Err("managed provider contract identity is incomplete".into()),
    };
    if contract.adapter != provider {
        return Err("managed provider contract adapter mismatch".into());
    }
    Ok(ProviderRuntimeContract {
        contract_id: contract.id.clone(),
        catalog_digest: digest,
        auth_mode: contract.auth_mode.clone(),
        auth_scheme: AuthScheme::parse(&contract.auth_scheme)?,
        api_key_env: contract.api_key_env.clone(),
        transport: contract.transport.clone(),
        endpoint_policy: contract.endpoint_policy.clone(),
        endpoint_join: EndpointJoin::parse(&contract.endpoint_join)?,
        connect_timeout: Duration::from_millis(contract.timeouts.connect_ms),
        request_timeout: Duration::from_millis(contract.timeouts.total_ms),
        read_idle_timeout: Duration::from_millis(contract.timeouts.read_idle_ms),
        normal_ttl_seconds: contract.cache.normal_ttl_seconds,
        stale_ttl_seconds: contract.cache.stale_ttl_seconds,
        upstream_client_version: contract.upstream_client_version.clone(),
    })
}

pub(crate) fn codex_contract_from_runtime(
    runtime: &ProviderRuntimeContract,
) -> Result<CodexRuntimeContract, String> {
    if runtime.contract_id != "codex-oauth"
        || runtime.auth_mode != "csswitch_oauth"
        || runtime.auth_scheme != AuthScheme::CsswitchOauth
        || runtime.transport != "codex_responses_sse"
        || runtime.endpoint_policy != "gateway_managed_official"
        || runtime.endpoint_join != EndpointJoin::ManagedOfficial
        || runtime.api_key_env.is_some()
        || runtime.upstream_client_version.as_deref() != Some("0.144.4")
    {
        return Err("Codex provider contract is invalid".into());
    }
    Ok(CodexRuntimeContract {
        contract_id: runtime.contract_id.clone(),
        catalog_digest: runtime.catalog_digest.clone(),
        connect_timeout: runtime.connect_timeout,
        request_timeout: runtime.request_timeout,
        read_idle_timeout: runtime.read_idle_timeout,
        normal_ttl_seconds: runtime.normal_ttl_seconds,
        stale_ttl_seconds: runtime.stale_ttl_seconds,
        model_catalog_client_version: runtime
            .upstream_client_version
            .clone()
            .expect("validated Codex client version"),
    })
}

#[cfg(test)]
pub(crate) fn load_codex_runtime_contract() -> Result<CodexRuntimeContract, String> {
    let catalog = parse_catalog()?;
    let mut codex = catalog.contracts.iter().filter(|contract| {
        contract.id == "codex-oauth"
            || contract.adapter == "codex"
            || contract.auth_mode == "csswitch_oauth"
            || contract.auth_scheme == "csswitch_oauth"
            || contract.transport == "codex_responses_sse"
    });
    let contract = codex
        .next()
        .ok_or("Codex provider contract is unavailable")?;
    if codex.next().is_some()
        || contract.id != "codex-oauth"
        || contract.template_ids != ["codex"]
        || contract.api_formats != ["openai_responses"]
        || contract.adapter != "codex"
        || contract.auth_mode != "csswitch_oauth"
        || contract.auth_scheme != "csswitch_oauth"
        || contract.credential_sources != ["csswitch_oauth"]
        || contract.default_credential_source != "csswitch_oauth"
        || contract.model_policies != ["dynamic_catalog"]
        || contract.default_model_policy != "dynamic_catalog"
        || contract.model_discovery != "codex_account_catalog"
        || contract.transport != "codex_responses_sse"
        || contract.endpoint_policy != "gateway_managed_official"
        || contract.endpoint_join != "managed_official"
        || contract.api_key_env.is_some()
        || contract.scratch_policy != "gateway_owned_auth"
        || !contract.thinking_policy.is_empty()
        || contract.upstream_client_version.as_deref() != Some("0.144.4")
        || contract.timeouts.connect_ms == 0
        || contract.timeouts.total_ms < contract.timeouts.connect_ms
        || contract.timeouts.read_idle_ms == 0
        || contract.cache.stale_ttl_seconds < contract.cache.normal_ttl_seconds
    {
        return Err("Codex provider contract is invalid".into());
    }
    codex_contract_from_runtime(&load_runtime_contract(
        "codex",
        Some(&contract.id),
        Some(&catalog_digest()),
    )?)
}

#[cfg(test)]
pub(crate) fn validate_managed_identity(
    contract: &CodexRuntimeContract,
    expected_id: Option<&str>,
    expected_digest: Option<&str>,
) -> Result<(), String> {
    match (expected_id, expected_digest) {
        (None, None) => Ok(()),
        (Some(id), Some(digest))
            if id == contract.contract_id && digest == contract.catalog_digest =>
        {
            Ok(())
        }
        _ => Err("managed provider contract identity mismatch".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_codex_contract_drives_gateway_runtime_values() {
        let contract = load_codex_runtime_contract().unwrap();
        assert_eq!(contract.contract_id, "codex-oauth");
        assert_eq!(contract.catalog_digest.len(), 64);
        assert_eq!(contract.connect_timeout, Duration::from_secs(10));
        assert_eq!(contract.request_timeout, Duration::from_secs(30));
        assert_eq!(contract.read_idle_timeout, Duration::from_secs(300));
        assert_eq!(contract.normal_ttl_seconds, 300);
        assert_eq!(contract.stale_ttl_seconds, 86_400);
        assert_eq!(contract.model_catalog_client_version, "0.144.4");
    }

    #[test]
    fn managed_identity_is_optional_for_standalone_but_fail_closed_when_present() {
        let contract = load_codex_runtime_contract().unwrap();
        assert!(validate_managed_identity(&contract, None, None).is_ok());
        assert!(validate_managed_identity(
            &contract,
            Some(&contract.contract_id),
            Some(&contract.catalog_digest)
        )
        .is_ok());
        assert!(validate_managed_identity(&contract, Some("wrong"), None).is_err());
        assert!(
            validate_managed_identity(&contract, Some(&contract.contract_id), Some("wrong"))
                .is_err()
        );
    }

    #[test]
    fn managed_identity_selects_exact_non_codex_contract_and_rejects_cross_adapter_ids() {
        let digest = catalog_digest();
        let kimi =
            load_runtime_contract("relay", Some("kimi-anthropic-relay"), Some(&digest)).unwrap();
        assert_eq!(kimi.contract_id, "kimi-anthropic-relay");
        assert_eq!(kimi.endpoint_join, EndpointJoin::AnthropicV1);
        assert_eq!(kimi.transport, "anthropic_messages");

        let opencode =
            load_runtime_contract("relay", Some("opencode-go-anthropic"), Some(&digest)).unwrap();
        assert_eq!(opencode.auth_scheme, AuthScheme::Bearer);
        assert_eq!(opencode.endpoint_join, EndpointJoin::AnthropicV1);

        let gemini =
            load_runtime_contract("openai-custom", Some("gemini-openai-chat"), Some(&digest))
                .unwrap();
        assert_eq!(gemini.auth_scheme, AuthScheme::Bearer);
        assert_eq!(gemini.endpoint_join, EndpointJoin::OpenaiPath);

        assert!(load_runtime_contract(
            "openai-custom",
            Some("kimi-anthropic-relay"),
            Some(&digest),
        )
        .is_err());
        assert!(load_runtime_contract("relay", Some("kimi-anthropic-relay"), None).is_err());
        assert!(load_runtime_contract(
            "relay",
            Some("kimi-anthropic-relay"),
            Some(&"0".repeat(64)),
        )
        .is_err());
    }
}
