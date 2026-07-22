// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// macOS 链接器对齐段警告（tract-onnx 固有，可安全忽略）
#![allow(linker_messages)]

fn main() {
    mdgo_lib::run()
}
