use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};

use serde::{de, Deserialize, Deserializer, Serialize};

use super::requirements::SkillRequirements;

pub(crate) const INVENTORY_SCHEMA_VERSION: u32 = 1;
const SKILL_ID_PREFIX: &str = "sk_";
const MAX_ACKNOWLEDGED_RULES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityAcknowledgment {
    #[serde(default, alias = "rule_ids")]
    pub(crate) capability_rule_ids: Vec<String>,
    #[serde(default)]
    pub(crate) last_action_rule_ids: Vec<String>,
    pub(crate) capability_fingerprint: String,
    pub(crate) acknowledged_at: u64,
}

impl CompatibilityAcknowledgment {
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_ack_rule_ids(&self.capability_rule_ids)?;
        validate_ack_rule_ids(&self.last_action_rule_ids)?;
        if self.capability_rule_ids.len() > MAX_ACKNOWLEDGED_RULES
            || self.last_action_rule_ids.len() > MAX_ACKNOWLEDGED_RULES
        {
            return Err("兼容性确认规则数量无效".to_string());
        }

        if self.capability_rule_ids.is_empty() && self.last_action_rule_ids.is_empty() {
            return Err("空兼容性确认不得持久化".to_string());
        }
        if self.capability_fingerprint.len() != 64
            || !self
                .capability_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.acknowledged_at == 0
        {
            return Err("兼容性确认指纹或时间无效".to_string());
        }
        Ok(())
    }
}

fn validate_ack_rule_ids(rule_ids: &[String]) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for rule_id in rule_ids {
        if rule_id.is_empty()
            || rule_id.len() > 128
            || !rule_id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-' | b'_')
            })
            || previous.is_some_and(|value| value >= rule_id.as_str())
        {
            return Err("兼容性确认规则标识无效或未规范排序".to_string());
        }
        previous = Some(rule_id);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub(crate) struct SkillId(String);

impl<'de> Deserialize<'de> for SkillId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

