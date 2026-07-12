use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

pub(crate) const REQUIREMENTS_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_REQUIREMENT_ITEMS: usize = 64;
pub(crate) const MAX_REQUIREMENT_ITEM_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequirementFlag {
    True,
    False,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequirementSource {
    Declared,
    Inferred,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlagRequirement {
    pub(crate) value: RequirementFlag,
    pub(crate) source: RequirementSource,
}

impl Default for FlagRequirement {
    fn default() -> Self {
        Self {
            value: RequirementFlag::Unknown,
            source: RequirementSource::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListRequirement {
    pub(crate) values: Vec<String>,
    pub(crate) source: RequirementSource,
}

impl Default for ListRequirement {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            source: RequirementSource::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StringRequirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<String>,
    pub(crate) source: RequirementSource,
}

impl Default for StringRequirement {
    fn default() -> Self {
        Self {
            value: None,
            source: RequirementSource::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillRequirements {
    pub(crate) needs_network: FlagRequirement,
    pub(crate) needs_ssh: FlagRequirement,
    pub(crate) needs_mcp: FlagRequirement,
    pub(crate) needs_local_command: FlagRequirement,
    pub(crate) required_binaries: ListRequirement,
    pub(crate) required_environment: ListRequirement,
    pub(crate) required_runtime_assets: ListRequirement,
    pub(crate) restart_required: FlagRequirement,
    pub(crate) supported_platforms: ListRequirement,
    pub(crate) minimum_runtime_version: StringRequirement,
}

impl Default for SkillRequirements {
    fn default() -> Self {
        let mut requirements = Self::unknown();
        requirements.restart_required = FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Inferred,
        };
        requirements
    }
}

impl SkillRequirements {
    pub(crate) fn unknown() -> Self {
        Self {
            needs_network: FlagRequirement::default(),
            needs_ssh: FlagRequirement::default(),
            needs_mcp: FlagRequirement::default(),
            needs_local_command: FlagRequirement::default(),
            required_binaries: ListRequirement::default(),
            required_environment: ListRequirement::default(),
            required_runtime_assets: ListRequirement::default(),
            restart_required: FlagRequirement::default(),
            supported_platforms: ListRequirement::default(),
            minimum_runtime_version: StringRequirement::default(),
        }
    }

    pub(crate) fn from_public_json(data: Option<&[u8]>) -> Result<Self, String> {
        let declared = match data {
            Some(data) => {
                let declaration: SkillRequirementDeclaration = serde_json::from_slice(data)
                    .map_err(|_| "csswitch.skill.json 结构无效".to_string())?;
                if declaration.schema_version != REQUIREMENTS_SCHEMA_VERSION {
                    return Err(format!(
                        "不支持的 csswitch.skill.json schema_version：{}",
                        declaration.schema_version
                    ));
                }
                Self::from_declaration(declaration.requirements)?
            }
            None => Self::unknown(),
        };
        Ok(Self::merge(&declared, &Self::inferred_runtime_facts()))
    }

    fn inferred_runtime_facts() -> Self {
        let mut facts = Self::unknown();
        facts.restart_required = FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Inferred,
        };
        facts
    }

    fn from_declaration(declaration: DeclaredRequirements) -> Result<Self, String> {
        Ok(Self {
            needs_network: declared_flag(declaration.needs_network),
            needs_ssh: declared_flag(declaration.needs_ssh),
            needs_mcp: declared_flag(declaration.needs_mcp),
            needs_local_command: declared_flag(declaration.needs_local_command),
            required_binaries: declared_list(
                declaration.required_binaries,
                "required_binaries",
                validate_binary,
            )?,
            required_environment: declared_list(
                declaration.required_environment,
                "required_environment",
                validate_environment,
            )?,
            required_runtime_assets: declared_list(
                declaration.required_runtime_assets,
                "required_runtime_assets",
                validate_runtime_asset,
            )?,
            restart_required: declared_flag(declaration.restart_required),
            supported_platforms: declared_list(
                declaration.supported_platforms,
                "supported_platforms",
                validate_platform,
            )?,
            minimum_runtime_version: declared_string(
                declaration.minimum_runtime_version,
                "minimum_runtime_version",
                validate_runtime_version,
            )?,
        })
    }

    pub(crate) fn merge(declared: &Self, inferred: &Self) -> Self {
        Self {
            needs_network: merge_flag(&declared.needs_network, &inferred.needs_network),
            needs_ssh: merge_flag(&declared.needs_ssh, &inferred.needs_ssh),
            needs_mcp: merge_flag(&declared.needs_mcp, &inferred.needs_mcp),
            needs_local_command: merge_flag(
                &declared.needs_local_command,
                &inferred.needs_local_command,
            ),
            required_binaries: merge_list(&declared.required_binaries, &inferred.required_binaries),
            required_environment: merge_list(
                &declared.required_environment,
                &inferred.required_environment,
            ),
            required_runtime_assets: merge_list(
                &declared.required_runtime_assets,
                &inferred.required_runtime_assets,
            ),
            restart_required: merge_flag(&declared.restart_required, &inferred.restart_required),
            supported_platforms: merge_list(
                &declared.supported_platforms,
                &inferred.supported_platforms,
            ),
            minimum_runtime_version: merge_string(
                &declared.minimum_runtime_version,
                &inferred.minimum_runtime_version,
            ),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_declared_flag(&self.needs_network)?;
        validate_declared_flag(&self.needs_ssh)?;
        validate_declared_flag(&self.needs_mcp)?;
        validate_declared_flag(&self.needs_local_command)?;
        if self.restart_required
            != (FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Inferred,
            })
        {
            return Err("restart_required 不符合 CSSwitch 固定推断策略".to_string());
        }
        validate_normalized_list(
            &self.required_binaries,
            "required_binaries",
            validate_binary,
        )?;
        validate_normalized_list(
            &self.required_environment,
            "required_environment",
            validate_environment,
        )?;
        validate_normalized_list(
            &self.required_runtime_assets,
            "required_runtime_assets",
            validate_runtime_asset,
        )?;
        validate_normalized_list(
            &self.supported_platforms,
            "supported_platforms",
            validate_platform,
        )?;
        validate_normalized_string(
            &self.minimum_runtime_version,
            "minimum_runtime_version",
            validate_runtime_version,
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillRequirementDeclaration {
    schema_version: u32,
    requirements: DeclaredRequirements,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeclaredRequirements {
    needs_network: Option<bool>,
    needs_ssh: Option<bool>,
    needs_mcp: Option<bool>,
    needs_local_command: Option<bool>,
    required_binaries: Option<Vec<String>>,
    required_environment: Option<Vec<String>>,
    required_runtime_assets: Option<Vec<String>>,
    restart_required: Option<bool>,
    supported_platforms: Option<Vec<String>>,
    minimum_runtime_version: Option<String>,
}

fn declared_flag(value: Option<bool>) -> FlagRequirement {
    match value {
        Some(true) => FlagRequirement {
            value: RequirementFlag::True,
            source: RequirementSource::Declared,
        },
        Some(false) => FlagRequirement {
            value: RequirementFlag::False,
            source: RequirementSource::Declared,
        },
        None => FlagRequirement::default(),
    }
}

fn declared_list(
    value: Option<Vec<String>>,
    field: &str,
    validator: fn(&str) -> bool,
) -> Result<ListRequirement, String> {
    let Some(values) = value else {
        return Ok(ListRequirement::default());
    };
    let values = normalize_list(values, field, validator)?;
    Ok(ListRequirement {
        values,
        source: RequirementSource::Declared,
    })
}

fn declared_string(
    value: Option<String>,
    field: &str,
    validator: fn(&str) -> bool,
) -> Result<StringRequirement, String> {
    let Some(value) = value else {
        return Ok(StringRequirement::default());
    };
    if !valid_item(&value) || !validator(&value) {
        return Err(format!("{field} 包含无效值"));
    }
    Ok(StringRequirement {
        value: Some(value),
        source: RequirementSource::Declared,
    })
}

fn merge_flag(declared: &FlagRequirement, inferred: &FlagRequirement) -> FlagRequirement {
    match (declared.value, inferred.value) {
        (_, RequirementFlag::True) => inferred.clone(),
        (RequirementFlag::True, _) => declared.clone(),
        (RequirementFlag::False, _) => declared.clone(),
        (RequirementFlag::Unknown, RequirementFlag::False) => inferred.clone(),
        (RequirementFlag::Unknown, RequirementFlag::Unknown) => FlagRequirement::default(),
    }
}

fn merge_list(declared: &ListRequirement, inferred: &ListRequirement) -> ListRequirement {
    if inferred.source == RequirementSource::Unknown {
        return declared.clone();
    }
    if declared.source == RequirementSource::Unknown {
        return inferred.clone();
    }
    let values = declared
        .values
        .iter()
        .chain(&inferred.values)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ListRequirement {
        values,
        source: RequirementSource::Inferred,
    }
}

fn merge_string(declared: &StringRequirement, inferred: &StringRequirement) -> StringRequirement {
    if inferred.source != RequirementSource::Unknown {
        inferred.clone()
    } else {
        declared.clone()
    }
}

fn validate_declared_flag(requirement: &FlagRequirement) -> Result<(), String> {
    match (requirement.value, requirement.source) {
        (RequirementFlag::Unknown, RequirementSource::Unknown)
        | (RequirementFlag::True, RequirementSource::Declared)
        | (RequirementFlag::False, RequirementSource::Declared) => Ok(()),
        _ => Err("Requirement flag 不符合当前 provenance 策略".to_string()),
    }
}

fn validate_normalized_list(
    requirement: &ListRequirement,
    field: &str,
    validator: fn(&str) -> bool,
) -> Result<(), String> {
    if requirement.source == RequirementSource::Unknown && !requirement.values.is_empty() {
        return Err(format!("{field} 的 unknown provenance 不得携带值"));
    }
    if requirement.source == RequirementSource::Inferred {
        return Err(format!("{field} 当前不允许 inferred provenance"));
    }
    let normalized = normalize_list(requirement.values.clone(), field, validator)?;
    if normalized != requirement.values {
        return Err(format!("{field} 未规范化"));
    }
    Ok(())
}

fn validate_normalized_string(
    requirement: &StringRequirement,
    field: &str,
    validator: fn(&str) -> bool,
) -> Result<(), String> {
    if requirement.value.is_none() != (requirement.source == RequirementSource::Unknown) {
        return Err(format!("{field} 的 value/source 不一致"));
    }
    if requirement.source == RequirementSource::Inferred {
        return Err(format!("{field} 当前不允许 inferred provenance"));
    }
    if let Some(value) = &requirement.value {
        if !valid_item(value) || !validator(value) {
            return Err(format!("{field} 包含无效值"));
        }
    }
    Ok(())
}

fn normalize_list(
    values: Vec<String>,
    field: &str,
    validator: fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    if values.len() > MAX_REQUIREMENT_ITEMS {
        return Err(format!("{field} 超过 {MAX_REQUIREMENT_ITEMS} 项限制"));
    }
    let mut normalized = BTreeSet::new();
    for value in values {
        if !valid_item(&value) || !validator(&value) {
            return Err(format!("{field} 包含无效值"));
        }
        normalized.insert(value);
    }
    Ok(normalized.into_iter().collect())
}

fn valid_item(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUIREMENT_ITEM_BYTES
        && !value.chars().any(char::is_control)
}

pub(crate) fn validate_binary(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

pub(crate) fn validate_environment(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(crate) fn validate_runtime_asset(value: &str) -> bool {
    if !value.is_ascii()
        || value.contains('\\')
        || value.contains("//")
        || value.starts_with('/')
        || value.ends_with('/')
        || value.split('/').any(|segment| segment == ".")
    {
        return false;
    }
    let path = Path::new(value);
    !value.is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && value.split('/').all(validate_binary)
}

fn validate_platform(value: &str) -> bool {
    matches!(value, "macos" | "linux" | "windows")
}

fn validate_runtime_version(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 64
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(requirements: serde_json::Value) -> Result<SkillRequirements, String> {
        SkillRequirements::from_public_json(Some(
            &serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "requirements": requirements,
            }))
            .unwrap(),
        ))
    }

    #[test]
    fn absent_declaration_is_unknown_except_for_inferred_restart() {
        let requirements = SkillRequirements::from_public_json(None).unwrap();
        assert_eq!(requirements.needs_network, FlagRequirement::default());
        assert_eq!(requirements.required_binaries, ListRequirement::default());
        assert_eq!(
            requirements.minimum_runtime_version,
            StringRequirement::default()
        );
        assert_eq!(
            requirements.restart_required,
            FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Inferred,
            }
        );
    }

    #[test]
    fn declaration_is_normalized_and_has_explicit_provenance() {
        let requirements = parse(serde_json::json!({
            "needs_network": false,
            "needs_ssh": true,
            "required_binaries": ["python3", "git", "python3"],
            "required_environment": ["PATH", "SAFE_ENV"],
            "required_runtime_assets": ["models/index.json", "templates"],
            "supported_platforms": ["macos", "linux"],
            "minimum_runtime_version": "0.1.18-dev.1",
            "restart_required": false
        }))
        .unwrap();
        assert_eq!(requirements.needs_network.value, RequirementFlag::False);
        assert_eq!(requirements.needs_ssh.source, RequirementSource::Declared);
        assert_eq!(requirements.required_binaries.values, ["git", "python3"]);
        assert_eq!(
            requirements.minimum_runtime_version.source,
            RequirementSource::Declared
        );
        assert_eq!(requirements.restart_required.value, RequirementFlag::True);
        assert_eq!(
            requirements.restart_required.source,
            RequirementSource::Inferred
        );
        requirements.validate().unwrap();
    }

    #[test]
    fn inferred_true_cannot_be_weakened_by_declared_false() {
        let declared = SkillRequirements {
            needs_network: declared_flag(Some(false)),
            ..SkillRequirements::unknown()
        };
        let inferred = SkillRequirements {
            needs_network: FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Inferred,
            },
            ..SkillRequirements::unknown()
        };
        let merged = SkillRequirements::merge(&declared, &inferred);
        assert_eq!(merged.needs_network.value, RequirementFlag::True);
        assert_eq!(merged.needs_network.source, RequirementSource::Inferred);
    }

    #[test]
    fn public_schema_rejects_unknown_fields_and_unsafe_values() {
        assert!(parse(serde_json::json!({"surprise": true})).is_err());
        assert!(SkillRequirements::from_public_json(Some(
            br#"{"schema_version":2,"requirements":{}}"#
        ))
        .is_err());
        assert!(parse(serde_json::json!({"required_binaries": ["../git"]})).is_err());
        assert!(parse(serde_json::json!({"required_binaries": [".."]})).is_err());
        assert!(parse(serde_json::json!({"required_environment": ["A=B"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["../secret"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["a//b"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["./a"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["a/./b"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["models/my file"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["models/a:b"]})).is_err());
        assert!(parse(serde_json::json!({"required_runtime_assets": ["资料/index"]})).is_err());
        assert!(parse(serde_json::json!({"supported_platforms": ["darwin"]})).is_err());
        assert!(parse(serde_json::json!({
            "required_binaries": (0..65).map(|index| format!("bin{index}")).collect::<Vec<_>>()
        }))
        .is_err());
        assert!(parse(serde_json::json!({
            "minimum_runtime_version": "x".repeat(65)
        }))
        .is_err());
    }

    #[test]
    fn serialized_requirement_state_rejects_inconsistent_provenance() {
        let mut requirements = SkillRequirements {
            needs_network: FlagRequirement {
                value: RequirementFlag::True,
                source: RequirementSource::Unknown,
            },
            ..SkillRequirements::default()
        };
        assert!(requirements.validate().is_err());

        requirements = SkillRequirements::default();
        requirements.required_binaries = ListRequirement {
            values: vec!["python3".to_string()],
            source: RequirementSource::Unknown,
        };
        assert!(requirements.validate().is_err());

        requirements = SkillRequirements::default();
        requirements.needs_ssh = FlagRequirement {
            value: RequirementFlag::False,
            source: RequirementSource::Inferred,
        };
        assert!(requirements.validate().is_err());

        requirements = SkillRequirements::default();
        requirements.restart_required = FlagRequirement {
            value: RequirementFlag::False,
            source: RequirementSource::Declared,
        };
        assert!(requirements.validate().is_err());
    }
}
