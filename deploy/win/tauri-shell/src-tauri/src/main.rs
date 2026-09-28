#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! IoT-DAQ Gateway 桌面壳（Tauri 2.x）。
//!
//! 职责边界：
//! 1. 承载网关侧 web-console（`frontendDist` → `../../web-console/dist`）；
//! 2. 以**侧车**方式拉起 `iot-daq-daemon`，并为其提供真实守护能力
//!    （崩溃自动重启 / 看门狗 / 启动失败保护），经 Tauri 命令
//!    `supervisor_status` / `supervisor_set` 暴露给前端（见 [`supervisor`]）；
//! 3. 首次运行就地生成**最小配置**（daemon 对「配置缺失」是 fail-fast）。
//!
//! ⚠️ 授权判定恒在 daemon 的 Rust 侧；本壳只承载 UI、拉起进程、转发命令。

mod supervisor;

use std::sync::Arc;

use supervisor::{Supervisor, SupervisorPatch, SupervisorStatus};

/// 首次运行就地生成的**最小配置**。
///
/// daemon 的默认配置路径是 `<工作目录>/config.toml`，而 `GatewayConfig::load` 对
/// 「文件不存在」是 fail-fast（退出码 1），因此该文件**必须存在**；但 `GatewayConfig`
/// 全字段带 `#[serde(default)]` + `Default`，最小内容即可正常启动。故安装包不再随包
/// 分发 `config.example.toml`——模板只作为开发参考留在仓库根目录。
const MINIMAL_CONFIG: &str = "\
# 本文件由 IoT-DAQ Gateway 桌面端首次运行时自动生成（已存在则绝不会被覆盖）。
# 采集点位 / 北向出口 / 告警 / 管理面账号均可在本文件中配置，改后重启程序生效。
# 完整字段示例见仓库根目录 config.example.toml。

[gateway]
gateway_id = \"gw-local\"
";

/// `supervisor_status`：读取守护真实状态（前端 `shell.ts::supervisorStatus`）。
#[tauri::command]
fn supervisor_status(state: tauri::State<'_, Arc<Supervisor>>) -> SupervisorStatus {
    state.status()
}

/// `supervisor_set`：写守护补丁，返回**写后真实状态**
/// （前端 `shell.ts::setSupervisor`，参数名 snake_case）。
#[tauri::command]
fn supervisor_set(
    state: tauri::State<'_, Arc<Supervisor>>,
    crash_restart: Option<bool>,
    watchdog: Option<bool>,
    boot_failure_guard: Option<bool>,
) -> SupervisorStatus {
    state.set(SupervisorPatch {
        crash_restart,
        watchdog,
        boot_failure_guard,
    })
}

/// `api_base`：壳实际选定的管理面基址（默认端口被占时经 [`supervisor::pick_mgmt_addr`]
/// 自动顺延后的真实结果；前端 `client.ts::resolveApiBase` 启动时调用一次）。
#[tauri::command]
fn api_base(state: tauri::State<'_, Arc<Supervisor>>) -> String {
    format!("http://{}", state.mgmt_addr())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            supervisor_status,
            supervisor_set,
            api_base
        ])
        .setup(|app| {
            use tauri::Manager as _;
            let handle = app.handle().clone();
            let data_dir = supervisor::resolve_data_dir(&handle);
            let daemon_path = supervisor::resolve_daemon_path(&handle);

            // 管理面端口被占用时自动顺延（最多 MGMT_PORT_PROBES 个）；全部占用
            // 回落 preferred（fail-closed），由 daemon 报真实 bind 错误。
            let preferred = Supervisor::mgmt_addr_from_env();
            let mgmt_addr = supervisor::pick_mgmt_addr(preferred, supervisor::MGMT_PORT_PROBES);

            // 数据目录里没有 config.toml 时就地生成最小配置（用户手写资产，存在则不覆盖）。
            let config_path = data_dir.join("config.toml");
            if !config_path.exists() {
                if let Err(err) = std::fs::write(&config_path, MINIMAL_CONFIG) {
                    eprintln!("[tauri-shell] 生成最小配置失败（{config_path:?}）：{err}");
                }
            }

            // 真实守护：拉起侧车 + 起守护线程（未随包分发 daemon 时 supported=false，
            // 前端按诚实空态呈现并给出真实原因，绝不回退 mock）。
            let sup = Supervisor::new(daemon_path, data_dir, mgmt_addr);
            sup.start();
            sup.log(&format!(
                "管理面地址已选定 {mgmt_addr}（首选 {preferred}；端口被占时自动顺延，探测上限 {} 个）",
                supervisor::MGMT_PORT_PROBES
            ));
            app.manage(sup);

            // 主窗口：加载 web-console 前端。
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("IoT-DAQ 网关控制台")
                .inner_size(1366.0, 768.0)
                .min_inner_size(1024.0, 640.0)
                .resizable(true)
                .fullscreen(false)
                .center()
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 Tauri 运行时失败");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小配置必须能被 daemon 的配置解析器接受（键存在且语法合法）。
    #[test]
    fn minimal_config_has_gateway_section() {
        assert!(
            MINIMAL_CONFIG.contains("[gateway]"),
            "minimal config must declare [gateway]"
        );
        assert!(
            MINIMAL_CONFIG.contains("gateway_id"),
            "minimal config must set gateway_id"
        );
    }
}
