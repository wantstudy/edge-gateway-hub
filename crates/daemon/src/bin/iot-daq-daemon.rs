//! `iot-daq-daemon` —— headless 守护进程入口（task 33）。
//!
//! 职责边界（薄壳，不做业务）：
//! 1. 解析启动参数（配置路径 / 管理面绑定地址 / 预检 / 前台标记，均可被环境变量覆盖）；
//! 2. `--preflight` 时执行容器启动前置校验（配置 / 宿主锚点 / 持久卷），见下；
//! 3. 预加载配置 → 装配 [`mgmt::MgmtState`] 并启动管理面 HTTP（REST + SSE + 静态）；
//! 4. 交棒 [`bootstrap::BootstrapBuilder::run`]（生命周期 / 热重载 / 调度 / 看门狗 /
//!    信号处理全在 bootstrap 内），停机后按其结果决定进程退出码。
//!
//! 环境变量：
//! - `IOT_DAQ_CONFIG`：配置文件路径（默认 `./config.toml`，`--config` 可覆盖）；
//! - `IOT_DAQ_MGMT_BIND`：管理面监听地址（默认 `127.0.0.1:8080`，`--bind` 可覆盖）；
//!   task-61 D-11：兼容部署资产注入的 `IOT_DAQ_HTTP_BIND`（+`IOT_DAQ_HTTP_PORT`，
//!   bind 不含端口时拼接）——解析顺序 `IOT_DAQ_MGMT_BIND` > `IOT_DAQ_HTTP_BIND` > 默认；
//! - `IOT_DAQ_WEB_DIST`：web-console 静态资源根目录（mgmt 模块读取，默认 `./web-dist`）；
//! - `IOT_DAQ_LOG_LEVEL` / `IOT_DAQ_LOG_JSON`：日志级别 / JSON 结构化开关
//!   （task-61 D-13 修复：bin 早期初始化全局 tracing subscriber）；
//! - `IOT_DAQ_DATA_DIR`：数据根目录（task-61 D-08：经 `GatewayConfig` 缺省解析
//!   流入运行期，设备密钥 / 审计库 / 试用标记随之落宿主持久卷）。
//!
//! 退出码：`0` = 优雅停机 / preflight 通过；`1` = 启动失败（配置加载 / 端口绑定 /
//! bootstrap 装配错误）；`2` = `--preflight` 前置校验失败（fail-fast，附可操作原因）。
//!
//! # `--preflight` / `--foreground` 语义（容器入口契约，task 60）
//!
//! 生产镜像为 distroless/static（**无 shell**），故入口不能用 shell 脚本，
//! Dockerfile 的 `ENTRYPOINT` 固定携带 `--preflight`、`CMD` 携带 `--foreground`，
//! 两者最终合成为 `iot-daq-daemon --preflight --config <cfg> --foreground`。语义：
//!
//! - `--preflight` 单独出现 → **仅校验并退出**（0 通过 / 2 失败），不进入服务。
//!   供 CI 与现场排障快速自检容器锚点 / 持久卷是否就位。
//! - `--preflight --foreground`（ENTRYPOINT + CMD 的实际组合）→ 先校验，
//!   **全部通过后才进入前台服务**；任一项失败即 `exit 2` 拒绝启动
//!   （与 `deploy/docker/entrypoint.sh` 的 fail-fast 语义一致）。
//! - `--foreground` 单独出现 → 语义 no-op（进程本就前台运行），仅用于让
//!   Docker 的 `CMD` 能安全地与 `ENTRYPOINT` 组合（不再报「未知参数」）。
//!
//! 该校验与 `deploy/docker/entrypoint.sh`（shell 版，供调试 / 非 distroless 变体）
//! 一一对应，红线依据见 `docs/design/container-machine-binding.md` 与
//! `docs/design/container-persistence-layout.md`。

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use tracing::{info, warn};

use daemon::bootstrap::{BootstrapBuilder, DaemonShared};
use daemon::config::{ConfigShared, GatewayConfig};
use daemon::mgmt::MgmtState;
use daemon::mgmt::ctrl_api::ControlApiState;
use daemon::platform::{self, AnchorMountStatus, DetectionConfidence, RuntimeForm};

// ---------------------------------------------------------------------------
// 1. 参数解析
// ---------------------------------------------------------------------------

/// 命令行原始解析结果（未套用环境变量默认值，便于确定性单测）。
#[derive(Debug, Default, PartialEq, Eq)]
struct RawArgs {
    /// `--config <path>`。
    config_path: Option<PathBuf>,
    /// `--bind <addr>`。
    bind_addr: Option<String>,
    /// `--preflight`：启动前置校验（容器入口防线）。
    preflight: bool,
    /// `--foreground`：显式前台标记（语义 no-op，供 Docker CMD 组合）。
    foreground: bool,
}

