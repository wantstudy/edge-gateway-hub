//! `iot-daq-daemon` —— headless 守护进程入口（task 33）。
//!
//! 职责边界（薄壳，不做业务）：
//! 1. 解析启动参数（配置路径 / 管理面绑定地址，均可被环境变量覆盖）；
//! 2. 预加载配置 → 装配 [`mgmt::MgmtState`] 并启动管理面 HTTP（REST + SSE + 静态）；
//! 3. 交棒 [`bootstrap::BootstrapBuilder::run`]（生命周期 / 热重载 / 调度 / 看门狗 /
//!    信号处理全在 bootstrap 内），停机后按其结果决定进程退出码。
//!
//! 环境变量：
//! - `IOT_DAQ_CONFIG`：配置文件路径（默认 `./config.toml`，`--config` 可覆盖）；
//! - `IOT_DAQ_MGMT_BIND`：管理面监听地址（默认 `127.0.0.1:8080`，`--bind` 可覆盖）；
//! - `IOT_DAQ_WEB_DIST`：web-console 静态资源根目录（mgmt 模块读取，默认 `./web-dist`）。
//!
//! 退出码：`0` = 优雅停机；`1` = 启动失败（配置加载 / 端口绑定 / bootstrap 装配错误）。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use daemon::bootstrap::{BootstrapBuilder, DaemonShared};
use daemon::config::{ConfigShared, GatewayConfig};
use daemon::mgmt::MgmtState;

/// 解析命令行参数（优先）与环境变量（兜底）。
struct Args {
    config_path: PathBuf,
    bind_addr: String,
}

fn parse_args() -> Result<Args, String> {
    let mut config_path: Option<PathBuf> = None;
    let mut bind_addr: Option<String> = None;
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(
                    raw.next().ok_or("--config 需要一个路径参数")?,
                ));
            }
            "--bind" => {
                bind_addr = Some(raw.next().ok_or("--bind 需要一个地址参数")?);
            }
            other => return Err(format!("未知参数 {other}（支持 --config <path> / --bind <addr>）")),
        }
    }
    Ok(Args {
        config_path: config_path
            .or_else(|| std::env::var("IOT_DAQ_CONFIG").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("config.toml")),
        bind_addr: bind_addr
            .or_else(|| std::env::var("IOT_DAQ_MGMT_BIND").ok())
            .unwrap_or_else(|| "127.0.0.1:8080".to_string()),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("[iot-daq-daemon] 参数错误: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // ① 预加载配置：启动失败（文件缺失 / TOML 非法）必须 fail-fast，不能带病运行。
    let config = match GatewayConfig::load(&args.config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[iot-daq-daemon] 配置加载失败 ({}): {e}", args.config_path.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "[iot-daq-daemon] 配置已加载 ({}): {} 点位 / {} 北向出口",
        args.config_path.display(),
        config.points.len(),
        config.outlets.len(),
    );

    // ② 共享状态 + 管理面：与 bootstrap 共用同一 DaemonShared（状态/热重载/事件）。
    let shared = DaemonShared::default();
    shared.set_config(Arc::new(ConfigShared::new(config.clone())));
    let mgmt_state = MgmtState::new(shared.clone(), Arc::new(config));

    let listener = match tokio::net::TcpListener::bind(&args.bind_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[iot-daq-daemon] 管理面绑定 {} 失败: {e}", args.bind_addr);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[iot-daq-daemon] 管理面已监听 http://{}", args.bind_addr);
    tokio::spawn(async move {
        // 运行期 serve 异常只记录不主动杀 daemon：北向采集不受管理面单点影响。
        if let Err(e) = axum::serve(listener, daemon::mgmt::router(mgmt_state)).await {
            eprintln!("[iot-daq-daemon] 管理面 serve 异常退出: {e}");
        }
    });

    // ③ bootstrap 全权接管：信号处理 / 热重载 / 调度 / 看门狗 / 优雅停机。
    match BootstrapBuilder::new(&args.config_path)
        .with_shared(shared)
        .run()
        .await
    {
        Ok(_shared) => {
            eprintln!("[iot-daq-daemon] 已优雅停机");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[iot-daq-daemon] bootstrap 运行失败: {e}");
            ExitCode::FAILURE
        }
    }
}
