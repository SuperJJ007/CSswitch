use serde::{Deserialize, Serialize};

use super::model::SkillId;

pub(crate) type SkillResult<T> = Result<T, SkillManagerError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SkillErrorCode {
    InvalidSource,
    UnsafePath,
    UnsupportedFileType,
    HardlinkRejected,
    LimitExceeded,
    SourceChanged,
    InvalidManifest,
    IoFailed,
    InventoryInvalid,
    ManagerBusy,
    StoreConflict,
    DowngradeConfirmationRequired,
    AtomicCommitFailed,
    CommitDurabilityUncertain,
    SkillNotFound,
    RevealRejected,
    Internal,
    DeploymentConflict,
    CompatibilityCatalogInvalid,
    CompatibilityUnsupported,
    CompatibilityAcknowledgmentRequired,
}

impl SkillErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSource => "INVALID_SOURCE",
            Self::UnsafePath => "UNSAFE_PATH",
            Self::UnsupportedFileType => "UNSUPPORTED_FILE_TYPE",
            Self::HardlinkRejected => "HARDLINK_REJECTED",
            Self::LimitExceeded => "LIMIT_EXCEEDED",
            Self::SourceChanged => "SOURCE_CHANGED",
            Self::InvalidManifest => "INVALID_MANIFEST",
            Self::IoFailed => "IO_FAILED",
            Self::InventoryInvalid => "INVENTORY_INVALID",
            Self::ManagerBusy => "MANAGER_BUSY",
            Self::StoreConflict => "STORE_CONFLICT",
            Self::DowngradeConfirmationRequired => "DOWNGRADE_CONFIRMATION_REQUIRED",
            Self::AtomicCommitFailed => "ATOMIC_COMMIT_FAILED",
            Self::CommitDurabilityUncertain => "COMMIT_DURABILITY_UNCERTAIN",
            Self::SkillNotFound => "SKILL_NOT_FOUND",
            Self::RevealRejected => "REVEAL_REJECTED",
            Self::Internal => "INTERNAL",
            Self::DeploymentConflict => "DEPLOYMENT_CONFLICT",
            Self::CompatibilityCatalogInvalid => "COMPATIBILITY_CATALOG_INVALID",
            Self::CompatibilityUnsupported => "COMPATIBILITY_UNSUPPORTED",
            Self::CompatibilityAcknowledgmentRequired => "COMPATIBILITY_ACKNOWLEDGMENT_REQUIRED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillManagerError {
    pub(crate) code: SkillErrorCode,
    pub(crate) message: String,
    pub(crate) remediation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) skill_id: Option<SkillId>,
}

impl SkillManagerError {
    pub(crate) fn new(
        code: SkillErrorCode,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            remediation: remediation.into(),
            skill_id: None,
        }
    }

    pub(crate) fn safe_io() -> Self {
        Self::new(
            SkillErrorCode::IoFailed,
            "无法安全读取 Skill 来源",
            "请确认目录可读、未被其他进程修改，然后重试",
        )
    }

    pub(crate) fn with_skill_id(mut self, skill_id: SkillId) -> Self {
        self.skill_id = Some(skill_id);
        self
    }
}

impl std::fmt::Display for SkillManagerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SkillManagerError {}
