// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::var_os("CSSWITCH_ASKPASS_MODE").is_some() {
        std::process::exit(desktop_lib::remote::askpass::run_cli());
    }
    desktop_lib::run()
}
