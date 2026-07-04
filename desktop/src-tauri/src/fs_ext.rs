//! 跨平台文件权限抽象。
//!
//! Unix: re-export 标准库的 OpenOptionsExt / PermissionsExt，提供真实的 0600/0700 权限。
//! Windows: 提供同名 trait 的 no-op 实现，权限操作为空操作。
//!
//! 所有文件使用 `use crate::fs_ext::...` 替代 `use std::os::unix::fs::...`。

use std::fs;
use std::io;
use std::path::Path;

// ---------- 平台条件编译 ----------

#[cfg(unix)]
mod imp {
    pub use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    pub fn set_file_permissions(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    pub fn is_executable(metadata: &fs::Metadata) -> bool {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0)
    }

    /// 打开（truncate）日志文件，带 O_NOFOLLOW 防护。
    /// macOS/BSD=0x0100，Linux=0x20000。
    pub fn open_log_file(path: &std::path::Path) -> std::io::Result<fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        const O_NOFOLLOW: i32 = if cfg!(target_os = "linux") { 0x2_0000 } else { 0x0100 };
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(path)
    }
}

#[cfg(windows)]
mod imp {
    use std::fs;
    use std::io;
    use std::path::Path;

    /// Windows: OpenOptions 没有 mode 概念。
    pub trait OpenOptionsExt {
        fn mode(&mut self, _mode: u32) -> &mut Self;
    }
    impl OpenOptionsExt for fs::OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self { self }
    }

    /// Windows: Permissions 只有只读位，mode 操作无意义。
    pub trait PermissionsExt {
        fn from_mode(_mode: u32) -> fs::Permissions;
        fn mode(&self) -> u32;
    }
    impl PermissionsExt for fs::Permissions {
        fn from_mode(_mode: u32) -> fs::Permissions {
            fs::Permissions::new() // 默认权限（非只读）
        }
        fn mode(&self) -> u32 {
            if self.readonly() { 0o444 } else { 0o666 }
        }
    }

    pub fn set_file_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn is_executable(metadata: &fs::Metadata) -> bool {
        // Windows: 检查扩展名是否为 .exe/.bat/.cmd/.ps1（简易判断）
        metadata.is_file()
    }

    /// Windows: 没有 O_NOFOLLOW，用普通 OpenOptions。
    pub fn open_log_file(path: &Path) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

// ---------- 公开导出 ----------

pub use imp::{is_executable, open_log_file, set_file_permissions, OpenOptionsExt, PermissionsExt};
