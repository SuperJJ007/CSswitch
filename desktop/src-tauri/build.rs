use std::{
    env,
    path::{Path, PathBuf},
};

fn main() {
    configure_bundled_proxy_dir();

    #[cfg(feature = "desktop")]
    tauri_build::build()
}

fn configure_bundled_proxy_dir() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required"));
    let proxy_dir = env::var_os("CSSWITCH_BUNDLED_PROXY_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                manifest_dir.join(path)
            }
        })
        .unwrap_or_else(|| manifest_dir.join("..").join("..").join("proxy"));
    let proxy_dir_str = proxy_dir.to_string_lossy().replace('\\', "/");

    println!("cargo:rerun-if-env-changed=CSSWITCH_BUNDLED_PROXY_DIR");
    for resource in [
        "csswitch_proxy.py",
        "dsml_shim.py",
        "provider_policy.py",
        "anthropic_compat.py",
    ] {
        require_bundled_proxy_file(&proxy_dir, resource);
        println!(
            "cargo:rerun-if-changed={}",
            proxy_dir.join(resource).display()
        );
    }
    println!("cargo:rustc-env=CSSWITCH_BUNDLED_PROXY_DIR={proxy_dir_str}");
}

fn require_bundled_proxy_file(proxy_dir: &Path, resource: &str) {
    let path = proxy_dir.join(resource);
    if !path.is_file() {
        panic!(
            "bundled proxy resource '{}' not found at {}; set CSSWITCH_BUNDLED_PROXY_DIR to the repository proxy directory",
            resource,
            path.display()
        );
    }
}
