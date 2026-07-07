//! Remote SSH login secrets backed by the system credential store.
//!
//! Plaintext passwords, key passphrases, and verification codes must never be
//! written to `remote-hosts.json` or logs. This module prefers the OS
//! credential store and falls back to an app-local encrypted file when the OS
//! store is unavailable.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SERVICE: &str = "CSSwitch";
const LOCAL_SECRET_FILE: &str = "remote-secrets.json";
const LOCAL_KEY_FILE: &str = "encryption.key";
const LOCAL_KEY_NAME: &str = "REMOTE_SECRET_ENCRYPTION_KEY";
const LOCAL_HKDF_INFO: &[u8] = b"csswitch:remote-login-secrets:v1";

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

struct LocalEncryptedCredentialStore {
    dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalSecretFile {
    version: u32,
    #[serde(default)]
    items: HashMap<String, String>,
}

impl Default for LocalSecretFile {
    fn default() -> Self {
        Self {
            version: 1,
            items: HashMap::new(),
        }
    }
}

impl LocalEncryptedCredentialStore {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn default() -> Self {
        Self::new(crate::config::default_dir())
    }

    fn secrets_path(&self) -> PathBuf {
        self.dir.join(LOCAL_SECRET_FILE)
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(LOCAL_KEY_FILE)
    }

    fn ensure_dir(&self) -> Result<(), String> {
        crate::config::assert_not_symlink(&self.dir)
            .map_err(|e| format!("本机密码目录安全拒绝：{e}"))?;
        fs::create_dir_all(&self.dir)
            .map_err(|e| format!("无法创建本机密码目录 {}：{e}", self.dir.display()))?;
        crate::fs_ext::set_file_permissions(&self.dir, 0o700)
            .map_err(|e| format!("设置本机密码目录权限失败：{e}"))
    }

    fn load_file(&self) -> Result<LocalSecretFile, String> {
        let path = self.secrets_path();
        crate::config::assert_not_symlink(&path)
            .map_err(|e| format!("本机密码文件安全拒绝：{e}"))?;
        if !path.exists() {
            return Ok(LocalSecretFile::default());
        }
        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("读取本机密码文件 {} 失败：{e}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(LocalSecretFile::default());
        }
        serde_json::from_str(&raw).map_err(|e| format!("解析本机密码文件失败：{e}"))
    }