/// 套用环境变量兜底后的最终启动参数。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    config_path: PathBuf,
    bind_addr: String,
    preflight: bool,
    foreground: bool,
}

/// 解析命令行参数（**仅解析标志，不读环境变量**——环境兜底见 [`resolve_args`]）。
///
/// 支持的标志：`--config <path>`、`--bind <addr>`、`--preflight`、`--foreground`。
/// 未知标志返回可读错误；缺少值的标志同样报错。
///
/// # Errors
/// 未知参数 / 缺少必要值时返回 `Err(String)`（中文可读原因）。
fn parse_args<I>(raw: I) -> Result<RawArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut out = RawArgs::default();
    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                out.config_path = Some(PathBuf::from(
                    iter.next().ok_or("--config 需要一个路径参数")?,
                ));
            }
            "--bind" => {
                out.bind_addr = Some(iter.next().ok_or("--bind 需要一个地址参数")?);
            }
            "--preflight" => out.preflight = true,
            "--foreground" => out.foreground = true,
            other => {
                return Err(format!(
                    "未知参数 {other}（支持 --config <path> / --bind <addr> / --preflight / --foreground）"
                ))
            }
        }
    }
    Ok(out)
}

/// 套用环境变量兜底得到最终参数（命令行优先；空环境变量串按「未设置」处理）。
fn resolve_args_with(raw: RawArgs, config_env: Option<String>, bind_env: Option<String>) -> Args {
    Args {
        config_path: raw
            .config_path
            .or_else(|| config_env.filter(|s| !s.is_empty()).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("config.toml")),
        bind_addr: raw
            .bind_addr
            .or_else(|| bind_env.filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_MGMT_BIND.to_string()),
        preflight: raw.preflight,
        foreground: raw.foreground,
    }
}

/// [`resolve_args_with`] 的生产包装：从进程环境读取兜底值。
///
/// 管理面绑定解析顺序（task-61 D-11 修复）：`IOT_DAQ_MGMT_BIND` >
/// `IOT_DAQ_HTTP_BIND`（+`IOT_DAQ_HTTP_PORT`，bind 不含端口时拼接）> 内置默认。
fn resolve_args(raw: RawArgs) -> Args {
    let bind_env = mgmt_bind_from_env(
        std::env::var("IOT_DAQ_MGMT_BIND").ok(),
        std::env::var("IOT_DAQ_HTTP_BIND").ok(),
        std::env::var("IOT_DAQ_HTTP_PORT").ok(),
    );
    resolve_args_with(raw, std::env::var("IOT_DAQ_CONFIG").ok(), bind_env)
}

/// 管理面绑定地址的内置默认（宿主回环；容器形态经 env 覆盖为 `0.0.0.0:8080`）。
const DEFAULT_MGMT_BIND: &str = "127.0.0.1:8080";
/// `IOT_DAQ_HTTP_BIND` 不含端口且无 `IOT_DAQ_HTTP_PORT` 时的兜底端口。
const DEFAULT_MGMT_PORT: &str = "8080";

/// 管理面绑定地址的环境解析（task-61 D-11 修复，纯函数单测注入）：
/// 1. `IOT_DAQ_MGMT_BIND` 存在且非空白 → 直接使用；
/// 2. `IOT_DAQ_HTTP_BIND` 存在且非空白 → 含端口（含 `:`）直接使用；否则拼接
///    `IOT_DAQ_HTTP_PORT`（非空白时）或默认端口 [`DEFAULT_MGMT_PORT`]；
/// 3. 两者皆无 → `None`（由 [`resolve_args_with`] 回落内置默认）。
fn mgmt_bind_from_env(
    mgmt: Option<String>,
    http_bind: Option<String>,
    http_port: Option<String>,
) -> Option<String> {
    if let Some(bind) = mgmt.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return Some(bind);
    }
    let bind = http_bind
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    if bind.contains(':') {
        return Some(bind);
    }
    let port = http_port
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    Some(match port {
        Some(port) => format!("{bind}:{port}"),
        None => format!("{bind}:{DEFAULT_MGMT_PORT}"),
    })
}

// ---------------------------------------------------------------------------
// 2. preflight 前置校验（task 60：容器启动 fail-fast 防线）
// ---------------------------------------------------------------------------

/// preflight 失败退出码（与 `entrypoint.sh` 的通用失败码区分：daemon 侧统一 2）。
const PREFLIGHT_FAIL: u8 = 2;

/// 宿主锚点在容器内的只读挂载根（compose 用 `/host` 前缀，避免与镜像内
/// `/etc/machine-id` 混淆）。可由 `IOT_DAQ_HOST_ANCHOR_ROOT` 覆盖。
const DEFAULT_ANCHOR_ROOT: &str = "/host";
/// 默认持久卷根（宿主持久卷的容器内挂载点）。
const DEFAULT_DATA_DIR: &str = "/var/lib/iot-daq";
/// 默认宿主指纹降级文件（machine-binding §3）。
const DEFAULT_FINGERPRINT_FILE: &str = "/var/lib/iot-daq/host-fingerprint.json";

