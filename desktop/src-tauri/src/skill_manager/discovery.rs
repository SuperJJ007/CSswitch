use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::deployment::{DeploymentRegistry, ReconcileReport};
use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::model::{DiscoveryStatus, InstalledSkill, Inventory, SkillId};

pub(crate) const DISCOVERY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoveryEvidence {
    pub(crate) skill_id: SkillId,
    pub(crate) runtime_name: String,
    pub(crate) content_hash: String,
    pub(crate) science_version: String,
    pub(crate) runtime_fingerprint: String,
    pub(crate) discovered: bool,
    pub(crate) observed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoveryEvidenceRegistry {
    pub(crate) schema_version: u32,
    pub(crate) evidence: Vec<DiscoveryEvidence>,
}

impl Default for DiscoveryEvidenceRegistry {
    fn default() -> Self {
        Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence: Vec::new(),
        }
    }
}

impl DiscoveryEvidenceRegistry {
    pub(crate) fn validate(&self) -> SkillResult<()> {
        if self.schema_version != DISCOVERY_SCHEMA_VERSION {
            return Err(discovery_invalid());
        }
        let mut ids = BTreeSet::new();
        for item in &self.evidence {
            if !ids.insert(item.skill_id.clone())
                || item.runtime_name.is_empty()
                || item.runtime_name.len() > 96
                || item.content_hash.len() != 64
                || !item
                    .content_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || item.science_version.is_empty()
                || item.science_version.len() > 160
                || item.runtime_fingerprint.len() != 64
                || !item
                    .runtime_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || item.observed_at == 0
                || !item.science_version.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(byte, b'.' | b'-' | b'_' | b' ' | b'(' | b')' | b',')
                })
            {
                return Err(discovery_invalid());
            }
        }
        Ok(())
    }
}