    fn save_file(&self, file: &LocalSecretFile) -> Result<(), String> {
        self.ensure_dir()?;
        let path = self.secrets_path();
        crate::config::assert_not_symlink(&path)
            .map_err(|e| format!("本机密码文件安全拒绝：{e}"))?;
        let json =
            serde_json::to_vec_pretty(file).map_err(|e| format!("序列化本机密码文件失败：{e}"))?;
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(&tmp, json).map_err(|e| format!("写入本机密码临时文件失败：{e}"))?;
        crate::fs_ext::set_file_permissions(&tmp, 0o600)
            .map_err(|e| format!("设置本机密码文件权限失败：{e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("替换本机密码文件失败：{e}"))?;
        crate::fs_ext::set_file_permissions(&path, 0o600)
            .map_err(|e| format!("确认本机密码文件权限失败：{e}"))
    }

    fn read_or_create_key(&self) -> Result<String, String> {
        self.ensure_dir()?;
        let path = self.key_path();
        crate::config::assert_not_symlink(&path)
            .map_err(|e| format!("本机密码密钥文件安全拒绝：{e}"))?;
        if path.exists() {
            let raw =
                fs::read_to_string(&path).map_err(|e| format!("读取本机密码密钥文件失败：{e}"))?;
            if let Some(value) = parse_local_key(&raw) {
                return Ok(value);
            }
        }

        let key = random_key_b64();
        let body = format!("{LOCAL_KEY_NAME}={key}\n");
        fs::write(&path, body).map_err(|e| format!("写入本机密码密钥文件失败：{e}"))?;
        crate::fs_ext::set_file_permissions(&path, 0o600)
            .map_err(|e| format!("设置本机密码密钥文件权限失败：{e}"))?;
        Ok(key)
    }

    fn read_key(&self) -> Result<Option<String>, String> {
        crate::config::assert_not_symlink(&self.dir)
            .map_err(|e| format!("本机密码目录安全拒绝：{e}"))?;
        if !self.dir.exists() {
            return Ok(None);
        }

        let path = self.key_path();
        crate::config::assert_not_symlink(&path)
            .map_err(|e| format!("本机密码密钥文件安全拒绝：{e}"))?;
        if !path.exists() {
            return Ok(None);
        }

        let raw =
            fs::read_to_string(&path).map_err(|e| format!("读取本机密码密钥文件失败：{e}"))?;
        Ok(parse_local_key(&raw))
    }
}

impl CredentialStore for LocalEncryptedCredentialStore {
    fn save(&self, label: &str, secret: &str) -> Result<(), String> {
        let key = self.read_or_create_key()?;
        let encrypted = encrypt_local_secret(label, secret.as_bytes(), &key)?;
        let mut file = self.load_file()?;
        file.version = 1;
        file.items.insert(label.to_string(), encrypted);
        self.save_file(&file)
    }

    fn read(&self, label: &str) -> Option<String> {
        let key = self.read_key().ok()??;
        let file = self.load_file().ok()?;
        let encrypted = file.items.get(label)?;
        let plaintext = decrypt_local_secret(label, encrypted, &key).ok()?;
        String::from_utf8(plaintext).ok()
    }

    fn delete(&self, label: &str) -> Result<(), String> {
        let path = self.secrets_path();
        crate::config::assert_not_symlink(&path)
            .map_err(|e| format!("本机密码文件安全拒绝：{e}"))?;
        if !path.exists() {
            return Ok(());
        }

        let mut file = self.load_file()?;
        if file.items.remove(label).is_none() {
            return Ok(());
        }
        if file.items.is_empty() {
            fs::remove_file(&path).map_err(|e| format!("删除本机密码文件失败：{e}"))
        } else {
            self.save_file(&file)
        }
    }
}

fn parse_local_key(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix(&format!("{LOCAL_KEY_NAME}=")) {
            let value = value.trim();
            if B64.decode(value).map(|b| b.len() >= 16).unwrap_or(false) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn random_key_b64() -> String {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    B64.encode(key)
}

fn derive_local_key(root_key_b64: &str) -> Result<[u8; 32], String> {
    let ikm = B64
        .decode(root_key_b64.trim())
        .map_err(|e| format!("本机密码密钥不是合法 base64：{e}"))?;
    if ikm.len() < 16 {
        return Err("本机密码密钥长度不足".to_string());
    }
    let hk = Hkdf::<Sha256>::new(Some(&[]), &ikm);
    let mut out = [0u8; 32];
    hk.expand(LOCAL_HKDF_INFO, &mut out)
        .map_err(|_| "本机密码密钥派生失败".to_string())?;
    Ok(out)
}

fn encrypt_local_secret(
    label: &str,
    plaintext: &[u8],
    root_key_b64: &str,
) -> Result<String, String> {
    let key = derive_local_key(root_key_b64)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| "加密本机密码失败".to_string())?;
    let mut body = nonce.to_vec();
    body.extend(ciphertext);
    Ok(format!("v1:{}", B64.encode(body)))
}

fn decrypt_local_secret(label: &str, body: &str, root_key_b64: &str) -> Result<Vec<u8>, String> {
    let raw = B64
        .decode(body.strip_prefix("v1:").ok_or("本机密码密文缺少 v1 前缀")?)
        .map_err(|e| format!("本机密码密文不是合法 base64：{e}"))?;
    if raw.len() < 12 + 16 {
        return Err("本机密码密文过短".to_string());
    }
    let (nonce, ciphertext) = raw.split_at(12);
    let key = derive_local_key(root_key_b64)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| "解密本机密码失败".to_string())
}

#[cfg(test)]
fn save_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
    secret: &str,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    store.save(&label, secret)
}

#[cfg(test)]
fn read_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Option<String> {
    let label = credential_label(profile_id, kind);
    store.read(&label)
}

#[cfg(test)]
fn delete_secret_with_store(
    store: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    store.delete(&label)
}

fn save_secret_with_fallback(
    system: &impl CredentialStore,
    fallback: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
    secret: &str,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    match system.save(&label, secret) {
        Ok(()) => {
            let _ = fallback.delete(&label);
            Ok(())
        }
        Err(system_error) => fallback.save(&label, secret).map_err(|fallback_error| {
            format!("系统安全存储失败：{system_error}；本机加密存储也失败：{fallback_error}")
        }),
    }
}

fn read_secret_with_fallback(
    system: &impl CredentialStore,
    fallback: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Option<String> {
    let label = credential_label(profile_id, kind);
    system.read(&label).or_else(|| fallback.read(&label))
}

fn delete_secret_with_fallback(
    system: &impl CredentialStore,
    fallback: &impl CredentialStore,
    profile_id: &str,
    kind: CredentialKind<'_>,
) -> Result<(), String> {
    let label = credential_label(profile_id, kind);
    let system_result = system.delete(&label);
    let fallback_result = fallback.delete(&label);
    match (system_result, fallback_result) {
        (Ok(()), _) | (_, Ok(())) => Ok(()),
        (Err(system_error), Err(fallback_error)) => Err(format!(
            "系统安全存储删除失败：{system_error}；本机加密存储删除失败：{fallback_error}"
        )),
    }
}

pub fn save_secret(profile_id: &str, kind: CredentialKind<'_>, secret: &str) -> Result<(), String> {
    save_secret_with_fallback(
        &SystemCredentialStore,
        &LocalEncryptedCredentialStore::default(),
        profile_id,
        kind,
        secret,
    )
}

#[allow(dead_code)]
pub fn read_secret(profile_id: &str, kind: CredentialKind<'_>) -> Option<String> {
    read_secret_with_fallback(
        &SystemCredentialStore,
        &LocalEncryptedCredentialStore::default(),
        profile_id,
        kind,
    )
}

pub fn delete_secret(profile_id: &str, kind: CredentialKind<'_>) -> Result<(), String> {
    delete_secret_with_fallback(
        &SystemCredentialStore,
        &LocalEncryptedCredentialStore::default(),
        profile_id,
        kind,
    )
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

    struct FailingCredentialStore;

    impl CredentialStore for FailingCredentialStore {
        fn save(&self, _label: &str, _secret: &str) -> Result<(), String> {
            Err("No default store has been set".to_string())
        }

        fn read(&self, _label: &str) -> Option<String> {
            None
        }

        fn delete(&self, _label: &str) -> Result<(), String> {
            Err("No default store has been set".to_string())
        }
    }

    fn temp_secret_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-remote-secrets-test-{}-{}",
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
    fn encrypted_fallback_roundtrip_when_system_store_is_unavailable() {
        let dir = temp_secret_dir();
        let system = FailingCredentialStore;
        let fallback = LocalEncryptedCredentialStore::new(dir.clone());

        save_secret_with_fallback(
            &system,
            &fallback,
            "profile-1",
            CredentialKind::Password,
            "server-password",
        )
        .unwrap();

        assert_eq!(
            read_secret_with_fallback(&system, &fallback, "profile-1", CredentialKind::Password)
                .as_deref(),
            Some("server-password")
        );

        let raw = std::fs::read_to_string(dir.join("remote-secrets.json")).unwrap();
        assert!(!raw.contains("server-password"));
        assert!(raw.contains("v1:"));
        assert!(dir.join("encryption.key").is_file());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encrypted_fallback_delete_works_even_when_system_store_is_unavailable() {
        let dir = temp_secret_dir();
        let system = FailingCredentialStore;
        let fallback = LocalEncryptedCredentialStore::new(dir.clone());
        let kind = CredentialKind::Password;

        save_secret_with_fallback(&system, &fallback, "profile-1", kind, "server-password")
            .unwrap();
        delete_secret_with_fallback(&system, &fallback, "profile-1", kind).unwrap();

        assert!(read_secret_with_fallback(&system, &fallback, "profile-1", kind).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encrypted_fallback_read_does_not_create_key_when_empty() {
        let dir = temp_secret_dir();
        let system = FailingCredentialStore;
        let fallback = LocalEncryptedCredentialStore::new(dir.clone());

        assert!(read_secret_with_fallback(
            &system,
            &fallback,
            "profile-1",
            CredentialKind::Password
        )
        .is_none());
        assert!(!dir.join("encryption.key").exists());
        assert!(!dir.join("remote-secrets.json").exists());

        let _ = std::fs::remove_dir_all(dir);
    }
}
