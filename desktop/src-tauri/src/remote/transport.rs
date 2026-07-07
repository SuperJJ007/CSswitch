//! Unified remote helper transport dispatch.
//!
//! SSH and WSL targets share the same csswitch-helper JSON protocol. This
//! module keeps command handlers from branching on transport details.

use serde::de::DeserializeOwned;

use super::types::{RemoteError, RemoteHostProfile, RemoteTargetKind};
use super::{ssh, wsl};

pub fn run_helper_json_with_retry<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::run_helper_json_with_retry(profile, helper_args),
        RemoteTargetKind::Wsl => wsl::run_helper_json_with_retry(profile, helper_args),
    }
}

pub fn run_helper_json_stdin_with_retry<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::run_helper_json_stdin_with_retry(profile, helper_args),
        RemoteTargetKind::Wsl => wsl::run_helper_json_stdin_with_retry(profile, helper_args),
    }
}

pub fn run_helper_json_slow<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::run_helper_json_slow(profile, helper_args),
        RemoteTargetKind::Wsl => wsl::run_helper_json_slow(profile, helper_args),
    }
}

pub fn run_helper_install(profile: &RemoteHostProfile) -> Result<String, RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::run_helper_install(profile),
        RemoteTargetKind::Wsl => wsl::run_helper_install(profile),
    }
}

pub fn install_helper_from_stdin(
    profile: &RemoteHostProfile,
    helper_bytes: &[u8],
) -> Result<String, RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::install_helper_from_stdin(profile, helper_bytes),
        RemoteTargetKind::Wsl => wsl::install_helper_from_stdin(profile, helper_bytes),
    }
}

pub fn detect_remote_platform(
    profile: &RemoteHostProfile,
) -> Result<(String, String), RemoteError> {
    match profile.kind {
        RemoteTargetKind::Ssh => ssh::detect_remote_platform(profile),
        RemoteTargetKind::Wsl => wsl::detect_remote_platform(profile),
    }
}
