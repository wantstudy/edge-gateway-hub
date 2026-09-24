#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;
use tauri::WebviewUrl;
use tauri::WebviewWindowBuilder;

/// daemon 侧车可执行文件名（随平台变化）。
const DAEMON_EXE: &str = if cfg!(windows) {
    "iot-daq-daemon.exe"
} else {
    "iot-daq-daemon"
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // 尝试拉起 daemon 侧车。授权判定始终在 daemon Rust 侧，本壳只承载 UI 与转发。
            // 若 resource_dir 下不存在 daemon 二进制（尚未提供），壳以「纯 UI 模式」降级运行，
            // web-console 走其内置 mock / 受限后端（设计上授权判定不在此层）。
            spawn_daemon_sidecar(app.handle());

            // 主窗口：加载 web-console 前端（frontendDist 指向 ../../web-console/dist）。
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("IoT-DAQ 网关控制台")
                .inner_size(1366.0, 768.0)
                .min_inner_size(1024.0, 640.0)
                .resizable(true)
                .fullscreen(false)
                .center()
                .build()
                .expect("创建主窗口失败");

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 Tauri 运行时失败");
}

/// 从 `resource_dir()` 拉起 daemon 侧车（若存在）。失败仅记录，不阻断 UI。
fn spawn_daemon_sidecar(app: &tauri::AppHandle) {
    let resource_dir = match app.path().resource_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[tauri-shell] 无法定位 resource_dir，跳过 daemon 拉起：{e}");
            return;
        }
    };
    let daemon_path = resource_dir.join(DAEMON_EXE);
    if !daemon_path.exists() {
        eprintln!(
            "[tauri-shell] 未找到 daemon 侧车（{:?}），以纯 UI 模式运行。",
            daemon_path
        );
        return;
    }

    match std::process::Command::new(&daemon_path).spawn() {
        Ok(child) => eprintln!("[tauri-shell] 已拉起 daemon 侧车 pid={}", child.id()),
        Err(e) => eprintln!(
            "[tauri-shell] 拉起 daemon 侧车失败（{:?}）：{}",
            daemon_path, e
        ),
    }
}