impl SkillId {
    pub(crate) fn new_random() -> io::Result<Self> {
        let mut bytes = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let mut value = String::with_capacity(SKILL_ID_PREFIX.len() + bytes.len() * 2);
        value.push_str(SKILL_ID_PREFIX);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
        }
        Ok(Self(value))
    }

    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let suffix = value
            .strip_prefix(SKILL_ID_PREFIX)
            .ok_or("SkillId 必须以 sk_ 开头")?;
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("SkillId 必须包含 32 位小写十六进制随机标识".to_string());
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn short(&self) -> &str {
        &self.0[SKILL_ID_PREFIX.len()..SKILL_ID_PREFIX.len() + 8]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SkillSource {
    LocalDirectory { display_path: String },
    ExternalHomeDirectory { directory_name: String },
}

impl SkillSource {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::LocalDirectory { display_path } => {
                if display_path.is_empty()
                    || display_path.len() > 1_024
                    || display_path.chars().any(char::is_control)
                {
                    return Err("Skill 本地来源标签无效".to_string());
                }
            }
            Self::ExternalHomeDirectory { directory_name } => {
                if directory_name.is_empty()
                    || directory_name.starts_with('.')
                    || directory_name.len() > 255
                    || directory_name.chars().any(char::is_control)
                    || directory_name.contains('/')
                    || matches!(directory_name.as_str(), "." | "..")
                {
                    return Err("Skill 外部 HOME 来源键无效".to_string());
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillManifest {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) declared_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) license: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationStatus {
    Valid,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeploymentStatus {
    NotDeployed,
    Pending,
    Deployed,
    NeedsRestart,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryStatus {
    Unknown,
    NotRunning,
    NotDiscovered,
    Discovered,
    ProbeFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledSkill {
    pub(crate) skill_id: SkillId,
    pub(crate) manifest: SkillManifest,
    pub(crate) source: SkillSource,
    pub(crate) content_hash: String,
    #[serde(default)]
    pub(crate) requirements: SkillRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compatibility_acknowledgment: Option<CompatibilityAcknowledgment>,
    pub(crate) runtime_name: String,
    pub(crate) installed_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) enabled: bool,
    pub(crate) validation_status: ValidationStatus,
    pub(crate) deployment_status: DeploymentStatus,
    pub(crate) discovery_status: DiscoveryStatus,
    pub(crate) restart_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

impl InstalledSkill {
    pub(crate) fn validate(&self) -> Result<(), String> {
        SkillId::parse(self.skill_id.as_str())?;
        validate_manifest(&self.manifest)?;
        self.source.validate()?;
        self.requirements.validate()?;
        if let Some(acknowledgment) = &self.compatibility_acknowledgment {
            acknowledgment.validate()?;
        }
        if self.content_hash.len() != 64
            || !self
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "Skill {} 的 content_hash 不是 64 位小写十六进制",
                self.skill_id.as_str()
            ));
        }
        if self.runtime_name != runtime_name(&self.manifest.name, &self.skill_id)? {
            return Err(format!(
                "Skill {} 的 runtime_name 与稳定派生值不一致",
                self.skill_id.as_str()
            ));
        }
        if self.updated_at < self.installed_at {
            return Err(format!(
                "Skill {} 的 updated_at 早于 installed_at",
                self.skill_id.as_str()
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
    pub(crate) schema_version: u32,
    pub(crate) skills: Vec<InstalledSkill>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: Vec::new(),
        }
    }
}

impl Inventory {
    pub(crate) fn from_slice(data: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_slice(data)
            .map_err(|error| format!("Skill inventory JSON 解析失败：{error}"))?;
        let schema_version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or("Skill inventory 缺少有效 schema_version")?;
        if schema_version != u64::from(INVENTORY_SCHEMA_VERSION) {
            return Err(format!(
                "不支持的 Skill inventory schema_version：{schema_version}"
            ));
        }
        let inventory: Self = serde_json::from_value(value)
            .map_err(|error| format!("Skill inventory 结构无效：{error}"))?;
        inventory.validate()?;
        Ok(inventory)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != INVENTORY_SCHEMA_VERSION {
            return Err(format!(
                "不支持的 Skill inventory schema_version：{}",
                self.schema_version
            ));
        }
        let mut ids = BTreeSet::new();
        let mut runtime_names = BTreeSet::new();
        let mut external_sources = BTreeSet::new();
        for skill in &self.skills {
            skill.validate()?;
            if !ids.insert(skill.skill_id.clone()) {
                return Err(format!("重复的 SkillId：{}", skill.skill_id.as_str()));
            }
            if !runtime_names.insert(skill.runtime_name.clone()) {
                return Err(format!("重复的 runtime_name：{}", skill.runtime_name));
            }
            if let SkillSource::ExternalHomeDirectory { directory_name } = &skill.source {
                if !external_sources.insert(directory_name.clone()) {
                    return Err(format!("重复的外部 HOME Skill 来源：{directory_name}"));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_manifest(manifest: &SkillManifest) -> Result<(), String> {
    let name = manifest.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err("Skill name 必须为 1–100 个非控制字符".to_string());
    }
    let description = manifest.description.trim();
    if description.is_empty()
        || description.len() > 2_000
        || description.chars().any(char::is_control)
    {
        return Err("Skill description 必须为 1–2000 个非控制字符".to_string());
    }
    Ok(())
}

pub(crate) fn runtime_name(name: &str, skill_id: &SkillId) -> Result<String, String> {
    let mut slug = String::with_capacity(name.len().min(48));
    let mut separator_pending = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator_pending && !slug.is_empty() && slug.len() < 48 {
                slug.push('-');
            }
            separator_pending = false;
            if slug.len() < 48 {
                slug.push(ch.to_ascii_lowercase());
            }
        } else {
            separator_pending = !slug.is_empty();
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("skill");
    }
    Ok(format!("{slug}--{}", skill_id.short()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, name: &str) -> InstalledSkill {
        let skill_id = SkillId::parse(id).unwrap();
        InstalledSkill {
            runtime_name: runtime_name(name, &skill_id).unwrap(),
            skill_id,
            manifest: SkillManifest {
                name: name.to_string(),
                description: "A safe test skill".to_string(),
                declared_version: None,
                license: None,
            },
            source: SkillSource::LocalDirectory {
                display_path: "/redacted/source".to_string(),
            },
            content_hash: "a".repeat(64),
            requirements: SkillRequirements::default(),
            compatibility_acknowledgment: None,
            installed_at: 10,
            updated_at: 10,
            enabled: false,
            validation_status: ValidationStatus::Valid,
            deployment_status: DeploymentStatus::NotDeployed,
            discovery_status: DiscoveryStatus::Unknown,
            restart_required: false,
            last_error: None,
        }
    }

    #[test]
    fn skill_id_round_trip_and_runtime_name_are_stable() {
        let id = SkillId::parse("sk_0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(id.short(), "01234567");
        assert_eq!(
            runtime_name("  My Useful_Skill  ", &id).unwrap(),
            "my-useful-skill--01234567"
        );
        assert_eq!(runtime_name("中文技能", &id).unwrap(), "skill--01234567");
        assert!(SkillId::parse("sk_ABC").is_err());
    }

    #[test]
    fn inventory_v1_round_trips_without_skill_body() {
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![installed(
                "sk_0123456789abcdef0123456789abcdef",
                "Probe Skill",
            )],
        };
        inventory.validate().unwrap();
        let encoded = serde_json::to_vec(&inventory).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("SKILL body secret"));
        assert_eq!(Inventory::from_slice(&encoded).unwrap(), inventory);
    }

    #[test]
    fn inventory_rejects_new_schema_and_collisions() {
        assert!(Inventory::from_slice(br#"{"schema_version":2,"skills":[]}"#).is_err());
        assert!(Inventory::from_slice(br#"{"schema_version":1}"#).is_err());
        assert!(Inventory::from_slice(br#"{"schema_version":1,"skills":[],"skils":[]}"#).is_err());

        let skill = installed("sk_0123456789abcdef0123456789abcdef", "Probe Skill");
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![skill.clone(), skill],
        };
        assert!(inventory.validate().unwrap_err().contains("重复的 SkillId"));

        let first = installed("sk_01234567aaaaaaaaaaaaaaaaaaaaaaaa", "Probe Skill");
        let second = installed("sk_01234567bbbbbbbbbbbbbbbbbbbbbbbb", "Probe Skill");
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![first, second],
        };
        assert!(inventory
            .validate()
            .unwrap_err()
            .contains("重复的 runtime_name"));
    }

    #[test]
    fn invalid_manifest_and_hash_fail_closed() {
        let mut skill = installed("sk_0123456789abcdef0123456789abcdef", "Probe Skill");
        skill.manifest.description.clear();
        assert!(skill.validate().is_err());
        skill.manifest.description = "ok".to_string();
        skill.content_hash = "A".repeat(64);
        assert!(skill.validate().is_err());
    }

    #[test]
    fn compatibility_acknowledgment_is_bounded_sorted_and_body_free() {
        let mut skill = installed("sk_0123456789abcdef0123456789abcdef", "Probe Skill");
        skill.compatibility_acknowledgment = Some(CompatibilityAcknowledgment {
            capability_rule_ids: vec!["skill.network.requirement-unknown".to_string()],
            last_action_rule_ids: vec!["skill.deployment.pending".to_string()],
            capability_fingerprint: "b".repeat(64),
            acknowledged_at: 42,
        });
        skill.validate().unwrap();
        let encoded = serde_json::to_string(&skill).unwrap();
        assert!(encoded.contains("skill.network.requirement-unknown"));
        assert!(!encoded.contains("reason"));
        assert!(!encoded.contains("SKILL body"));

        skill
            .compatibility_acknowledgment
            .as_mut()
            .unwrap()
            .capability_rule_ids = vec![
            "skill.ssh.requirement-unknown".to_string(),
            "skill.network.requirement-unknown".to_string(),
        ];
        assert!(skill.validate().is_err());
    }

    #[test]
    fn skill_id_deserialization_preserves_the_type_invariant() {
        let valid: SkillId =
            serde_json::from_str(r#""sk_0123456789abcdef0123456789abcdef""#).unwrap();
        assert_eq!(valid.short(), "01234567");
        for invalid in [
            r#""short""#,
            r#""sk_ABC""#,
            r#""sk_0123456789abcdef0123456789abcdeé""#,
        ] {
            assert!(serde_json::from_str::<SkillId>(invalid).is_err());
        }
    }

    #[test]
    fn inventory_rejects_unknown_source_fields_and_missing_restart_state() {
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![installed(
                "sk_0123456789abcdef0123456789abcdef",
                "Probe Skill",
            )],
        };
        let mut value = serde_json::to_value(inventory).unwrap();
        value["skills"][0]["source"]["unexpected"] = serde_json::json!(true);
        assert!(Inventory::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());

        value["skills"][0]["source"]
            .as_object_mut()
            .unwrap()
            .remove("unexpected");
        value["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("restart_required");
        assert!(Inventory::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn inventory_v1_without_requirements_uses_safe_backcompat_default() {
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![installed(
                "sk_0123456789abcdef0123456789abcdef",
                "Probe Skill",
            )],
        };
        let mut value = serde_json::to_value(inventory).unwrap();
        value["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("requirements");
        let decoded = Inventory::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.skills[0].requirements, SkillRequirements::default());
    }

    #[test]
    fn inventory_rejects_unsafe_external_home_source_keys() {
        let mut skill = installed("sk_0123456789abcdef0123456789abcdef", "Probe Skill");
        for directory_name in ["../escape", ".hidden", "", "nested/skill"] {
            skill.source = SkillSource::ExternalHomeDirectory {
                directory_name: directory_name.to_string(),
            };
            let inventory = Inventory {
                schema_version: INVENTORY_SCHEMA_VERSION,
                skills: vec![skill.clone()],
            };
            assert!(Inventory::from_slice(&serde_json::to_vec(&inventory).unwrap()).is_err());
        }
    }

    #[test]
    fn inventory_rejects_duplicate_external_home_source_identity() {
        let mut first = installed("sk_0123456789abcdef0123456789abcdef", "First Skill");
        first.source = SkillSource::ExternalHomeDirectory {
            directory_name: "same-source".to_string(),
        };
        let mut second = installed("sk_fedcba9876543210fedcba9876543210", "Second Skill");
        second.source = first.source.clone();
        let inventory = Inventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            skills: vec![first, second],
        };
        assert!(inventory.validate().is_err());
    }
}
