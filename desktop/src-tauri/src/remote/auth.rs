use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::types::{RemoteAuthMethod, RemoteHostProfile};

#[derive(Debug, Clone, Default)]
pub struct SshAuthPlan {
    pub args: Vec<String>,
    #[allow(dead_code)]
    pub env: Vec<(String, String)>,
    #[allow(dead_code)]
    pub default_key_paths: Vec<String>,
    #[allow(dead_code)]
    pub interactive: bool,
    #[allow(dead_code)]
    pub remember_connection: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AuthRuntimeOptions {
    pub askpass_path: Option<String>,
    pub askpass_session_dir: Option<String>,
    pub askpass_env: Vec<(String, String)>,
    pub control_path: Option<String>,
    pub default_ssh_dir: Option<PathBuf>,
}

impl AuthRuntimeOptions {
    pub fn default_for(profile: &RemoteHostProfile) -> Self {
        Self {
            askpass_path: None,
            askpass_session_dir: None,
            askpass_env: Vec::new(),
            control_path: default_control_path(profile),
            default_ssh_dir: dirs::home_dir().map(|home| home.join(".ssh")),
        }
    }

    #[cfg(test)]
    pub fn test() -> Self {
        Self {
            askpass_path: Some("csswitch-ssh-askpass".to_string()),
            askpass_session_dir: Some("csswitch-askpass-session".to_string()),
            askpass_env: Vec::new(),
            control_path: Some("csswitch-control.sock".to_string()),
            default_ssh_dir: None,
        }
    }
}

impl SshAuthPlan {
    pub fn from_profile(profile: &RemoteHostProfile, runtime: AuthRuntimeOptions) -> Self {
        let mut args = Vec::new();
        let mut env = Vec::new();
        let mut default_key_paths: Vec<String> = Vec::new();
        let mut askpass_key_path: Option<String> = None;

        let (interactive, remember_connection) = match &profile.auth_method {
            RemoteAuthMethod::SshAgent => (false, false),
            RemoteAuthMethod::Recommended {
                use_default_key_files,
                allow_password,
                allow_verification_code,
                remember_connection,
                strict,
                ..
            } => {
                if *use_default_key_files {
                    default_key_paths =
                        collect_default_key_paths(runtime.default_ssh_dir.as_deref());
                    for path in &default_key_paths {
                        push_option_value(&mut args, "-i", path);
                    }
                }
                if *strict {
                    push_ssh_option(&mut args, "IdentitiesOnly=yes");
                }
                (
                    *allow_password || *allow_verification_code,
                    *remember_connection,
                )
            }
            RemoteAuthMethod::Password {
                remember_connection,
                ..
            } => (true, *remember_connection),
            RemoteAuthMethod::KeyFile {
                path,
                save_key_password,
                allow_password_fallback,
                allow_verification_code,
                remember_connection,
                ..
            } => {
                push_option_value(&mut args, "-i", path);
                askpass_key_path = Some(path.clone());
                if !allow_password_fallback {
                    push_ssh_option(&mut args, "IdentitiesOnly=yes");
                }
                (
                    *save_key_password || *allow_password_fallback || *allow_verification_code,
                    *remember_connection,
                )
            }
        };

        if interactive {
            push_ssh_option(&mut args, "BatchMode=no");
            push_ssh_option(&mut args, "NumberOfPasswordPrompts=3");
            if let Some(askpass_path) = runtime.askpass_path {
                env.push(("SSH_ASKPASS".to_string(), askpass_path));
                env.push(("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()));
                env.push(("DISPLAY".to_string(), "csswitch".to_string()));
                env.extend(runtime.askpass_env);
            }
            if let Some(session_dir) = runtime.askpass_session_dir {
                env.push(("CSSWITCH_ASKPASS_DIR".to_string(), session_dir));
                env.push(("CSSWITCH_ASKPASS_PROFILE".to_string(), profile.id.clone()));
            }
            if let Some(key_path) = askpass_key_path {
                env.push(("CSSWITCH_ASKPASS_KEY_PATH".to_string(), key_path));
            }
        } else {
            push_ssh_option(&mut args, "BatchMode=yes");
            push_ssh_option(&mut args, "NumberOfPasswordPrompts=0");
        }

        if remember_connection {
            if let Some(control_path) = runtime.control_path {
                push_ssh_option(&mut args, "ControlMaster=auto");
                push_ssh_option(&mut args, "ControlPersist=10m");
                push_ssh_option(&mut args, &format!("ControlPath={control_path}"));
            }
        }

        if profile.ssh_options.legacy_compat {
            push_ssh_option(&mut args, "HostKeyAlgorithms=+ssh-rsa");
            push_ssh_option(&mut args, "PubkeyAcceptedAlgorithms=+ssh-rsa");
            push_ssh_option(&mut args, "KexAlgorithms=+diffie-hellman-group14-sha1");
        }

        Self {
            args,
            env,
            default_key_paths,
            interactive,
            remember_connection,
        }
    }
}

fn push_option_value(args: &mut Vec<String>, option: &str, value: &str) {
    args.push(option.to_string());
    args.push(value.to_string());
}

fn push_ssh_option(args: &mut Vec<String>, option: &str) {
    push_option_value(args, "-o", option);
}

#[cfg_attr(windows, allow(dead_code))]
pub fn control_path_for(profile: &RemoteHostProfile) -> PathBuf {
    let raw = format!("{}@{}:{}", profile.username, profile.host, profile.port);
    let hash = Sha256::digest(raw.as_bytes());
    crate::config::default_dir()
        .join("ssh-control")
        .join(format!("{hash:x}.sock"))
}

#[cfg(windows)]
fn default_control_path(_profile: &RemoteHostProfile) -> Option<String> {
    None
}

#[cfg(not(windows))]
fn default_control_path(profile: &RemoteHostProfile) -> Option<String> {
    Some(control_path_for(profile).to_string_lossy().into_owned())
}

fn collect_default_key_paths(ssh_dir: Option<&Path>) -> Vec<String> {
    let Some(ssh_dir) = ssh_dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(ssh_dir) else {
        return Vec::new();
    };
    let names =
        entries.filter_map(|entry| entry.ok()?.path().file_name()?.to_str().map(str::to_string));
    order_default_key_names(names)
        .into_iter()
        .map(|name| ssh_dir.join(name).to_string_lossy().into_owned())
        .collect()
}

#[allow(dead_code)]
pub fn order_default_key_names<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut names: Vec<String> = names
        .into_iter()
        .map(Into::into)
        .filter(|name| name.starts_with("id_"))
        .filter(|name| !name.ends_with(".pub"))
        .collect();
    let preferred = ["id_ed25519", "id_ecdsa", "id_rsa"];
    let mut ordered = Vec::new();
    for preferred_name in preferred {
        if let Some(pos) = names.iter().position(|name| name == preferred_name) {
            ordered.push(names.remove(pos));
        }
    }
    names.sort();
    ordered.extend(names);
    ordered
}

#[cfg(test)]
mod tests {
    use super::super::types::{RemoteAuthMethod, RemoteHostProfile, RemoteSshAdvancedOptions};
    use super::*;
    use std::path::PathBuf;

