//! CSSwitch 桌面 app 后端（进程管家 + 远程服务器管理）。
//!
//! 职责：管理「翻译代理」与「沙箱 Science」两个子进程的生命周期（本地 macOS 模式），
//! 或通过 SSH 管理远程 Linux 服务器上的同名服务（远程模式）；读写
//! `~/.csswitch/config.json`（多 profile 形态）；把第三方 key 以【环境变量】注入代理子进程
//! （绝不进 argv）；探活；把沙箱 URL 交系统浏览器打开。已验证的越权/翻译逻辑仍留在
//! Python/Node/shell 里被当作子进程调用，以保住铁律护栏与已验证行为。
//!
//! 运行行为由生效 profile 的 `template_id` 经 [`templates`] 注册表派生出 adapter
//! （deepseek | qwen | relay），再传给 python 代理 `--provider`。
//!
//! 跨平台适配：macOS 代码用 `#[cfg(target_os = "macos")]` 守卫；Windows 不支持本地模式
//! （缺少 Claude Science.app / zsh / pkill 等），本地操作返回明确错误。
//!
//! 铁律相关：key 只在内存与 0600 的 config.json；回显前端只给掩码；沙箱端口/目录护栏
//! 由被调脚本负责（对 8765 与真实目录失败关闭）；退 app 默认停代理、保留沙箱。

mod config;
mod config_legacy;
mod lifecycle;
// 虚拟 OAuth 伪造器仅 macOS + desktop feature 需要。
#[cfg(all(target_os = "macos", feature = "desktop"))]
mod oauth_forge;
mod proc;
mod scratch;
mod templates;
// 跨平台文件权限抽象：Unix 下提供真实的 0600/0700 权限，Windows 下为 no-op。
mod fs_ext;
// 远程服务器管理：SSH 连接、Profile 存储、远程命令（跨平台，无 Tauri 依赖）。
pub mod remote;
// 远程 Tauri commands — 仅 desktop feature 编译。
#[cfg(feature = "desktop")]
mod remote_commands;

// ---- desktop feature gate ----
// tauri 相关代码在 lib_tauri.rs 中，仅 desktop feature 启用时编译。
// csswitch-helper 编译 (`--no-default-features`) 时跳过此 include。
#[cfg(feature = "desktop")]
include!("lib_tauri.rs");
