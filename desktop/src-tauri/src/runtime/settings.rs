use std::path::{Path, PathBuf};

fn system_ssh_config_path_for_home(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能启用系统 SSH 配置。".into());
    }
    let config = home.join(".ssh").join("config");
    if !config.is_file() {
        return Err("未找到系统 ~/.ssh/config，不能启用系统 SSH 配置。".into());
    }
    Ok(config)
}

pub(crate) fn system_ssh_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能启用系统 SSH 配置。")?;
    system_ssh_config_path_for_home(&home)
}

pub(crate) fn validate_runtime_ports(proxy_port: u16, sandbox_port: u16) -> Result<(), String> {
    crate::config::validate_runtime_ports(proxy_port, sandbox_port)?;
    let preview_port = sandbox_port
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if preview_port == 8765 {
        return Err("沙箱预览端口会命中真实 Science 保留端口 8765。".into());
    }
    if preview_port == proxy_port {
        return Err("代理端口不能与沙箱预览端口相同。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{system_ssh_config_path_for_home, validate_runtime_ports};

    #[test]
    fn validate_runtime_ports_rejects_reserved_real_science_port() {
        assert!(validate_runtime_ports(8765, 18991).is_err());
        assert!(validate_runtime_ports(18991, 8765).is_err());
    }

    #[test]
    fn validate_runtime_ports_rejects_zero_and_same_port() {
        assert!(validate_runtime_ports(0, 18991).is_err());
        assert!(validate_runtime_ports(18991, 0).is_err());
        assert!(validate_runtime_ports(18991, 18991).is_err());
        assert!(validate_runtime_ports(8991, 8990).is_err());
        assert!(validate_runtime_ports(18991, 8764).is_err());
        assert!(validate_runtime_ports(18991, u16::MAX).is_err());
        assert!(
            crate::config::validate_runtime_ports(8991, 8990).is_ok(),
            "legacy config must remain readable so the UI can repair it"
        );
    }

    #[test]
    fn validate_runtime_ports_accepts_distinct_nonreserved_ports() {
        assert!(validate_runtime_ports(18991, 18992).is_ok());
    }

    #[test]
    fn system_ssh_config_requires_an_absolute_home_and_regular_target() {
        assert!(system_ssh_config_path_for_home(std::path::Path::new("relative-home")).is_err());
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        assert!(system_ssh_config_path_for_home(&home).is_err());
        std::fs::write(home.join(".ssh/config"), "Host test\n").unwrap();
        assert_eq!(
            system_ssh_config_path_for_home(&home).unwrap(),
            home.join(".ssh/config")
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