/// 宿主锚点挂载根：优先 `IOT_DAQ_HOST_ANCHOR_ROOT`，否则由
/// `IOT_DAQ_HOST_MACHINE_ID_PATH`（如 `/host/etc/machine-id`）反推，最后回落 `/host`。
fn anchor_root() -> PathBuf {
    if let Ok(v) = std::env::var("IOT_DAQ_HOST_ANCHOR_ROOT") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(p) = std::env::var("IOT_DAQ_HOST_MACHINE_ID_PATH") {
        let path = PathBuf::from(p);
        // 去掉尾部 `<...>/etc/machine-id` → 剩下锚点根。
        if let Some(root) = path.parent().and_then(Path::parent) {
            if !root.as_os_str().is_empty() {
                return root.to_path_buf();
            }
        }
    }
    PathBuf::from(DEFAULT_ANCHOR_ROOT)
}

/// 持久卷根（`IOT_DAQ_DATA_DIR` 覆盖，默认 `/var/lib/iot-daq`）。
fn data_dir() -> PathBuf {
    std::env::var("IOT_DAQ_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
}

/// 宿主指纹降级文件路径（`IOT_DAQ_HOST_FINGERPRINT_FILE` 覆盖）。
fn fingerprint_file() -> PathBuf {
    std::env::var("IOT_DAQ_HOST_FINGERPRINT_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FINGERPRINT_FILE))
}

/// 读取文本文件并去除首尾空白；读取失败返回空串（调用方按「缺失」处理）。
fn read_trim(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 文件存在、是普通文件且非空。
fn is_readable_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// 在目录内做一次**真实写探针**（写 + 删除）——比权限位可靠：只读挂载下
/// 权限位再宽也写不了。返回 `Err` 即视为不可写。
fn write_probe(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".preflight-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"preflight")?;
    // 删除失败不视为探针失败（写已成功；残留文件无害）。
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 检测可信度的中文标签。
fn confidence_str(c: DetectionConfidence) -> &'static str {
    match c {
        DetectionConfidence::High => "high",
        DetectionConfidence::Low => "low（证据不足）",
    }
}

/// 执行全部前置校验并打印逐项结果（成 / 败 + 原因）。
///
/// 返回 `true` 表示全部通过；`false` 表示存在失败项（原因已打印，
/// 调用方据此 `exit 2`）。该校验为**纯读 + 幂等目录创建 + 可写探针**，零外部副作用。
fn run_preflight(config_path: &Path) -> bool {
    println!("[iot-daq-daemon] preflight 启动前置校验开始 ...");
    let mut failures: Vec<String> = Vec::new();

    // ---- 1/4 配置解析（与主流程同一加载路径，确保失败即 fail-fast）----
    println!(
        "[iot-daq-daemon] 1/4 配置解析 ({}) ...",
        config_path.display()
    );
    match GatewayConfig::load(config_path) {
        Ok(cfg) => println!(
            "[iot-daq-daemon]     成 —— {} 点位 / {} 北向出口",
            cfg.points.len(),
            cfg.outlets.len()
        ),
        Err(e) => {
            let reason = format!(
                "配置加载失败 ({}): {e}。请确认文件存在且为合法 TOML。",
                config_path.display()
            );
            println!("[iot-daq-daemon]     败 —— {reason}");
            failures.push(reason);
        }
    }

    // ---- 2/4 运行形态检测（容器 / 原生分支）----
    let det = platform::detect();
    println!("[iot-daq-daemon] 2/4 运行形态 ...");
    println!(
        "[iot-daq-daemon]     形态={}（可信度 {}）",
        det.form.as_str(),
        confidence_str(det.confidence)
    );

    // ---- 3/4 宿主锚点只读挂载（陷阱 1：机器码只来源宿主机）----
    println!("[iot-daq-daemon] 3/4 宿主锚点 ...");
    if det.form.is_container() {
        for reason in check_host_anchors() {
            failures.push(reason);
        }
    } else {
        println!("[iot-daq-daemon]     非容器形态 —— 锚点取自本机，跳过只读挂载校验");
    }

    // ---- 4/4 持久卷可写性（陷阱 2：授权/试用/队列/配置/日志落宿主卷）----
    println!("[iot-daq-daemon] 4/4 持久卷 ...");
    for reason in check_persistent_volumes() {
        failures.push(reason);
    }

    // ---- 汇总 ----
    if failures.is_empty() {
        println!("[iot-daq-daemon] preflight 全部检查通过");
        true
    } else {
        println!(
            "[iot-daq-daemon] preflight 检查未通过（{} 项失败）：",
            failures.len()
        );
        for (i, f) in failures.iter().enumerate() {
            println!("[iot-daq-daemon]   失败 {}: {f}", i + 1);
        }
        println!(
            "[iot-daq-daemon] 拒绝启动；请按上方提示修复。切勿通过删除指纹文件 / 改挂载点绕过（会被运行时自检判为环境变更）。"
        );
        false
    }
}

/// 校验容器形态下的宿主锚点（machine-id / DMI / 宿主 MAC 或签名指纹文件）。
///
/// 复用 [`platform::check_host_anchor_mounts`] 的逐项结论，并叠加一个**强信号**
/// 判定：宿主锚点与容器自身 `/etc/machine-id` 相同 → 挂载落回了容器内路径
/// （容器内标识每次重建即变，红线禁止，见 container-machine-binding §2）。
///
/// 返回失败原因列表（空 = 通过）。
fn check_host_anchors() -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let root = anchor_root();
    let report = platform::check_host_anchor_mounts(RuntimeForm::LinuxDocker, &root);

    // 宿主 machine-id（只读挂载进容器）。
    let host_machine_id_path = root.join("etc/machine-id");
    match report.machine_id {
        AnchorMountStatus::Ok => println!(
            "[iot-daq-daemon]     宿主 machine-id ({}): 成（只读）",
            host_machine_id_path.display()
        ),
        AnchorMountStatus::Missing => println!(
            "[iot-daq-daemon]     宿主 machine-id ({}): 缺失",
            host_machine_id_path.display()
        ),
        AnchorMountStatus::ReadOnlyViolation => println!(
            "[iot-daq-daemon]     宿主 machine-id ({}): 可写（疑似容器内生成；见 platform 模块已知局限）",
            host_machine_id_path.display()
        ),
    }

    // 强信号：宿主锚点 == 容器自身 machine-id → 挂载落点错误（red-line）。
    let host_mid = read_trim(&host_machine_id_path);
    let container_mid = read_trim(Path::new("/etc/machine-id"));
    if !host_mid.is_empty() && !container_mid.is_empty() && host_mid == container_mid {
        let head: String = host_mid.chars().take(8).collect();
        failures.push(format!(
            "锚点 machine-id 与容器自身相同（前 8 位 {head}…）：{} 实际挂的是容器内 /etc/machine-id。\
             容器内标识每次 docker rm && docker run 都会变 → 授权失效 + SQLCipher 解不开。\
             请改用随包 compose 的 /host 前缀只读挂载。",
            host_machine_id_path.display()
        ));
    }

    // DMI 锚点（D-17：逐条目统计可读数而非二元「缺失」——标准 Linux 上
    // product_uuid / product_serial 为 0400 root-only，非 root 容器读不到是
    // **预期常态**；board_serial 等个别条目常为 0444，读得到即计入）。
    let dmi_dir = root.join("sys/class/dmi/id");
    let dmi_readable = platform::count_readable_dmi_entries(&dmi_dir);
    let dmi_ok = report.dmi == AnchorMountStatus::Ok;
    let dmi_note = if dmi_readable == 0 {
        "（0 条目可读：目录未挂载或 0400 条目对非 root 不可读，属预期；由 machine-id / 宿主 MAC / 指纹文件凑 quorum）"
    } else {
        ""
    };
    println!(
        "[iot-daq-daemon]     宿主 DMI ({}): {}/{} 条目可读{}",
        dmi_dir.display(),
        dmi_readable,
        platform::DMI_ANCHOR_ENTRIES.len(),
        dmi_note
    );

    // 宿主 MAC：三选一（只读文件挂载 / 环境变量注入 / 签名指纹文件降级）。
    let mac_env = std::env::var("IOT_DAQ_HOST_MAC")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let fingerprint = fingerprint_file();
    let fingerprint_present = is_readable_nonempty(&fingerprint);
    let mac_ok = report.mac == AnchorMountStatus::Ok || mac_env.is_some() || fingerprint_present;
    println!(
        "[iot-daq-daemon]     宿主 MAC: {}（挂载文件={}, env={}, 指纹文件={}）",
        if mac_ok { "成" } else { "缺失" },
        report.mac == AnchorMountStatus::Ok,
        mac_env.is_some(),
        fingerprint_present
    );

    // 至少一个可用锚点，否则无法派生机器码。
    let machine_id_available = matches!(
        report.machine_id,
        AnchorMountStatus::Ok | AnchorMountStatus::ReadOnlyViolation
    ) && !host_mid.is_empty();
    if !machine_id_available && !dmi_ok && !mac_ok {
        failures.push(
            "宿主锚点全部不可用：machine-id 未挂载、DMI 无可用项、也无宿主 MAC / 签名指纹文件。\
             请按 container-machine-binding.md §3 在**宿主**执行指纹采集后重试；严禁在容器内自行采集。"
                .to_string(),
        );
    }

    failures
}

/// 校验持久卷（陷阱 2）：根目录存在且可写，`license/ trial/ config/ logs/`
/// 子目录就位且可写（缺失则幂等创建）。
///
/// `queue.db` 为首次写入时创建的文件，不要求预先生成——由根目录可写性覆盖。
///
/// 返回失败原因列表（空 = 通过）。
fn check_persistent_volumes() -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let data = data_dir();
    println!("[iot-daq-daemon]     持久卷根: {}", data.display());

    if !data.is_dir() {
        failures.push(format!(
            "持久卷根 {} 不存在或不是目录。请先执行 install.sh（创建并挂载宿主持久卷，陷阱 2）。",
            data.display()
        ));
        // 根不存在时子目录检查无意义，直接返回。
        return failures;
    }

    match write_probe(&data) {
        Ok(()) => println!("[iot-daq-daemon]     根目录可写探针: 成"),
        Err(e) => failures.push(format!(
            "持久卷 {} 不可写（{e}）。若状态落到容器可写层，docker rm && docker run 即成为重置试用的后门；\
             请检查卷是否 :ro / 属主是否为容器运行用户（uid 65532）。",
            data.display()
        )),
    }

    for sub in ["license", "trial", "config", "logs"] {
        let dir = data.join(sub);
        if !dir.is_dir() {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => println!("[iot-daq-daemon]     子目录 {}: 已创建", dir.display()),
                Err(e) => {
                    failures.push(format!(
                        "授权相关目录 {} 缺失且无法创建（{e}）。请在宿主 mkdir 并 chown 给容器运行用户后重启。",
                        dir.display()
                    ));
                    continue;
                }
            }
        }
        match write_probe(&dir) {
            Ok(()) => println!("[iot-daq-daemon]     子目录 {}: 可写", dir.display()),
            Err(e) => failures.push(format!(
                "授权相关目录 {} 不可写（{e}）。该四类（授权/试用/配置/日志）必须位于宿主持久卷 {} 之下。",
                dir.display(),
                data.display()
            )),
        }
    }

    println!(
        "[iot-daq-daemon]     队列库 {}: 依赖卷可写（首次写入时创建）",
        data.join("queue.db").display()
    );

    failures
}

