//! Remote SSH login secrets backed by the system credential store.
//!
//! Plaintext passwords, key passphrases, and verification codes must never be
//! written to `remote-hosts.json` or logs. This module only stores long-lived
//! login secrets in the OS credential store and uses stable, secret-free labels.

use sha2::{Digest, Sha256};

const SERVICE: &str = "CSSwitch";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind<'a> {
    Password,
    KeyPassword(&'a str),
}

pub fn credential_kind_from_parts<'a>(
    kind: &str,
    key_path: Option<&'a str>,
) -> Result<CredentialKind<'a>, String> {
    match kind {
        "password" => Ok(CredentialKind::Password),
        "keyPassword" => {
            let path = key_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "密钥文件路径不能为空".to_string())?;
            Ok(CredentialKind::KeyPassword(path))
        }
        _ => Err("未知登录信息类型".to_string()),
    }
}

pub fn credential_label(profile_id: &str, kind: CredentialKind<'_>) -> String {
    let profile_hash = hash_label_part(profile_id);
    match kind {
        CredentialKind::Password => format!("remote:{profile_hash}:password"),
        CredentialKind::KeyPassword(path) => {
            let path_hash = hash_label_part(path);
            format!("remote:{profile_hash}:key-password:{path_hash}")
        }
    }
}

fn hash_label_part(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[allow(dead_code)]
trait CredentialStore {
    fn save(&self, label: &str, secret: &str) -> Result<(), String>;
    fn read(&self, label: &str) -> Option<String>;
    fn delete(&self, label: &str) -> Result<(), String>;
}

struct SystemCredentialStore;

impl CredentialStore for SystemCredentialStore {
    fn save(&self, label: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, label)
            .map_err(|e| format!("无法打开系统安全存储：{e}"))?
            .set_password(secret)
            .map_err(|e| format!("保存登录信息失败：{e}"))
    }

    fn read(&self, label: &str) -> Option<String> {
        keyring::Entry::new(SERVICE, label)
            .ok()?
            .get_password()
            .ok()
    }

    fn delete(&self, label: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, label)
            .map_err(|e| format!("无法打开系统安全存储：{e}"))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("删除登录信息失败：{e}")),
        }
    }
}

fn save_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
    secret: &str,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    store.save(&label, secret)
}

#[allow(dead_code)]
fn read_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Option<String> {
    let label = credential_label(profile_id, kind);
    store.read(&label)
}

fn delete_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    store.delete(&label)
}

pub fn save_secret(profile_id: &str, kind: CredentialKind<'_>, secret: &str) -> Result<(), String> {
    save_secret_with_store(&SystemCredentialStore, profile_id, kind, secret)
}

#[allow(dead_code)]
pub fn read_secret(profile_id: &str, kind: CredentialKind<'_>) -> Option<String> {
    read_secret_with_store(&SystemCredentialStore, profile_id, kind)
}

pub fn delete_secret(profile_id: &str, kind: CredentialKind<'_>) -> Result<(), String> {
    delete_secret_with_store(&SystemCredentialStore, profile_id, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryCredentialStore {
        values: Mutex<HashMap<String, String>>,
    }

    impl CredentialStore for MemoryCredentialStore {
        fn save(&self, label: &str, secret: &str) -> Result<(), String> {
            self.values
                .lock()
                .unwrap()
                .insert(label.to_string(), secret.to_string());
            Ok(())
        }

        fn read(&self, label: &str) -> Option<String> {
            self.values.lock().unwrap().get(label).cloned()
        }

        fn delete(&self, label: &str) -> Result<(), String> {
            self.values.lock().unwrap().remove(label);
            Ok(())
        }
    }

    #[test]
    fn credential_labels_are_stable_and_secret_free() {
        let profile_id = "host.example.com/root/password-ish";
        let password = credential_label(profile_id, CredentialKind::Password);
        let key_pass = credential_label(
            profile_id,
            CredentialKind::KeyPassword("C:/Users/me/.ssh/id_ed25519"),
        );

        assert!(password.starts_with("remote:"));
        assert!(password.ends_with(":password"));
        assert!(!password.contains(profile_id));
        assert!(key_pass.starts_with("remote:"));
        assert!(key_pass.contains(":key-password:"));
        assert!(!key_pass.contains(profile_id));
        assert!(!key_pass.contains("id_ed25519"));
        assert!(!key_pass.contains(".ssh"));
        assert_eq!(password.len(), "remote:".len() + 64 + ":password".len());
        assert_eq!(
            key_pass.len(),
            "remote:".len() + 64 + ":key-password:".len() + 64
        );
    }

    #[test]
    fn credential_kind_parser_rejects_unknown_kind() {
        assert!(credential_kind_from_parts("verificationCode", None).is_err());
    }

    #[test]
    fn key_password_kind_requires_key_path() {
        assert!(credential_kind_from_parts("keyPassword", None).is_err());
        assert!(credential_kind_from_parts("keyPassword", Some("   ")).is_err());
        assert_eq!(
            credential_kind_from_parts("keyPassword", Some(" ~/.ssh/id_ed25519 ")).unwrap(),
            CredentialKind::KeyPassword("~/.ssh/id_ed25519")
        );
    }

    #[test]
    fn memory_store_roundtrip_does_not_touch_system_credentials() {
        let store = MemoryCredentialStore::default();
        let kind = CredentialKind::Password;

        save_secret_with_store(&store, "p2", kind, "server-password").unwrap();
        assert_eq!(
            read_secret_with_store(&store, "p2", kind).as_deref(),
            Some("server-password")
        );

        delete_secret_with_store(&store, "p2", kind).unwrap();
        assert!(read_secret_with_store(&store, "p2", kind).is_none());
    }
}