#[allow(dead_code, reason = "called by the Stage 3.2 isolated discovery probe")]
pub(crate) fn validate_science_version(value: &str) -> SkillResult<()> {
    let registry = DiscoveryEvidenceRegistry {
        schema_version: DISCOVERY_SCHEMA_VERSION,
        evidence: vec![DiscoveryEvidence {
            skill_id: SkillId::parse("sk_00000000000000000000000000000000")
                .expect("fixed SkillId is valid"),
            runtime_name: "probe-00000000".to_string(),
            content_hash: "0".repeat(64),
            science_version: value.to_string(),
            runtime_fingerprint: "0".repeat(64),
            discovered: false,
            observed_at: 1,
        }],
    };
    registry.validate()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScienceProbeState {
    Running,
    NotRunning,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillRuntimeStatus {
    pub(crate) skill_id: SkillId,
    pub(crate) name: String,
    pub(crate) installed: bool,
    pub(crate) enabled: bool,
    pub(crate) deployed: bool,
    pub(crate) deployed_hash: Option<String>,
    pub(crate) discovery_status: DiscoveryStatus,
    pub(crate) restart_required: bool,
    pub(crate) deployment_status: super::model::DeploymentStatus,
    pub(crate) last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillManagerStatus {
    pub(crate) schema_version: u32,
    pub(crate) science_state: ScienceProbeState,
    pub(crate) science_version: Option<String>,
    pub(crate) skills: Vec<SkillRuntimeStatus>,
    pub(crate) diagnostic_codes: Vec<String>,
}

pub(crate) fn evaluate_status(
    inventory: &Inventory,
    deployments: &DeploymentRegistry,
    evidence: &DiscoveryEvidenceRegistry,
    reconcile: &ReconcileReport,
    science_state: ScienceProbeState,
    science_version: Option<&str>,
    runtime_fingerprint: Option<&str>,
) -> SkillManagerStatus {
    let mut diagnostic_codes = reconcile
        .errors
        .iter()
        .map(|error| error.code.clone())
        .collect::<Vec<_>>();
    diagnostic_codes.sort();
    diagnostic_codes.dedup();
    let skills = inventory
        .skills
        .iter()
        .map(|skill| {
            skill_status(
                skill,
                deployments,
                evidence,
                reconcile,
                science_state,
                science_version,
                runtime_fingerprint,
            )
        })
        .collect();
    SkillManagerStatus {
        schema_version: 1,
        science_state,
        science_version: science_version.map(str::to_string),
        skills,
        diagnostic_codes,
    }
}

fn skill_status(
    skill: &InstalledSkill,
    deployments: &DeploymentRegistry,
    evidence: &DiscoveryEvidenceRegistry,
    reconcile: &ReconcileReport,
    science_state: ScienceProbeState,
    science_version: Option<&str>,
    runtime_fingerprint: Option<&str>,
) -> SkillRuntimeStatus {
    let record = deployments
        .deployments
        .iter()
        .find(|record| record.skill_id == skill.skill_id);
    let has_error = reconcile.errors.iter().any(|error| {
        error
            .skill_id
            .as_ref()
            .is_none_or(|id| id == &skill.skill_id)
    });
    let has_plan = reconcile
        .planned
        .iter()
        .any(|item| item.skill_id == skill.skill_id);
    let deployed = !has_error
        && !has_plan
        && skill.enabled
        && record.is_some_and(|record| {
            record.runtime_name == skill.runtime_name && record.content_hash == skill.content_hash
        });
    let discovery_status = match science_state {
        ScienceProbeState::NotRunning => DiscoveryStatus::NotRunning,
        ScienceProbeState::Unknown => DiscoveryStatus::ProbeFailed,
        ScienceProbeState::Running if !deployed => DiscoveryStatus::Unknown,
        ScienceProbeState::Running => evidence
            .evidence
            .iter()
            .find(|item| {
                item.skill_id == skill.skill_id
                    && item.runtime_name == skill.runtime_name
                    && item.content_hash == skill.content_hash
                    && science_version.is_some_and(|version| version == item.science_version)
                    && runtime_fingerprint
                        .is_some_and(|fingerprint| fingerprint == item.runtime_fingerprint)
            })
            .map(|item| {
                if item.discovered {
                    DiscoveryStatus::Discovered
                } else {
                    DiscoveryStatus::NotDiscovered
                }
            })
            .unwrap_or(DiscoveryStatus::Unknown),
    };
    SkillRuntimeStatus {
        skill_id: skill.skill_id.clone(),
        name: skill.manifest.name.clone(),
        installed: true,
        enabled: skill.enabled,
        deployed,
        deployed_hash: record.map(|record| record.content_hash.clone()),
        discovery_status,
        restart_required: skill.restart_required || has_plan,
        deployment_status: skill.deployment_status,
        last_error: skill.last_error.clone(),
    }
}

fn discovery_invalid() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::InventoryInvalid,
        "Skill discovery evidence 缺失完整性或版本校验",
        "请保留现有文件并重新运行隔离 discovery probe",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_manager::deployment::{DeploymentRecord, ReconcileAction, ReconcileItem};
    use crate::skill_manager::model::{
        runtime_name, DeploymentStatus, SkillManifest, SkillSource, ValidationStatus,
    };

    fn skill() -> InstalledSkill {
        let skill_id = SkillId::parse("sk_00112233445566778899aabbccddeeff").unwrap();
        InstalledSkill {
            runtime_name: runtime_name("Probe", &skill_id).unwrap(),
            skill_id,
            manifest: SkillManifest {
                name: "Probe".into(),
                description: "probe".into(),
                declared_version: None,
                license: None,
            },
            source: SkillSource::LocalDirectory {
                display_path: "probe".into(),
            },
            content_hash: "a".repeat(64),
            requirements: crate::skill_manager::requirements::SkillRequirements::default(),
            compatibility_acknowledgment: None,
            installed_at: 1,
            updated_at: 1,
            enabled: true,
            validation_status: ValidationStatus::Valid,
            deployment_status: DeploymentStatus::Deployed,
            discovery_status: DiscoveryStatus::Unknown,
            restart_required: false,
            last_error: None,
        }
    }

    fn report() -> ReconcileReport {
        ReconcileReport {
            dry_run: true,
            reason: "status".into(),
            planned: Vec::new(),
            applied: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
            restart_required: false,
        }
    }

    #[test]
    fn deployment_and_discovery_are_independent_and_version_bound() {
        let skill = skill();
        let inventory = Inventory {
            schema_version: 1,
            skills: vec![skill.clone()],
        };
        let deployments = DeploymentRegistry {
            schema_version: 1,
            deployments: vec![DeploymentRecord {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
            }],
        };
        let evidence = DiscoveryEvidenceRegistry {
            schema_version: 1,
            evidence: vec![DiscoveryEvidence {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
                science_version: "science-1".into(),
                runtime_fingerprint: "f".repeat(64),
                discovered: true,
                observed_at: 1,
            }],
        };
        let current = evaluate_status(
            &inventory,
            &deployments,
            &evidence,
            &report(),
            ScienceProbeState::Running,
            Some("science-1"),
            Some(&"f".repeat(64)),
        );
        assert!(current.skills[0].deployed);
        assert_eq!(
            current.skills[0].discovery_status,
            DiscoveryStatus::Discovered
        );
        let stale = evaluate_status(
            &inventory,
            &deployments,
            &evidence,
            &report(),
            ScienceProbeState::Running,
            Some("science-2"),
            Some(&"f".repeat(64)),
        );
        assert!(stale.skills[0].deployed);
        assert_eq!(stale.skills[0].discovery_status, DiscoveryStatus::Unknown);
        let rebuilt = evaluate_status(
            &inventory,
            &deployments,
            &evidence,
            &report(),
            ScienceProbeState::Running,
            Some("science-1"),
            Some(&"e".repeat(64)),
        );
        assert_eq!(rebuilt.skills[0].discovery_status, DiscoveryStatus::Unknown);

        let mut negative = evidence.clone();
        negative.evidence[0].discovered = false;
        let explicit_negative = evaluate_status(
            &inventory,
            &deployments,
            &negative,
            &report(),
            ScienceProbeState::Running,
            Some("science-1"),
            Some(&"f".repeat(64)),
        );
        assert_eq!(
            explicit_negative.skills[0].discovery_status,
            DiscoveryStatus::NotDiscovered
        );
    }

    #[test]
    fn planned_change_never_claims_deployed_or_discovered() {
        let skill = skill();
        let inventory = Inventory {
            schema_version: 1,
            skills: vec![skill.clone()],
        };
        let deployments = DeploymentRegistry {
            schema_version: 1,
            deployments: vec![DeploymentRecord {
                skill_id: skill.skill_id.clone(),
                runtime_name: skill.runtime_name.clone(),
                content_hash: skill.content_hash.clone(),
            }],
        };
        let mut pending = report();
        pending.planned.push(ReconcileItem {
            skill_id: skill.skill_id.clone(),
            runtime_name: skill.runtime_name.clone(),
            action: ReconcileAction::Replace,
            applied: false,
            detail: "pending".into(),
        });
        pending.restart_required = true;
        let status = evaluate_status(
            &inventory,
            &deployments,
            &DiscoveryEvidenceRegistry::default(),
            &pending,
            ScienceProbeState::Running,
            Some("science-1"),
            Some(&"f".repeat(64)),
        );
        assert!(!status.skills[0].deployed);
        assert_eq!(status.skills[0].discovery_status, DiscoveryStatus::Unknown);
        assert!(status.skills[0].restart_required);
    }
}