// ---------------------------------------------------------------------------
// 3. 进程入口
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    // ⓪ 日志初始化（task-61 D-13 修复）：在任何组件初始化之前安装全局 tracing
    //    subscriber——修复前 init_logging 无生产调用方，所有框架日志（含调度器 /
    //    授权装配的关键告警）在生产形态不可见，IOT_DAQ_LOG_LEVEL/LOG_JSON 无效。
    //    初始化失败不阻断启动（降级为 eprintln 输出），绝不 panic。
    let _log_guard: Option<daemon::logging::WorkerGuard> =
        match daemon::logging::init(&daemon::logging::LoggingConfig::from_env()) {
            Ok(guard) => guard,
            Err(err) => {
                eprintln!("[iot-daq-daemon] 日志初始化失败（继续运行，仅 stdout 输出）: {err}");
                None
            }
        };

    let raw = match parse_args(std::env::args().skip(1)) {
        Ok(raw) => raw,
        Err(msg) => {
            eprintln!("[iot-daq-daemon] 参数错误: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let args = resolve_args(raw);

    if args.foreground {
        eprintln!("[iot-daq-daemon] --foreground 已声明（进程本就前台运行，语义 no-op）");
    }

    // ① preflight（容器入口防线）：失败即 fail-fast 退出 2。
    if args.preflight {
        let passed = run_preflight(&args.config_path);
        if !passed {
            return ExitCode::from(PREFLIGHT_FAIL);
        }
        if !args.foreground {
            // 仅校验模式：不开服务，退出 0（供 CI / 现场自检）。
            eprintln!("[iot-daq-daemon] preflight 通过（仅校验模式，未进入服务），退出 0");
            return ExitCode::SUCCESS;
        }
        // ENTRYPOINT + CMD 组合：校验通过后进入前台服务（与 Dockerfile 注释一致）。
        eprintln!("[iot-daq-daemon] preflight 通过 —— 进入前台服务");
    }

    // ② 预加载配置：启动失败（文件缺失 / TOML 非法）必须 fail-fast，不能带病运行。
    let config = match GatewayConfig::load(&args.config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!(
                "[iot-daq-daemon] 配置加载失败 ({}): {e}",
                args.config_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "[iot-daq-daemon] 配置已加载 ({}): {} 点位 / {} 北向出口",
        args.config_path.display(),
        config.points.len(),
        config.outlets.len(),
    );

    // ②-b 数据根 / 南向采集装配（task-61 D-08 / D-12 修复；config 被 MgmtState
    //     取走之前完成捕获）：
    //     - data_dir：设备密钥（data_dir/license/device-ed25519.key）、审计库
    //       （data_dir/audit.db）、试用标记统一落持久卷（缺省随 env
    //       IOT_DAQ_DATA_DIR 解析，见 config::default_data_dir）；
    //     - poll handler：配置声明了点位 → 构造真实南向轮询动作注入 bootstrap。
    //       修复前 with_poll_handler 全仓无生产调用方，采集调度器在生产形态
    //       永不启动（容器实测：点位注册成功但 mock 从站 0 连接）。
    let data_dir = config.gateway.data_dir.clone();
    let poll_handler: Option<Arc<dyn daemon::scheduler::PollHandler>> = if config.points.is_empty()
    {
        None
    } else {
        Some(Arc::new(
            daemon::southbound::DevicePollHandler::from_config(&config),
        ))
    };

    // ②-c 授权生产装配（task 22/23 上岗；在 config 被 MgmtState 取走之前完成）。
    //     读 [gateway.licensing]——
    //     - 未配置（无 cloud_url 且无 activation_code）→ 行为与既往完全一致；
    //     - 已配置 → 构造 MachineIdentity + 设备签名密钥 + LicensingClient +
    //       LicenseRuntimeConfig，走 with_license_runtime 上岗；
    //     - 装配失败 → fail-closed：daemon 照常启动（本地采集不受影响），北向闸门
    //       保持关闭，可解释原因（含恢复路径）落入共享态。
    let licensing_outcome = daemon::auth::assembly::assemble_production(
        &config.gateway.licensing,
        &config.gateway.data_dir,
    );

    // ②-d 北向运行期与离线队列生产装配（queue.db 生产落盘缺口补齐）：
    //     - 声明了 [[outlets]] → 构造 OfflineQueue（<data_dir>/queue.db，D-08 同卷）
    //       与 NorthRuntimeConfig 注入 bootstrap（驱动循环 / 授权闸门 / 补发 /
    //       审计上报均由 bootstrap 既有装配路径统一完成）；
    //     - 未声明 → 保持既有行为（北向不启动、不创建 queue.db）。
    //     离线队列打开失败 fail-fast：降级落盘路径失去落盘保证，不能带病运行。
    let north_assembly = match daemon::bootstrap::assemble_north_runtime(&config) {
        Ok(assembly) => assembly,
        Err(e) => {
            eprintln!(
                "[iot-daq-daemon] [ERROR] 离线队列初始化失败（{}），拒绝启动（fail-closed）：{e}",
                data_dir
                    .join(daemon::offline_queue::QUEUE_DB_FILE_NAME)
                    .display()
            );
            return ExitCode::FAILURE;
        }
    };

    // ③ 共享状态 + 管理面：与 bootstrap 共用同一 DaemonShared（状态/热重载/事件）。
    let shared = DaemonShared::default();
    shared.set_config(Arc::new(ConfigShared::new(config.clone())));

    // ③-a 控制面装配（BE-CTRL / task 140）：打开控制账本（data_dir/control.db），
    // 构造 ControlRegistry 并挂载到 shared，再经 with_control_api 注入 MgmtState。
    let control_db_path = data_dir.join("control.db");
    let control_ledger = match daemon::ctrl::ControlLedger::open(&control_db_path) {
        Ok(ledger) => {
            info!(
                path = %control_db_path.display(),
                "bootstrap: control ledger opened"
            );
            Some(Arc::new(ledger))
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %control_db_path.display(),
                "bootstrap: [WARN] control ledger unavailable; control endpoints will return 503"
            );
            None
        }
    };
    let control_api = ControlApiState::new(shared.clone(), control_ledger);
    // 将 control_api 内部的 registry 克隆一份挂到 shared，供路由 handler 访问。
    if let Some(registered) = shared.control_registry() {
        drop(registered); // 先验证已挂载
    }

    // 写接口落盘路径绑定：与 IOT_DAQ_CONFIG / --config 指向同一文件（热重载同源）。
    let mgmt_state =
        MgmtState::new(shared.clone(), Arc::new(config))
            .with_config_path(args.config_path.clone())
            .with_control_api(control_api);

    let listener = match tokio::net::TcpListener::bind(&args.bind_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[iot-daq-daemon] 管理面绑定 {} 失败: {e}", args.bind_addr);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[iot-daq-daemon] 管理面已监听 http://{}", args.bind_addr);
    tokio::spawn(async move {
        // 周期备份循环（B-2；`[settings.backup_policy].interval_min == 0` 时空转）。
        // 30s tick 读热快照：PUT 备份策略后下一拍按新间隔生效。测试服务不挂本循环。
        daemon::mgmt::ops_api::spawn_periodic_backup(&mgmt_state);
        // 运行期 serve 异常只记录不主动杀 daemon：北向采集不受管理面单点影响。
        // ⚠️ 必须挂 ConnectInfo：`/api/auth/bootstrap` 用它在**任何业务分支之前**
        //    判定来源是否为回环（`is_local_peer`）。旧写法 `axum::serve(.., router)`
        //    拿不到 ConnectInfo → `None` → 恒 403 → 首次初始化入口等于不存在。
        //    本行是 task 27 的上线前提，请勿回退。
        let svc = daemon::mgmt::router(mgmt_state)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        if let Err(e) = axum::serve(listener, svc).await {
            eprintln!("[iot-daq-daemon] 管理面 serve 异常退出: {e}");
        }
    });

    // ④ bootstrap 全权接管：信号处理 / 热重载 / 调度 / 看门狗 / 优雅停机。
    //    data_dir（D-08）与 poll handler（D-12）在此注入生产装配链。
    let mut builder = BootstrapBuilder::new(&args.config_path)
        .with_shared(shared)
        .with_data_dir(data_dir);
    if let Some(handler) = poll_handler {
        builder = builder.with_poll_handler(handler);
    }
    // 北向运行期接线 + 停机离线队列 flush 钩子（见 ②-d）：未声明 [[outlets]] 时
    // 保持既有行为——north runtime 不注入，bootstrap 记 info 说明。
    if let Some(assembly) = north_assembly {
        builder = builder
            .with_north_runtime(assembly.runtime_config)
            .with_offline_flusher(daemon::bootstrap::offline_flush_hook(assembly.queue));
    }
    match licensing_outcome {
        daemon::auth::assembly::AssemblyOutcome::Assembled(rt_cfg) => {
            eprintln!(
                "[iot-daq-daemon] 授权运行期装配完成（cloud={}，心跳 {}s）",
                rt_cfg.cloud_url.as_deref().unwrap_or(""),
                rt_cfg.heartbeat_interval.as_secs(),
            );
            builder = builder.with_license_runtime(rt_cfg);
        }
        daemon::auth::assembly::AssemblyOutcome::NotConfigured => {
            eprintln!("[iot-daq-daemon] 未配置云授权（[gateway.licensing]），授权编排跳过");
        }
        daemon::auth::assembly::AssemblyOutcome::Failed(reason) => {
            // reason 只含环境变量名 / 路径 / 数量——激活码与密钥材料绝不进日志。
            eprintln!("[iot-daq-daemon] [ERROR] 授权装配失败（fail-closed）：{reason}");
            builder = builder.with_license_assembly_failed(reason);
        }
    }
    match builder.run().await {
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

// ---------------------------------------------------------------------------
// 测试（task 60：参数解析回归；preflight 为纯运行时逻辑，不在此覆盖）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷入口：把 `&[&str]` 转成 `parse_args` 需要的 `String` 迭代器。
    fn parse(raw: &[&str]) -> Result<RawArgs, String> {
        parse_args(raw.iter().map(|s| s.to_string()))
    }

    /// T01 `--preflight` 单独出现：置位 preflight，其余保持缺省。
    #[test]
    fn parse_preflight_alone() {
        let a = parse(&["--preflight"]).expect("应解析成功");
        assert!(a.preflight);
        assert!(!a.foreground);
        assert_eq!(a.config_path, None);
        assert_eq!(a.bind_addr, None);
    }

    /// T02 `--foreground` 单独出现：置位 foreground，语义 no-op。
    #[test]
    fn parse_foreground_alone() {
        let a = parse(&["--foreground"]).expect("应解析成功");
        assert!(a.foreground);
        assert!(!a.preflight);
    }

    /// T03 `--preflight --config X`：预检 + 指定配置（容器 ENTRYPOINT 形态）。
    #[test]
    fn parse_preflight_with_config() {
        let a =
            parse(&["--preflight", "--config", "/etc/iot-daq/gateway.toml"]).expect("应解析成功");
        assert!(a.preflight);
        assert_eq!(
            a.config_path,
            Some(PathBuf::from("/etc/iot-daq/gateway.toml"))
        );
    }

    /// T04 `--config X --bind Y`：传统原生启动参数不被破坏。
    #[test]
    fn parse_config_and_bind() {
        let a = parse(&["--config", "gateway.toml", "--bind", "0.0.0.0:8080"]).expect("应解析成功");
        assert_eq!(a.config_path, Some(PathBuf::from("gateway.toml")));
        assert_eq!(a.bind_addr.as_deref(), Some("0.0.0.0:8080"));
        assert!(!a.preflight && !a.foreground);
    }

    /// T05 未知标志必须被拒绝，且错误信息列出支持的新标志。
    #[test]
    fn parse_rejects_unknown_flag() {
        let err = parse(&["--bogus"]).expect_err("未知参数必须报错");
        assert!(err.contains("--bogus"), "错误应包含未知参数名: {err}");
        assert!(err.contains("--preflight"), "错误应列出新标志: {err}");
        assert!(err.contains("--foreground"), "错误应列出新标志: {err}");
    }

    /// T06 缺少值的标志必须被拒绝。
    #[test]
    fn parse_rejects_missing_value() {
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--bind"]).is_err());
    }

    /// T07 组合：ENTRYPOINT + CMD 的实际合成形态。
    #[test]
    fn parse_all_flags_together() {
        let a = parse(&[
            "--preflight",
            "--config",
            "/etc/iot-daq/gateway.toml",
            "--foreground",
        ])
        .expect("应解析成功");
        assert!(a.preflight && a.foreground);
        assert_eq!(
            a.config_path,
            Some(PathBuf::from("/etc/iot-daq/gateway.toml"))
        );
    }

    /// T08 环境兜底：无命令行时用环境值；空串按未设置处理回落默认。
    #[test]
    fn resolve_args_applies_defaults_and_env() {
        // 无命令行 + 无环境 → 内置默认。
        let d = resolve_args_with(RawArgs::default(), None, None);
        assert_eq!(d.config_path, PathBuf::from("config.toml"));
        assert_eq!(d.bind_addr, "127.0.0.1:8080");

        // 无命令行 + 环境值 → 采用环境值。
        let e = resolve_args_with(
            RawArgs::default(),
            Some("/srv/iot-daq/gateway.toml".to_string()),
            Some("127.0.0.1:9000".to_string()),
        );
        assert_eq!(e.config_path, PathBuf::from("/srv/iot-daq/gateway.toml"));
        assert_eq!(e.bind_addr, "127.0.0.1:9000");

        // 命令行优先于环境；空环境串按未设置处理。
        let f = resolve_args_with(
            RawArgs {
                config_path: Some(PathBuf::from("cli.toml")),
                bind_addr: None,
                preflight: true,
                foreground: true,
            },
            Some("env.toml".to_string()),
            Some(String::new()),
        );
        assert_eq!(f.config_path, PathBuf::from("cli.toml"));
        assert_eq!(f.bind_addr, "127.0.0.1:8080");
        assert!(f.preflight && f.foreground);
    }

    /// task-61 D-11：管理面绑定环境解析——`IOT_DAQ_MGMT_BIND` 优先 >
    /// `IOT_DAQ_HTTP_BIND`(+`IOT_DAQ_HTTP_PORT`) > None（回落内置默认）。
    #[test]
    fn mgmt_bind_from_env_resolution_order() {
        // 两者皆无 → None（resolve_args_with 回落 127.0.0.1:8080）。
        assert_eq!(mgmt_bind_from_env(None, None, None), None);

        // MGMT_BIND 优先（HTTP_* 同时在场也被忽略）。
        assert_eq!(
            mgmt_bind_from_env(
                Some("0.0.0.0:9090".to_string()),
                Some("0.0.0.0:7070".to_string()),
                Some("1234".to_string()),
            ),
            Some("0.0.0.0:9090".to_string())
        );

        // MGMT_BIND 空白 = 未配置 → 回落 HTTP_BIND。
        assert_eq!(
            mgmt_bind_from_env(
                Some("   ".to_string()),
                Some("0.0.0.0:7070".to_string()),
                None,
            ),
            Some("0.0.0.0:7070".to_string())
        );

        // HTTP_BIND 含端口 → 原样使用（HTTP_PORT 忽略）。
        assert_eq!(
            mgmt_bind_from_env(
                None,
                Some("0.0.0.0:7070".to_string()),
                Some("1234".to_string())
            ),
            Some("0.0.0.0:7070".to_string())
        );

        // HTTP_BIND 不含端口 + HTTP_PORT → 拼接。
        assert_eq!(
            mgmt_bind_from_env(None, Some("0.0.0.0".to_string()), Some("7070".to_string())),
            Some("0.0.0.0:7070".to_string())
        );

        // HTTP_BIND 不含端口、无 HTTP_PORT → 默认端口兜底。
        assert_eq!(
            mgmt_bind_from_env(None, Some("0.0.0.0".to_string()), None),
            Some("0.0.0.0:8080".to_string())
        );

        // 空白 HTTP_BIND / HTTP_PORT = 未配置。
        assert_eq!(
            mgmt_bind_from_env(None, Some("  ".to_string()), Some("  ".to_string())),
            None
        );
        // 前后空白 trim。
        assert_eq!(
            mgmt_bind_from_env(Some(" 0.0.0.0:9090 \n".to_string()), None, None),
            Some("0.0.0.0:9090".to_string())
        );
    }
}