    fn sample_profile(auth_method: RemoteAuthMethod) -> RemoteHostProfile {
        RemoteHostProfile {
            id: "profile-1".to_string(),
            name: "Test".to_string(),
            kind: super::super::types::RemoteTargetKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            distribution: None,
            username: "ubuntu".to_string(),
            auth_method,
            helper_path: "/usr/local/bin/csswitch-helper".to_string(),
            last_connected: None,
            ssh_options: RemoteSshAdvancedOptions::default(),
            transient_password: None,
        }
    }

    fn temp_ssh_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-auth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_key_candidates_prefer_modern_keys() {
        let names = vec!["id_rsa", "id_custom", "not_a_key", "id_ed25519", "id_ecdsa"];

        let ordered = order_default_key_names(names);

        assert_eq!(
            ordered,
            vec!["id_ed25519", "id_ecdsa", "id_rsa", "id_custom"]
        );
    }

    #[test]
    fn recommended_auth_uses_default_keys_askpass_and_connection_reuse() {
        let ssh_dir = temp_ssh_dir();
        for name in ["id_rsa", "id_ed25519", "id_custom", "id_rsa.pub"] {
            std::fs::write(ssh_dir.join(name), "").unwrap();
        }
        let profile = sample_profile(RemoteAuthMethod::Recommended {
            use_saved_keys: true,
            use_default_key_files: true,
            allow_password: true,
            allow_verification_code: true,
            remember_connection: true,
            strict: false,
        });

        let plan = SshAuthPlan::from_profile(
            &profile,
            AuthRuntimeOptions {
                askpass_path: Some("C:/csswitch/csswitch-ssh-askpass.exe".to_string()),
                askpass_session_dir: Some("C:/Temp/csswitch-askpass".to_string()),
                askpass_env: Vec::new(),
                control_path: Some("C:/Temp/csswitch-control.sock".to_string()),
                default_ssh_dir: Some(ssh_dir.clone()),
            },
        );

        assert!(plan.args.contains(&"BatchMode=no".to_string()));
        assert!(plan.args.contains(&"NumberOfPasswordPrompts=3".to_string()));
        assert!(plan.args.contains(&"ControlMaster=auto".to_string()));
        assert_eq!(plan.default_key_paths.len(), 3);
        assert!(plan.default_key_paths[0].ends_with("id_ed25519"));
        assert!(plan.default_key_paths[1].ends_with("id_rsa"));
        assert!(plan.default_key_paths[2].ends_with("id_custom"));
        assert!(plan.env.iter().any(|(key, _)| key == "SSH_ASKPASS"));

        let _ = std::fs::remove_dir_all(ssh_dir);
    }

    #[test]
    fn key_file_without_fallback_stays_noninteractive() {
        let profile = sample_profile(RemoteAuthMethod::KeyFile {
            path: "~/.ssh/id_ed25519".to_string(),
            save_key_password: false,
            allow_password_fallback: false,
            allow_verification_code: false,
            remember_connection: false,
        });

        let plan = SshAuthPlan::from_profile(&profile, AuthRuntimeOptions::test());

        assert!(plan.args.contains(&"-i".to_string()));
        assert!(plan.args.contains(&"~/.ssh/id_ed25519".to_string()));
        assert!(plan.args.contains(&"BatchMode=yes".to_string()));
        assert!(plan.args.contains(&"NumberOfPasswordPrompts=0".to_string()));
    }

    #[test]
    fn legacy_compat_args_are_opt_in() {
        let mut profile = sample_profile(RemoteAuthMethod::SshAgent);
        let plan = SshAuthPlan::from_profile(&profile, AuthRuntimeOptions::test());
        assert!(!plan
            .args
            .contains(&"HostKeyAlgorithms=+ssh-rsa".to_string()));

        profile.ssh_options.legacy_compat = true;
        let plan = SshAuthPlan::from_profile(&profile, AuthRuntimeOptions::test());
        assert!(plan
            .args
            .contains(&"HostKeyAlgorithms=+ssh-rsa".to_string()));
    }
}
