use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::runtime::capability_catalog::{validate_catalog, CapabilityCatalog, CatalogRule};

use super::model::{
    CompatibilityAcknowledgment, DeploymentStatus, DiscoveryStatus, InstalledSkill,
    ValidationStatus,
};
use super::requirements::{
    validate_binary, validate_environment, validate_runtime_asset, RequirementFlag,
    RequirementSource, SkillRequirements,
};

const MAX_CONTEXT_ITEMS: usize = 256;
const MAX_CONTEXT_ITEM_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityAvailability {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BooleanCapability {
    True,
    False,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SandboxState {
    Ready,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkMode {
    Direct,
    Inherit,
    HttpProxy,
    Socks5,
    VpnTun,
    Gateway,
    Offline,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalCommandPolicy {
    Allowed,
    Denied,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SshCapabilitySummary {
    pub(crate) transport: CapabilityAvailability,
    pub(crate) agent_visible: BooleanCapability,
    pub(crate) config_available: BooleanCapability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) science_version: Option<String>,
    pub(crate) platform: String,
    pub(crate) sandbox_state: SandboxState,
    pub(crate) deployment_status: DeploymentStatus,
    pub(crate) discovery_status: DiscoveryStatus,
    pub(crate) network_mode: NetworkMode,
    pub(crate) network: CapabilityAvailability,
    pub(crate) mcp: CapabilityAvailability,
    pub(crate) local_command_policy: LocalCommandPolicy,
    pub(crate) ssh: SshCapabilitySummary,
    pub(crate) available_binaries: BTreeSet<String>,
    pub(crate) binary_inventory: CapabilityAvailability,
    pub(crate) available_environment: BTreeSet<String>,
    pub(crate) environment_inventory: CapabilityAvailability,
    pub(crate) available_runtime_assets: BTreeSet<String>,
    pub(crate) runtime_asset_inventory: CapabilityAvailability,
}

impl RuntimeContext {
    pub(crate) fn validate(&self) -> Result<(), CompatibilityError> {
        if !matches!(self.platform.as_str(), "macos" | "linux" | "windows") {
            return Err(CompatibilityError::invalid_context(
                "runtime platform is not a supported public identifier",
            ));
        }
        if let Some(version) = &self.science_version {
            if !safe_version_text(version) {
                return Err(CompatibilityError::invalid_context(
                    "Science version is not a safe public version string",
                ));
            }
        }
        if self.network_mode == NetworkMode::Offline
            && self.network != CapabilityAvailability::Unavailable
        {
            return Err(CompatibilityError::invalid_context(
                "offline network mode must be marked unavailable",
            ));
        }
        validate_name_set(&self.available_binaries, validate_binary, "binary")?;
        validate_name_set(
            &self.available_environment,
            validate_environment,
            "environment",
        )?;
        for (availability, names, label) in [
            (self.binary_inventory, &self.available_binaries, "binary"),
            (
                self.environment_inventory,
                &self.available_environment,
                "environment",
            ),
            (
                self.runtime_asset_inventory,
                &self.available_runtime_assets,
                "runtime asset",
            ),
        ] {
            if matches!(
                availability,
                CapabilityAvailability::Unknown | CapabilityAvailability::Unavailable
            ) && !names.is_empty()
            {
                return Err(CompatibilityError::invalid_context(format!(
                    "unknown or unavailable {label} inventory must not contain names"
                )));
            }
        }
        validate_name_set(
            &self.available_runtime_assets,
            validate_runtime_asset,
            "runtime asset",
        )?;
        Ok(())
    }

    pub(crate) fn fingerprint(&self) -> Result<String, CompatibilityError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|_| {
            CompatibilityError::invalid_context("runtime context could not be serialized")
        })?;
        Ok(hex_digest(Sha256::digest(encoded)))
    }

    /// Compatibility acknowledgments deliberately exclude deployment/discovery lifecycle state.
    /// Those fields change as reconcile progresses and must not invalidate a capability decision.
    pub(crate) fn capability_projection(&self) -> Self {
        let mut projected = self.clone();
        projected.deployment_status = DeploymentStatus::Deployed;
        projected.discovery_status = DiscoveryStatus::Discovered;
        projected.sandbox_state = SandboxState::Ready;
        projected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibilityStatus {
    Supported,
    Limited,
    Unsupported,
    Unknown,
}

impl CompatibilityStatus {
    fn severity(self) -> u8 {
        match self {
            Self::Supported => 0,
            Self::Limited => 1,
            Self::Unknown => 2,
            Self::Unsupported => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibilityAction {
    None,
    Document,
    Degrade,
    Diagnose,
    Disable,
}

impl CompatibilityAction {
    fn priority(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Document => 1,
            Self::Degrade => 2,
            Self::Diagnose => 3,
            Self::Disable => 4,
        }
    }

    fn next_step(self) -> &'static str {
        match self {
            Self::None => "No compatibility action is required.",
            Self::Document => "Review the documented limitation before enabling this Skill.",
            Self::Degrade => "Confirm the limited mode before enabling this Skill.",
            Self::Diagnose => {
                "Run the capability diagnostic and confirm the result before enabling this Skill."
            }
            Self::Disable => "Keep this Skill disabled until the missing capability is satisfied.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityDiagnostic {
    pub(crate) rule_id: String,
    pub(crate) reason: String,
    pub(crate) next_step: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityVerdict {
    pub(crate) status: CompatibilityStatus,
    pub(crate) action: CompatibilityAction,
    pub(crate) matched_rule_ids: Vec<String>,
    pub(crate) diagnostics: Vec<CompatibilityDiagnostic>,
    pub(crate) runtime_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityGate {
    pub(crate) full_verdict: CompatibilityVerdict,
    pub(crate) capability_verdict: CompatibilityVerdict,
    pub(crate) required_rule_ids: Vec<String>,
    pub(crate) capability_rule_ids: Vec<String>,
    pub(crate) capability_fingerprint: String,
    pub(crate) acknowledgment_satisfied: bool,
}

pub(crate) fn evaluate_compatibility_gate(
    skill: &InstalledSkill,
    context: &RuntimeContext,
    catalog: &CapabilityCatalog,
) -> Result<CompatibilityGate, CompatibilityError> {
    let full_verdict = evaluate_skill_compatibility(skill, context, catalog)?;
    let capability_verdict =
        evaluate_skill_compatibility(skill, &context.capability_projection(), catalog)?;
    let capability_fingerprint = capability_verdict.runtime_fingerprint.clone();
    let mut required_rule_ids = match full_verdict.status {
        CompatibilityStatus::Supported => Vec::new(),
        CompatibilityStatus::Limited | CompatibilityStatus::Unknown => full_verdict
            .matched_rule_ids
            .iter()
            .filter(|rule_id| rule_id.as_str() != "skill.baseline.satisfied")
            .cloned()
            .collect(),
        CompatibilityStatus::Unsupported => Vec::new(),
    };
    required_rule_ids.sort();
    required_rule_ids.dedup();
    let mut capability_rule_ids = match capability_verdict.status {
        CompatibilityStatus::Supported | CompatibilityStatus::Unsupported => Vec::new(),
        CompatibilityStatus::Limited | CompatibilityStatus::Unknown => capability_verdict
            .matched_rule_ids
            .iter()
            .filter(|rule_id| rule_id.as_str() != "skill.baseline.satisfied")
            .cloned()
            .collect(),
    };
    capability_rule_ids.sort();
    capability_rule_ids.dedup();
    let acknowledgment_satisfied = if capability_rule_ids.is_empty() {
        capability_verdict.status == CompatibilityStatus::Supported
    } else {
        skill
            .compatibility_acknowledgment
            .as_ref()
            .is_some_and(|acknowledgment| {
                acknowledgment.capability_rule_ids == capability_rule_ids
                    && acknowledgment.capability_fingerprint == capability_fingerprint
            })
    };
    Ok(CompatibilityGate {
        full_verdict,
        capability_verdict,
        required_rule_ids,
        capability_rule_ids,
        capability_fingerprint,
        acknowledgment_satisfied,
    })
}

pub(crate) fn acknowledgment_for(
    gate: &CompatibilityGate,
    acknowledged_rule_ids: &[String],
    acknowledged_at: u64,
) -> Result<Option<CompatibilityAcknowledgment>, CompatibilityError> {
    if gate.full_verdict.status == CompatibilityStatus::Unsupported
        || gate.capability_verdict.status == CompatibilityStatus::Unsupported
    {
        return Ok(None);
    }
    if acknowledged_rule_ids != gate.required_rule_ids || acknowledged_at == 0 {
        return Err(CompatibilityError::acknowledgment_required());
    }
    if gate.required_rule_ids.is_empty() && gate.capability_rule_ids.is_empty() {
        return Ok(None);
    }
    let acknowledgment = CompatibilityAcknowledgment {
        capability_rule_ids: gate.capability_rule_ids.clone(),
        last_action_rule_ids: gate.required_rule_ids.clone(),
        capability_fingerprint: gate.capability_fingerprint.clone(),
        acknowledged_at,
    };
    acknowledgment
        .validate()
        .map_err(|_| CompatibilityError::acknowledgment_required())?;
    Ok(Some(acknowledgment))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum CompatibilityErrorCode {
    InvalidSkill,
    InvalidRuntimeContext,
    InvalidCatalog,
    MissingCatalogRule,
    AcknowledgmentRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityError {
    pub(crate) code: CompatibilityErrorCode,
    pub(crate) message: String,
    pub(crate) remediation: String,
}

impl CompatibilityError {
    fn invalid_context(message: impl Into<String>) -> Self {
        Self {
            code: CompatibilityErrorCode::InvalidRuntimeContext,
            message: message.into(),
            remediation: "Refresh the safe runtime capability summary and try again.".to_string(),
        }
    }

    fn invalid_catalog() -> Self {
        Self {
            code: CompatibilityErrorCode::InvalidCatalog,
            message: "The Skill compatibility catalog is invalid.".to_string(),
            remediation: "Repair or restore the bundled capability catalog before enabling Skills."
                .to_string(),
        }
    }

    fn missing_rule() -> Self {
        Self {
            code: CompatibilityErrorCode::MissingCatalogRule,
            message:
                "The Skill compatibility catalog does not cover the current requirement state."
                    .to_string(),
            remediation: "Update the bundled capability catalog before enabling this Skill."
                .to_string(),
        }
    }

    fn acknowledgment_required() -> Self {
        Self {
            code: CompatibilityErrorCode::AcknowledgmentRequired,
            message: "The current compatibility warnings require explicit acknowledgment."
                .to_string(),
            remediation: "Evaluate compatibility again and acknowledge the exact current rule IDs."
                .to_string(),
        }
    }
}

impl std::fmt::Display for CompatibilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for CompatibilityError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Condition {
    requirement: &'static str,
    state: &'static str,
}

const REQUIRED_SKILL_CONDITIONS: &[(&str, &str)] = &[
    ("baseline", "satisfied"),
    ("sandbox", "unavailable"),
    ("sandbox", "unknown"),
    ("deployment", "not_deployed"),
    ("deployment", "pending"),
    ("deployment", "restart_required"),
    ("deployment", "failed"),
    ("discovery", "unknown"),
    ("discovery", "not_discovered"),
    ("network", "requirement_unknown"),
    ("network", "availability_unknown"),
    ("network", "unavailable"),
    ("ssh", "requirement_unknown"),
    ("ssh", "availability_unknown"),
    ("ssh", "unavailable"),
    ("mcp", "requirement_unknown"),
    ("mcp", "availability_unknown"),
    ("mcp", "unavailable"),
    ("local_command", "requirement_unknown"),
    ("local_command", "availability_unknown"),
    ("local_command", "unavailable"),
    ("binary", "requirement_unknown"),
    ("binary", "availability_unknown"),
    ("binary", "missing"),
    ("environment", "requirement_unknown"),
    ("environment", "availability_unknown"),
    ("environment", "missing"),
    ("runtime_asset", "requirement_unknown"),
    ("runtime_asset", "availability_unknown"),
    ("runtime_asset", "missing"),
    ("platform", "requirement_unknown"),
    ("platform", "mismatch"),
    ("minimum_runtime_version", "requirement_unknown"),
    ("minimum_runtime_version", "missing_runtime_version"),
    ("minimum_runtime_version", "unparseable"),
    ("minimum_runtime_version", "not_met"),
];

impl Condition {
    const fn new(requirement: &'static str, state: &'static str) -> Self {
        Self { requirement, state }
    }
}

#[derive(Clone)]
struct ParsedRule<'a> {
    rule: &'a CatalogRule,
    status: CompatibilityStatus,
    action: CompatibilityAction,
}

pub(crate) fn evaluate_skill_compatibility(
    skill: &InstalledSkill,
    context: &RuntimeContext,
    catalog: &CapabilityCatalog,
) -> Result<CompatibilityVerdict, CompatibilityError> {
    skill.validate().map_err(|_| CompatibilityError {
        code: CompatibilityErrorCode::InvalidSkill,
        message: "The installed Skill record is invalid.".to_string(),
        remediation: "Repair the Skill inventory before evaluating compatibility.".to_string(),
    })?;
    if skill.validation_status != ValidationStatus::Valid {
        return Err(CompatibilityError {
            code: CompatibilityErrorCode::InvalidSkill,
            message: "The installed Skill has not passed validation.".to_string(),
            remediation: "Reinspect or reinstall the Skill before evaluating compatibility."
                .to_string(),
        });
    }
    let runtime_fingerprint = context.fingerprint()?;
    let rules = validated_skill_rules(catalog)?;
    let conditions = conditions_for(&skill.requirements, context);

    let mut matched = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let Some(rule) = rules.get(&condition) else {
            return Err(CompatibilityError::missing_rule());
        };
        matched.push(rule.clone());
    }
    matched.sort_by(|left, right| {
        right
            .status
            .severity()
            .cmp(&left.status.severity())
            .then_with(|| right.action.priority().cmp(&left.action.priority()))
            .then_with(|| left.rule.id.cmp(&right.rule.id))
    });

    let status = matched
        .iter()
        .map(|rule| rule.status)
        .max_by_key(|status| status.severity())
        .unwrap_or(CompatibilityStatus::Unknown);
    let action = matched
        .iter()
        .filter(|rule| rule.status == status)
        .map(|rule| rule.action)
        .max_by_key(|action| action.priority())
        .unwrap_or(CompatibilityAction::Diagnose);
    let matched_rule_ids = matched.iter().map(|rule| rule.rule.id.clone()).collect();
    let diagnostics = matched
        .into_iter()
        .map(|rule| CompatibilityDiagnostic {
            rule_id: rule.rule.id.clone(),
            reason: rule.rule.reason.clone(),
            next_step: rule.action.next_step().to_string(),
        })
        .collect();

    Ok(CompatibilityVerdict {
        status,
        action,
        matched_rule_ids,
        diagnostics,
        runtime_fingerprint,
    })
}

pub(crate) fn validate_skill_catalog_rules(
    catalog: &CapabilityCatalog,
) -> Result<(), CompatibilityError> {
    let rules = validated_skill_rules(catalog)?;
    let required = REQUIRED_SKILL_CONDITIONS
        .iter()
        .map(|(requirement, state)| Condition::new(requirement, state))
        .collect::<BTreeSet<_>>();
    if rules.keys().cloned().collect::<BTreeSet<_>>() != required {
        return Err(CompatibilityError::missing_rule());
    }
    Ok(())
}

fn validated_skill_rules(
    catalog: &CapabilityCatalog,
) -> Result<BTreeMap<Condition, ParsedRule<'_>>, CompatibilityError> {
    validate_catalog(catalog).map_err(|_| CompatibilityError::invalid_catalog())?;
    let mut parsed = BTreeMap::new();
    for rule in &catalog.skills {
        if rule.scope != "skill"
            || !valid_rule_id(&rule.id)
            || !public_catalog_text_is_safe(&rule.reason, 512)
            || rule
                .evidence
                .iter()
                .chain(&rule.tests)
                .any(|text| !safe_skill_reference(text))
            || rule.match_fields.len() != 2
        {
            return Err(CompatibilityError::invalid_catalog());
        }
        let Some(requirement) = rule.match_fields.get("requirement").and_then(Value::as_str) else {
            return Err(CompatibilityError::invalid_catalog());
        };
        let Some(state) = rule.match_fields.get("state").and_then(Value::as_str) else {
            return Err(CompatibilityError::invalid_catalog());
        };
        let condition =
            parse_condition(requirement, state).ok_or_else(CompatibilityError::invalid_catalog)?;
        let status = parse_status(&rule.status).ok_or_else(CompatibilityError::invalid_catalog)?;
        let action = parse_action(&rule.action).ok_or_else(CompatibilityError::invalid_catalog)?;
        if !valid_rule_outcome(&condition, status, action)
            || parsed
                .insert(
                    condition,
                    ParsedRule {
                        rule,
                        status,
                        action,
                    },
                )
                .is_some()
        {
            return Err(CompatibilityError::invalid_catalog());
        }
    }
    if !parsed.contains_key(&Condition::new("baseline", "satisfied")) {
        return Err(CompatibilityError::missing_rule());
    }
    Ok(parsed)
}

fn conditions_for(
    requirements: &SkillRequirements,
    context: &RuntimeContext,
) -> BTreeSet<Condition> {
    let mut conditions = BTreeSet::from([Condition::new("baseline", "satisfied")]);

    match context.sandbox_state {
        SandboxState::Ready => {}
        SandboxState::Unavailable => {
            conditions.insert(Condition::new("sandbox", "unavailable"));
        }
        SandboxState::Unknown => {
            conditions.insert(Condition::new("sandbox", "unknown"));
        }
    }
    match context.deployment_status {
        DeploymentStatus::Deployed => {}
        DeploymentStatus::NotDeployed => {
            conditions.insert(Condition::new("deployment", "not_deployed"));
        }
        DeploymentStatus::Pending => {
            conditions.insert(Condition::new("deployment", "pending"));
        }
        DeploymentStatus::NeedsRestart => {
            conditions.insert(Condition::new("deployment", "restart_required"));
        }
        DeploymentStatus::Failed => {
            conditions.insert(Condition::new("deployment", "failed"));
        }
    }
    match context.discovery_status {
        DiscoveryStatus::Discovered => {}
        DiscoveryStatus::NotDiscovered => {
            conditions.insert(Condition::new("discovery", "not_discovered"));
        }
        DiscoveryStatus::Unknown | DiscoveryStatus::NotRunning | DiscoveryStatus::ProbeFailed => {
            conditions.insert(Condition::new("discovery", "unknown"));
        }
    }

    flag_conditions(
        &mut conditions,
        "network",
        requirements.needs_network.value,
        context.network,
    );
    flag_conditions(
        &mut conditions,
        "ssh",
        requirements.needs_ssh.value,
        effective_ssh_availability(&context.ssh),
    );
    flag_conditions(
        &mut conditions,
        "mcp",
        requirements.needs_mcp.value,
        context.mcp,
    );
    flag_conditions(
        &mut conditions,
        "local_command",
        requirements.needs_local_command.value,
        match context.local_command_policy {
            LocalCommandPolicy::Allowed => CapabilityAvailability::Available,
            LocalCommandPolicy::Denied => CapabilityAvailability::Unavailable,
            LocalCommandPolicy::Unknown => CapabilityAvailability::Unknown,
        },
    );
    list_conditions(
        &mut conditions,
        "binary",
        &requirements.required_binaries.values,
        requirements.required_binaries.source,
        &context.available_binaries,
        context.binary_inventory,
    );
    list_conditions(
        &mut conditions,
        "environment",
        &requirements.required_environment.values,
        requirements.required_environment.source,
        &context.available_environment,
        context.environment_inventory,
    );
    list_conditions(
        &mut conditions,
        "runtime_asset",
        &requirements.required_runtime_assets.values,
        requirements.required_runtime_assets.source,
        &context.available_runtime_assets,
        context.runtime_asset_inventory,
    );

    if requirements.supported_platforms.source == RequirementSource::Unknown {
        conditions.insert(Condition::new("platform", "requirement_unknown"));
    } else if !requirements
        .supported_platforms
        .values
        .iter()
        .any(|platform| platform == &context.platform)
    {
        conditions.insert(Condition::new("platform", "mismatch"));
    }

    match (
        requirements.minimum_runtime_version.source,
        requirements.minimum_runtime_version.value.as_deref(),
    ) {
        (RequirementSource::Unknown, _) => {
            conditions.insert(Condition::new(
                "minimum_runtime_version",
                "requirement_unknown",
            ));
        }
        (_, Some(minimum)) => match context.science_version.as_deref() {
            None => {
                conditions.insert(Condition::new(
                    "minimum_runtime_version",
                    "missing_runtime_version",
                ));
            }
            Some(current) => match compare_versions(current, minimum) {
                Some(Ordering::Less) => {
                    conditions.insert(Condition::new("minimum_runtime_version", "not_met"));
                }
                Some(_) => {}
                None => {
                    conditions.insert(Condition::new("minimum_runtime_version", "unparseable"));
                }
            },
        },
        (_, None) => {
            conditions.insert(Condition::new(
                "minimum_runtime_version",
                "requirement_unknown",
            ));
        }
    }
    conditions
}

fn effective_ssh_availability(summary: &SshCapabilitySummary) -> CapabilityAvailability {
    match summary.transport {
        CapabilityAvailability::Unavailable => CapabilityAvailability::Unavailable,
        CapabilityAvailability::Unknown => CapabilityAvailability::Unknown,
        CapabilityAvailability::Available => {
            if summary.agent_visible == BooleanCapability::True
                || summary.config_available == BooleanCapability::True
            {
                CapabilityAvailability::Available
            } else if summary.agent_visible == BooleanCapability::False
                && summary.config_available == BooleanCapability::False
            {
                CapabilityAvailability::Unavailable
            } else {
                CapabilityAvailability::Unknown
            }
        }
    }
}

fn flag_conditions(
    conditions: &mut BTreeSet<Condition>,
    requirement: &'static str,
    required: RequirementFlag,
    availability: CapabilityAvailability,
) {
    match required {
        RequirementFlag::False => {}
        RequirementFlag::Unknown => {
            conditions.insert(Condition::new(requirement, "requirement_unknown"));
        }
        RequirementFlag::True => match availability {
            CapabilityAvailability::Available => {}
            CapabilityAvailability::Unavailable => {
                conditions.insert(Condition::new(requirement, "unavailable"));
            }
            CapabilityAvailability::Unknown => {
                conditions.insert(Condition::new(requirement, "availability_unknown"));
            }
        },
    }
}

fn list_conditions(
    conditions: &mut BTreeSet<Condition>,
    requirement: &'static str,
    required: &[String],
    source: RequirementSource,
    available: &BTreeSet<String>,
    availability: CapabilityAvailability,
) {
    if source == RequirementSource::Unknown {
        conditions.insert(Condition::new(requirement, "requirement_unknown"));
    } else if !required.is_empty() {
        match availability {
            CapabilityAvailability::Unknown => {
                conditions.insert(Condition::new(requirement, "availability_unknown"));
            }
            CapabilityAvailability::Unavailable => {
                conditions.insert(Condition::new(requirement, "missing"));
            }
            CapabilityAvailability::Available
                if required.iter().any(|item| !available.contains(item)) =>
            {
                conditions.insert(Condition::new(requirement, "missing"));
            }
            CapabilityAvailability::Available => {}
        }
    }
}

fn parse_condition(requirement: &str, state: &str) -> Option<Condition> {
    let requirement = match requirement {
        "baseline" => "baseline",
        "sandbox" => "sandbox",
        "deployment" => "deployment",
        "discovery" => "discovery",
        "network" => "network",
        "ssh" => "ssh",
        "mcp" => "mcp",
        "local_command" => "local_command",
        "binary" => "binary",
        "environment" => "environment",
        "runtime_asset" => "runtime_asset",
        "platform" => "platform",
        "minimum_runtime_version" => "minimum_runtime_version",
        _ => return None,
    };
    let valid = match requirement {
        "baseline" => matches!(state, "satisfied"),
        "sandbox" => matches!(state, "unavailable" | "unknown"),
        "deployment" => matches!(
            state,
            "not_deployed" | "pending" | "restart_required" | "failed"
        ),
        "discovery" => matches!(state, "unknown" | "not_discovered"),
        "network" | "ssh" | "mcp" | "local_command" => matches!(
            state,
            "requirement_unknown" | "availability_unknown" | "unavailable"
        ),
        "binary" | "environment" | "runtime_asset" => {
            matches!(
                state,
                "requirement_unknown" | "availability_unknown" | "missing"
            )
        }
        "platform" => matches!(state, "requirement_unknown" | "mismatch"),
        "minimum_runtime_version" => matches!(
            state,
            "requirement_unknown" | "missing_runtime_version" | "unparseable" | "not_met"
        ),
        _ => false,
    };
    valid.then_some(Condition::new(
        requirement,
        match state {
            "satisfied" => "satisfied",
            "unavailable" => "unavailable",
            "unknown" => "unknown",
            "not_deployed" => "not_deployed",
            "pending" => "pending",
            "restart_required" => "restart_required",
            "failed" => "failed",
            "not_discovered" => "not_discovered",
            "requirement_unknown" => "requirement_unknown",
            "availability_unknown" => "availability_unknown",
            "missing" => "missing",
            "mismatch" => "mismatch",
            "missing_runtime_version" => "missing_runtime_version",
            "unparseable" => "unparseable",
            "not_met" => "not_met",
            _ => return None,
        },
    ))
}

fn parse_status(value: &str) -> Option<CompatibilityStatus> {
    match value {
        "supported" => Some(CompatibilityStatus::Supported),
        "limited" => Some(CompatibilityStatus::Limited),
        "unsupported" => Some(CompatibilityStatus::Unsupported),
        "unknown" => Some(CompatibilityStatus::Unknown),
        _ => None,
    }
}

fn parse_action(value: &str) -> Option<CompatibilityAction> {
    match value {
        "none" => Some(CompatibilityAction::None),
        "document" => Some(CompatibilityAction::Document),
        "degrade" => Some(CompatibilityAction::Degrade),
        "diagnose" => Some(CompatibilityAction::Diagnose),
        "disable" => Some(CompatibilityAction::Disable),
        _ => None,
    }
}

fn valid_rule_outcome(
    condition: &Condition,
    status: CompatibilityStatus,
    action: CompatibilityAction,
) -> bool {
    let expected = match (condition.requirement, condition.state) {
        ("baseline", "satisfied") => CompatibilityStatus::Supported,
        (_, "requirement_unknown" | "availability_unknown" | "unknown")
        | ("minimum_runtime_version", "missing_runtime_version" | "unparseable") => {
            CompatibilityStatus::Unknown
        }
        ("deployment", "not_deployed" | "pending" | "restart_required") => {
            CompatibilityStatus::Limited
        }
        _ => CompatibilityStatus::Unsupported,
    };
    if status != expected {
        return false;
    }
    match status {
        CompatibilityStatus::Supported => action == CompatibilityAction::None,
        CompatibilityStatus::Limited => matches!(
            action,
            CompatibilityAction::Document
                | CompatibilityAction::Degrade
                | CompatibilityAction::Diagnose
        ),
        CompatibilityStatus::Unknown => {
            matches!(
                action,
                CompatibilityAction::Diagnose | CompatibilityAction::Document
            )
        }
        CompatibilityStatus::Unsupported => action == CompatibilityAction::Disable,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PublicVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<VersionIdentifier>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VersionIdentifier {
    Numeric(u64),
    Text(String),
}

fn compare_versions(current: &str, minimum: &str) -> Option<Ordering> {
    let current = extract_public_version(current)?;
    let minimum = parse_version_token(minimum)?;
    Some(compare_public_versions(&current, &minimum))
}

fn extract_public_version(value: &str) -> Option<PublicVersion> {
    let versions: Vec<_> = value
        .split_ascii_whitespace()
        .filter_map(|token| {
            parse_version_token(token.trim_matches(|c| matches!(c, '(' | ')' | ',')))
        })
        .collect();
    if versions.len() == 1 {
        versions.into_iter().next()
    } else {
        None
    }
}

fn parse_version_token(value: &str) -> Option<PublicVersion> {
    if value.is_empty() || value.len() > 64 || !value.is_ascii() {
        return None;
    }
    let without_build = value.split_once('+').map_or(value, |(left, _)| left);
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(left, right)| (left, Some(right)));
    let mut core_parts = core.split('.');
    let major = core_parts.next()?.parse().ok()?;
    let minor = core_parts.next()?.parse().ok()?;
    let patch = core_parts.next()?.parse().ok()?;
    if core_parts.next().is_some() {
        return None;
    }
    let prerelease = match prerelease {
        None => Vec::new(),
        Some("") => return None,
        Some(value) => value
            .split('.')
            .map(|part| {
                if part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return None;
                }
                if part.bytes().all(|byte| byte.is_ascii_digit()) {
                    part.parse().ok().map(VersionIdentifier::Numeric)
                } else {
                    Some(VersionIdentifier::Text(part.to_string()))
                }
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(PublicVersion {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn compare_public_versions(left: &PublicVersion, right: &PublicVersion) -> Ordering {
    (left.major, left.minor, left.patch)
        .cmp(&(right.major, right.minor, right.patch))
        .then_with(|| compare_prerelease(&left.prerelease, &right.prerelease))
}

fn compare_prerelease(left: &[VersionIdentifier], right: &[VersionIdentifier]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (VersionIdentifier::Numeric(left), VersionIdentifier::Numeric(right)) => {
                left.cmp(right)
            }
            (VersionIdentifier::Numeric(_), VersionIdentifier::Text(_)) => Ordering::Less,
            (VersionIdentifier::Text(_), VersionIdentifier::Numeric(_)) => Ordering::Greater,
            (VersionIdentifier::Text(left), VersionIdentifier::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

fn safe_version_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b'+' | b' ' | b'(' | b')' | b',')
        })
}

fn validate_name_set(
    values: &BTreeSet<String>,
    validator: fn(&str) -> bool,
    kind: &str,
) -> Result<(), CompatibilityError> {
    if values.len() > MAX_CONTEXT_ITEMS
        || values
            .iter()
            .any(|value| value.len() > MAX_CONTEXT_ITEM_BYTES || !validator(value))
    {
        return Err(CompatibilityError::invalid_context(format!(
            "runtime {kind} names are invalid"
        )));
    }
    Ok(())
}

fn valid_rule_id(value: &str) -> bool {
    value
        .strip_prefix("skill.")
        .is_some_and(|suffix| !suffix.is_empty())
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

pub(crate) fn public_catalog_text_is_safe(value: &str, max_len: usize) -> bool {
    if value.is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    ![
        "/users/",
        "/home/",
        "/private/var/",
        "/var/folders/",
        "/tmp/",
        "/etc/",
        "~/",
        "\\users\\",
        ".ssh/",
        ".ssh\\",
        ".claude-science",
        "encryption.key",
        "oauth_token",
        "oauth-token",
        "access_token",
        "access-token",
        "refresh_token",
        "refresh-token",
        "api_key",
        "api-key",
        "private_key",
        "private-key",
        "client_secret",
        "client-secret",
        "-----begin ",
        "inventory.v1",
        "installed_at",
        "content_hash",
        "source_ref",
        "source_revision",
        "active-org",
        "/orgs/",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        && !credential_assignment(&lower)
}

fn safe_skill_reference(value: &str) -> bool {
    public_catalog_text_is_safe(value, 512)
        && ["desktop/", "test/", "scripts/"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
        && !value.contains("../")
        && !value.contains('\\')
}

fn credential_assignment(value: &str) -> bool {
    [
        "api key",
        "private key",
        "oauth token",
        "access token",
        "refresh token",
        "client secret",
    ]
    .iter()
    .any(|label| {
        let mut remainder = value;
        while let Some(index) = remainder.find(label) {
            let after = remainder[index + label.len()..].trim_start();
            let assigned = after.strip_prefix(':').or_else(|| after.strip_prefix('='));
            if assigned.is_some_and(|value| !value.trim().is_empty()) {
                return true;
            }
            remainder = &after[after.len().min(1)..];
        }
        false
    })
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_manager::model::{
        runtime_name, SkillId, SkillManifest, SkillSource, ValidationStatus,
    };
    use crate::skill_manager::requirements::{FlagRequirement, ListRequirement, StringRequirement};

    fn rule(id: &str, requirement: &str, state: &str, status: &str, action: &str) -> Value {
        serde_json::json!({
            "id": id,
            "scope": "skill",
            "match": {"requirement": requirement, "state": state},
            "status": status,
            "action": action,
            "reason": format!("Public compatibility reason for {requirement} {state}."),
            "evidence": ["desktop/src-tauri/src/skill_manager/compatibility.rs"],
            "tests": ["desktop/src-tauri/src/skill_manager/compatibility.rs::tests"]
        })
    }

    fn catalog_with(rules: Vec<Value>) -> CapabilityCatalog {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "providers": [],
            "tool_rules": [],
            "mcp_servers": [],
            "skills": rules,
            "science_versions": [],
            "transport_rules": []
        }))
        .unwrap()
    }

    fn baseline_rules() -> Vec<Value> {
        vec![rule(
            "skill.baseline.satisfied",
            "baseline",
            "satisfied",
            "supported",
            "none",
        )]
    }

    fn fully_declared_requirements() -> SkillRequirements {
        let declared_false = FlagRequirement {
            value: RequirementFlag::False,
            source: RequirementSource::Declared,
        };
        SkillRequirements {
            needs_network: declared_false.clone(),
            needs_ssh: declared_false.clone(),
            needs_mcp: declared_false.clone(),
            needs_local_command: declared_false,
            required_binaries: ListRequirement {
                values: Vec::new(),
                source: RequirementSource::Declared,
            },
            required_environment: ListRequirement {
                values: Vec::new(),
                source: RequirementSource::Declared,
            },
            required_runtime_assets: ListRequirement {
                values: Vec::new(),
                source: RequirementSource::Declared,
            },
            restart_required: FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Inferred,
            },
            supported_platforms: ListRequirement {
                values: vec!["macos".to_string()],
                source: RequirementSource::Declared,
            },
            minimum_runtime_version: StringRequirement {
                value: Some("0.1.0".to_string()),
                source: RequirementSource::Declared,
            },
        }
    }

    fn installed(requirements: SkillRequirements) -> InstalledSkill {
        let skill_id = SkillId::parse("sk_0123456789abcdef0123456789abcdef").unwrap();
        InstalledSkill {
            skill_id: skill_id.clone(),
            manifest: SkillManifest {
                name: "Compatibility probe".to_string(),
                description: "Safe compatibility fixture".to_string(),
                declared_version: Some("1.0.0".to_string()),
                license: None,
            },
            source: SkillSource::LocalDirectory {
                display_path: "<local-directory>".to_string(),
            },
            content_hash: "a".repeat(64),
            requirements,
            compatibility_acknowledgment: None,
            runtime_name: runtime_name("Compatibility probe", &skill_id).unwrap(),
            installed_at: 1,
            updated_at: 1,
            enabled: true,
            validation_status: ValidationStatus::Valid,
            deployment_status: DeploymentStatus::Deployed,
            discovery_status: DiscoveryStatus::Discovered,
            restart_required: false,
            last_error: None,
        }
    }

    fn context() -> RuntimeContext {
        RuntimeContext {
            science_version: Some(
                "claude-science 0.1.18-dev.20260709.t211149.shab3f5130 (release, public)"
                    .to_string(),
            ),
            platform: "macos".to_string(),
            sandbox_state: SandboxState::Ready,
            deployment_status: DeploymentStatus::Deployed,
            discovery_status: DiscoveryStatus::Discovered,
            network_mode: NetworkMode::Gateway,
            network: CapabilityAvailability::Available,
            mcp: CapabilityAvailability::Available,
            local_command_policy: LocalCommandPolicy::Allowed,
            ssh: SshCapabilitySummary {
                transport: CapabilityAvailability::Available,
                agent_visible: BooleanCapability::Unknown,
                config_available: BooleanCapability::Unknown,
            },
            available_binaries: BTreeSet::new(),
            binary_inventory: CapabilityAvailability::Available,
            available_environment: BTreeSet::new(),
            environment_inventory: CapabilityAvailability::Available,
            available_runtime_assets: BTreeSet::new(),
            runtime_asset_inventory: CapabilityAvailability::Available,
        }
    }

    #[test]
    fn supported_skill_uses_baseline_rule_and_stable_fingerprint() {
        let skill = installed(fully_declared_requirements());
        let context = context();
        let catalog = catalog_with(baseline_rules());
        let first = evaluate_skill_compatibility(&skill, &context, &catalog).unwrap();
        let second = evaluate_skill_compatibility(&skill, &context, &catalog).unwrap();
        assert_eq!(first.status, CompatibilityStatus::Supported);
        assert_eq!(first.action, CompatibilityAction::None);
        assert_eq!(first.matched_rule_ids, vec!["skill.baseline.satisfied"]);
        assert_eq!(first.runtime_fingerprint, second.runtime_fingerprint);
        assert_eq!(first.runtime_fingerprint.len(), 64);
    }

    #[test]
    fn unknown_requirement_is_not_treated_as_false() {
        let mut requirements = fully_declared_requirements();
        requirements.needs_ssh = FlagRequirement::default();
        let skill = installed(requirements);
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.ssh.requirement-unknown",
            "ssh",
            "requirement_unknown",
            "unknown",
            "diagnose",
        ));
        let verdict =
            evaluate_skill_compatibility(&skill, &context(), &catalog_with(rules)).unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unknown);
        assert_eq!(verdict.action, CompatibilityAction::Diagnose);
        assert_eq!(verdict.matched_rule_ids[0], "skill.ssh.requirement-unknown");
    }

    #[test]
    fn unavailable_ssh_is_unsupported_and_disable_wins() {
        let mut requirements = fully_declared_requirements();
        requirements.needs_ssh = FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Declared,
        };
        let mut context = context();
        context.ssh.transport = CapabilityAvailability::Unavailable;
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.ssh.unavailable",
            "ssh",
            "unavailable",
            "unsupported",
            "disable",
        ));
        let verdict =
            evaluate_skill_compatibility(&installed(requirements), &context, &catalog_with(rules))
                .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unsupported);
        assert_eq!(verdict.action, CompatibilityAction::Disable);
    }

    #[test]
    fn ssh_agent_and_config_summary_are_aggregated_conservatively() {
        let mut requirements = fully_declared_requirements();
        requirements.needs_ssh = FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Declared,
        };
        let skill = installed(requirements);

        let mut unavailable = context();
        unavailable.ssh.agent_visible = BooleanCapability::False;
        unavailable.ssh.config_available = BooleanCapability::False;
        let mut unavailable_rules = baseline_rules();
        unavailable_rules.push(rule(
            "skill.ssh.unavailable",
            "ssh",
            "unavailable",
            "unsupported",
            "disable",
        ));
        let unavailable_verdict =
            evaluate_skill_compatibility(&skill, &unavailable, &catalog_with(unavailable_rules))
                .unwrap();
        assert_eq!(unavailable_verdict.status, CompatibilityStatus::Unsupported);

        let mut unknown = context();
        unknown.ssh.agent_visible = BooleanCapability::False;
        unknown.ssh.config_available = BooleanCapability::Unknown;
        let mut unknown_rules = baseline_rules();
        unknown_rules.push(rule(
            "skill.ssh.availability-unknown",
            "ssh",
            "availability_unknown",
            "unknown",
            "diagnose",
        ));
        let unknown_verdict =
            evaluate_skill_compatibility(&skill, &unknown, &catalog_with(unknown_rules)).unwrap();
        assert_eq!(unknown_verdict.status, CompatibilityStatus::Unknown);

        let mut available = context();
        available.ssh.agent_visible = BooleanCapability::True;
        available.ssh.config_available = BooleanCapability::False;
        let available_verdict =
            evaluate_skill_compatibility(&skill, &available, &catalog_with(baseline_rules()))
                .unwrap();
        assert_eq!(available_verdict.status, CompatibilityStatus::Supported);
    }

    #[test]
    fn offline_mode_requires_explicitly_unavailable_network() {
        for availability in [
            CapabilityAvailability::Unknown,
            CapabilityAvailability::Available,
        ] {
            let mut runtime = context();
            runtime.network_mode = NetworkMode::Offline;
            runtime.network = availability;
            assert_eq!(
                runtime.fingerprint().unwrap_err().code,
                CompatibilityErrorCode::InvalidRuntimeContext
            );
        }
    }

    #[test]
    fn declared_runtime_asset_can_be_represented_as_available() {
        let mut requirements = fully_declared_requirements();
        requirements.required_runtime_assets.values = vec!["models/index.json".to_string()];
        let mut runtime = context();
        runtime
            .available_runtime_assets
            .insert("models/index.json".to_string());
        let verdict = evaluate_skill_compatibility(
            &installed(requirements),
            &runtime,
            &catalog_with(baseline_rules()),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Supported);
    }

    #[test]
    fn invalid_validation_status_never_produces_a_verdict() {
        let mut skill = installed(fully_declared_requirements());
        skill.validation_status = ValidationStatus::Invalid;
        assert_eq!(
            evaluate_skill_compatibility(&skill, &context(), &catalog_with(baseline_rules()))
                .unwrap_err()
                .code,
            CompatibilityErrorCode::InvalidSkill
        );
    }

    #[test]
    fn unavailable_network_mcp_and_local_command_are_explicitly_unsupported() {
        for (requirement_name, rule_id) in [
            ("network", "skill.network.unavailable"),
            ("mcp", "skill.mcp.unavailable"),
            ("local_command", "skill.local-command.unavailable"),
        ] {
            let mut requirements = fully_declared_requirements();
            let required = FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Declared,
            };
            let mut runtime = context();
            match requirement_name {
                "network" => {
                    requirements.needs_network = required;
                    runtime.network_mode = NetworkMode::Offline;
                    runtime.network = CapabilityAvailability::Unavailable;
                }
                "mcp" => {
                    requirements.needs_mcp = required;
                    runtime.mcp = CapabilityAvailability::Unavailable;
                }
                "local_command" => {
                    requirements.needs_local_command = required;
                    runtime.local_command_policy = LocalCommandPolicy::Denied;
                }
                _ => unreachable!(),
            }
            let mut rules = baseline_rules();
            rules.push(rule(
                rule_id,
                requirement_name,
                "unavailable",
                "unsupported",
                "disable",
            ));
            let verdict = evaluate_skill_compatibility(
                &installed(requirements),
                &runtime,
                &catalog_with(rules),
            )
            .unwrap();
            assert_eq!(verdict.status, CompatibilityStatus::Unsupported);
            assert_eq!(verdict.action, CompatibilityAction::Disable);
            assert_eq!(verdict.matched_rule_ids[0], rule_id);
        }
    }

    #[test]
    fn pending_deployment_is_limited_with_document_action() {
        let mut runtime = context();
        runtime.deployment_status = DeploymentStatus::Pending;
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.deployment.pending",
            "deployment",
            "pending",
            "limited",
            "document",
        ));
        let verdict = evaluate_skill_compatibility(
            &installed(fully_declared_requirements()),
            &runtime,
            &catalog_with(rules),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Limited);
        assert_eq!(verdict.action, CompatibilityAction::Document);
    }

    #[test]
    fn missing_resources_platform_and_version_merge_in_stable_severity_order() {
        let mut requirements = fully_declared_requirements();
        requirements.required_binaries.values = vec!["python3".to_string()];
        requirements.required_environment.values = vec!["SAFE_TOKEN_NAME".to_string()];
        requirements.required_runtime_assets.values = vec!["runtime/tool".to_string()];
        requirements.supported_platforms.values = vec!["linux".to_string()];
        requirements.minimum_runtime_version.value = Some("1.0.0".to_string());
        let mut rules = baseline_rules();
        for (id, requirement, state) in [
            ("skill.binary.missing", "binary", "missing"),
            ("skill.environment.missing", "environment", "missing"),
            ("skill.runtime-asset.missing", "runtime_asset", "missing"),
            ("skill.platform.mismatch", "platform", "mismatch"),
            (
                "skill.minimum-runtime.not-met",
                "minimum_runtime_version",
                "not_met",
            ),
        ] {
            rules.push(rule(id, requirement, state, "unsupported", "disable"));
        }
        let verdict = evaluate_skill_compatibility(
            &installed(requirements),
            &context(),
            &catalog_with(rules),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unsupported);
        assert_eq!(verdict.action, CompatibilityAction::Disable);
        let mut sorted = verdict.matched_rule_ids[..5].to_vec();
        sorted.sort();
        assert_eq!(verdict.matched_rule_ids[..5], sorted);
        assert_eq!(
            verdict.matched_rule_ids.last().unwrap(),
            "skill.baseline.satisfied"
        );
    }

    #[test]
    fn deployment_limited_and_discovery_unknown_merge_to_unknown() {
        let mut context = context();
        context.deployment_status = DeploymentStatus::NeedsRestart;
        context.discovery_status = DiscoveryStatus::NotRunning;
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.deployment.restart-required",
            "deployment",
            "restart_required",
            "limited",
            "document",
        ));
        rules.push(rule(
            "skill.discovery.unknown",
            "discovery",
            "unknown",
            "unknown",
            "diagnose",
        ));
        let verdict = evaluate_skill_compatibility(
            &installed(fully_declared_requirements()),
            &context,
            &catalog_with(rules),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unknown);
        assert_eq!(verdict.action, CompatibilityAction::Diagnose);
        assert_eq!(verdict.matched_rule_ids[0], "skill.discovery.unknown");
    }

    #[test]
    fn not_discovered_is_explicitly_unsupported() {
        let mut runtime = context();
        runtime.discovery_status = DiscoveryStatus::NotDiscovered;
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.discovery.not-discovered",
            "discovery",
            "not_discovered",
            "unsupported",
            "disable",
        ));
        let verdict = evaluate_skill_compatibility(
            &installed(fully_declared_requirements()),
            &runtime,
            &catalog_with(rules),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unsupported);
        assert_eq!(verdict.action, CompatibilityAction::Disable);
    }

    #[test]
    fn unparseable_runtime_version_yields_unknown_verdict() {
        let mut runtime = context();
        runtime.science_version = Some("public-release unknown".to_string());
        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.minimum-runtime.unparseable",
            "minimum_runtime_version",
            "unparseable",
            "unknown",
            "diagnose",
        ));
        let verdict = evaluate_skill_compatibility(
            &installed(fully_declared_requirements()),
            &runtime,
            &catalog_with(rules),
        )
        .unwrap();
        assert_eq!(verdict.status, CompatibilityStatus::Unknown);
        assert_eq!(verdict.action, CompatibilityAction::Diagnose);
    }

    #[test]
    fn missing_or_invalid_catalog_rule_fails_closed() {
        let mut requirements = fully_declared_requirements();
        requirements.needs_mcp = FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Declared,
        };
        let mut context = context();
        context.mcp = CapabilityAvailability::Unavailable;
        let error = evaluate_skill_compatibility(
            &installed(requirements.clone()),
            &context,
            &catalog_with(baseline_rules()),
        )
        .unwrap_err();
        assert_eq!(error.code, CompatibilityErrorCode::MissingCatalogRule);

        let mut rules = baseline_rules();
        rules.push(rule(
            "skill.mcp.unavailable",
            "mcp",
            "unavailable",
            "supported",
            "none",
        ));
        let error =
            evaluate_skill_compatibility(&installed(requirements), &context, &catalog_with(rules))
                .unwrap_err();
        assert_eq!(error.code, CompatibilityErrorCode::InvalidCatalog);
    }

    #[test]
    fn fingerprint_changes_for_version_network_and_availability() {
        let base = context();
        let base_fingerprint = base.fingerprint().unwrap();
        let mut changed = base.clone();
        changed.science_version = Some("claude-science 0.1.19 (public)".to_string());
        assert_ne!(base_fingerprint, changed.fingerprint().unwrap());
        changed = base.clone();
        changed.network_mode = NetworkMode::VpnTun;
        assert_ne!(base_fingerprint, changed.fingerprint().unwrap());
        changed = base.clone();
        changed.mcp = CapabilityAvailability::Unavailable;
        assert_ne!(base_fingerprint, changed.fingerprint().unwrap());
        changed = base.clone();
        changed.available_binaries.insert("python3".to_string());
        assert_ne!(base_fingerprint, changed.fingerprint().unwrap());
    }

    #[test]
    fn version_comparison_is_conservative() {
        assert_eq!(
            compare_versions(
                "claude-science 0.1.18-dev.20260709.t211149.shab3f5130 (release, public)",
                "0.1.17"
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_versions("release unknown", "0.1.17"), None);
        assert_eq!(compare_versions("0.1.18 0.1.19", "0.1.17"), None);
        assert_eq!(compare_versions("0.1", "0.1.0"), None);
    }

    #[test]
    fn runtime_context_and_verdict_have_no_value_or_path_fields() {
        let serialized = serde_json::to_value(context()).unwrap();
        let object = serialized.as_object().unwrap();
        assert!(!object.contains_key("environment_values"));
        assert!(!object.contains_key("host"));
        assert!(!object.contains_key("path"));
        assert!(!object.contains_key("credentials"));
        let text = serde_json::to_string(
            &evaluate_skill_compatibility(
                &installed(fully_declared_requirements()),
                &context(),
                &catalog_with(baseline_rules()),
            )
            .unwrap(),
        )
        .unwrap();
        for forbidden in ["/Users/", "oauth", "api_key", "private_key", "SKILL.md"] {
            assert!(!text
                .to_ascii_lowercase()
                .contains(&forbidden.to_ascii_lowercase()));
        }
    }

    #[test]
    fn catalog_skill_match_schema_and_rule_ids_are_strict() {
        let mut extra_field = rule(
            "skill.baseline.satisfied",
            "baseline",
            "satisfied",
            "supported",
            "none",
        );
        extra_field["match"]["internal_path"] = Value::String("hidden".to_string());
        assert_eq!(
            validate_skill_catalog_rules(&catalog_with(vec![extra_field]))
                .unwrap_err()
                .code,
            CompatibilityErrorCode::InvalidCatalog
        );
        let invalid_id = rule(
            "transport.baseline.satisfied",
            "baseline",
            "satisfied",
            "supported",
            "none",
        );
        assert_eq!(
            validate_skill_catalog_rules(&catalog_with(vec![invalid_id]))
                .unwrap_err()
                .code,
            CompatibilityErrorCode::InvalidCatalog
        );
    }

    #[test]
    fn public_catalog_text_allows_capability_terms_but_rejects_private_shapes() {
        assert!(public_catalog_text_is_safe(
            "API key and private key support are documented as capabilities only.",
            512
        ));
        for private in [
            "oauth_token",
            "api_key",
            "private_key",
            "~/.ssh/config",
            "/Users/example/private",
            "/tmp/private",
            ".claude-science/orgs/value",
            "inventory.v1.json content_hash",
            "-----BEGIN PRIVATE KEY-----",
        ] {
            assert!(
                !public_catalog_text_is_safe(private, 512),
                "accepted {private}"
            );
        }
        assert!(!public_catalog_text_is_safe(
            "API key: sk-secret-value",
            512
        ));
        assert!(public_catalog_text_is_safe("API key:", 512));
    }

    #[test]
    fn resource_inventory_availability_is_conservative() {
        let mut requirements = fully_declared_requirements();
        requirements.required_binaries.values = vec!["python3".to_string()];
        let skill = installed(requirements);
        let catalog = catalog_with(vec![
            baseline_rules().into_iter().next().unwrap(),
            rule(
                "skill.binary.availability-unknown",
                "binary",
                "availability_unknown",
                "unknown",
                "diagnose",
            ),
            rule(
                "skill.binary.missing",
                "binary",
                "missing",
                "unsupported",
                "disable",
            ),
        ]);

        let mut runtime = context();
        runtime.binary_inventory = CapabilityAvailability::Unknown;
        runtime.available_binaries.clear();
        let unknown = evaluate_skill_compatibility(&skill, &runtime, &catalog).unwrap();
        assert_eq!(unknown.status, CompatibilityStatus::Unknown);
        assert!(unknown
            .matched_rule_ids
            .contains(&"skill.binary.availability-unknown".to_string()));

        runtime.binary_inventory = CapabilityAvailability::Available;
        runtime.available_binaries.insert("python3".to_string());
        assert_eq!(
            evaluate_skill_compatibility(&skill, &runtime, &catalog)
                .unwrap()
                .status,
            CompatibilityStatus::Supported
        );

        runtime.binary_inventory = CapabilityAvailability::Unavailable;
        assert_eq!(
            evaluate_skill_compatibility(&skill, &runtime, &catalog)
                .unwrap_err()
                .code,
            CompatibilityErrorCode::InvalidRuntimeContext
        );
        runtime.available_binaries.clear();
        assert_eq!(
            evaluate_skill_compatibility(&skill, &runtime, &catalog)
                .unwrap()
                .status,
            CompatibilityStatus::Unsupported
        );
    }
}
