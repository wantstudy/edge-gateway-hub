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
            // 若 resource_dir 下不存在 daemon 二进制，壳以「纯 UI 模式」降级运行：
            // 前端按诚实空态呈现并给出真实原因（绝无 mock 回退）。
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

    // daemon 以「自己的数据目录」为工作目录：绝不继承安装目录（安装目录可能只读，
    // 且多用户共享），配置 / 队列 / 审计库一律落在 `%LOCALAPPDATA%` 下的应用数据目录。
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("[tauri-shell] 创建应用数据目录失败（{dir:?}）：{e}，回退到 resource_dir");
                resource_dir.clone()
            } else {
                dir
            }
        }
        Err(e) => {
            eprintln!("[tauri-shell] 无法定位 app_data_dir，回退到 resource_dir：{e}");
            resource_dir.clone()
        }
    };

    // 首次运行：若数据目录里没有 config.toml，用随包分发的示例配置初始化一份
    // （用户手写资产，之后由用户自行维护；已存在则绝不覆盖）。
    let config_path = data_dir.join("config.toml");
    if !config_path.exists() {
        let template = resource_dir.join("config.example.toml");
        if template.exists() {
            match std::fs::copy(&template, &config_path) {
                Ok(_) => eprintln!("[tauri-shell] 已用示例配置初始化：{config_path:?}"),
                Err(e) => eprintln!("[tauri-shell] 初始化配置失败（{template:?}）：{e}"),
            }
        } else {
            eprintln!("[tauri-shell] 未找到示例配置模板（{template:?}），由 daemon 自行处理缺省配置。");
        }
    }

    let mut command = std::process::Command::new(&daemon_path);
    command.current_dir(&data_dir);
    match command.spawn() {
        Ok(child) => eprintln!(
            "[tauri-shell] 已拉起 daemon 侧车 pid={}（工作目录 {:?}）",
            child.id(),
            data_dir
        ),
        Err(e) => eprintln!(
            "[tauri-shell] 拉起 daemon 侧车失败（{:?}）：{}",
            daemon_path, e
        ),
    }
}
