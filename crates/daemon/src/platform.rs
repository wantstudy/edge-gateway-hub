//! `platform` — 平台差异检测原语（task 57 切片 1）。
//!
//! # 职责边界
//! 网关双形态交付（Linux Docker 主推 + 原生 systemd 可选；Windows Tauri 桌面 /
//! 服务）。本模块提供**纯读、零副作用、永不 panic** 的运行形态检测与平台路径
//! 约定，供 bootstrap / 授权（机器码锚点）/ 存储层按形态分支：
//!
//! - [`RuntimeForm`] + [`detect`]：四形态判定，证据不足时回落保守值并给出
//!   [`DetectionConfidence`]；
//! - [`PlatformPaths`] + [`paths`]：各形态的数据 / 配置目录与授权锚点路径约定；
//!   目录创建独立为 [`ensure_dirs`]（声明与副作用分离）；
//! - [`check_host_anchor_mounts`]：容器形态下校验宿主锚点只读挂载
//!   （/etc/machine-id、DMI、宿主 MAC 注入文件），原生形态恒 [`AnchorMountStatus::Ok`]；
//! - [`systemd_unit_spec`] / [`udev_rules_spec`]：Linux 原生部署的纯文本模板。
//!
//! # 容器授权锚点红线（与 `auth::machine_id` 联动）
//! 容器内**禁止**采容器自身的 machine-id / MAC / 主机名。compose 必须满足：
//! - 只读挂载宿主 `/etc/machine-id` 与 `/sys/class/dmi/id/*` 进容器；
//! - 试用标记 / 租约 / 授权状态只落宿主持久卷（挂载到 [`PlatformPaths::data_dir`]），
//!   绝不写容器可写层（容器销毁即丢）。
//!
//! # 实现红线
//! - 零新依赖：仅 std + cfg + 文件系统探测；
//! - 非测试代码零 panic / unwrap / expect；探测失败一律回落保守值；
//! - 不做写探测（只读语义）：可写性经 unix 权限位判定，局限见
//!   [`check_host_anchor_mounts`] 的误判风险注释。

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{DaemonError, DaemonResult};

// ---------------------------------------------------------------------------
// 1. 运行形态检测
// ---------------------------------------------------------------------------

/// 服务模式标记环境变量：由 Windows 服务包装器（如 `sc` 包装 / winsw）在启动
/// 服务进程时设为 `IOT_DAQ_SERVICE_MODE=1`；桌面（Tauri）启动不设置。
pub const SERVICE_MODE_ENV: &str = "IOT_DAQ_SERVICE_MODE";

/// 容器标记文件（Docker 约定）。
const DOCKERENV_MARKER: &str = "/.dockerenv";
/// 容器标记文件（podman / 通用 OCI 约定）。
const CONTAINERENV_MARKER: &str = "/run/.containerenv";

/// 网关运行形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeForm {
    /// Windows 服务（由服务包装器启动，`IOT_DAQ_SERVICE_MODE=1`）。
    WindowsService,
    /// Windows 桌面进程（Tauri 宿主内嵌）。
    WindowsDesktop,
    /// Linux 原生（systemd 托管）。
    LinuxSystemd,
    /// Linux 容器（Docker / OCI 运行时）。
    LinuxDocker,
}

impl RuntimeForm {
    /// 形态字面量（日志 / 诊断用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeForm::WindowsService => "windows-service",
            RuntimeForm::WindowsDesktop => "windows-desktop",
            RuntimeForm::LinuxSystemd => "linux-systemd",
            RuntimeForm::LinuxDocker => "linux-docker",
        }
    }

    /// 是否运行在 Linux 容器内（宿主锚点挂载校验仅对该形态有意义）。
    pub fn is_container(&self) -> bool {
        matches!(self, RuntimeForm::LinuxDocker)
    }
}

/// 检测结论的可信度。`Low` 表示探测证据不完整（IO 失败 / 未知平台），
/// 调用方应记录日志并在敏感路径上做保守处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionConfidence {
    /// 证据充分。
    High,
    /// 证据不足，形态为保守回落值。
    Low,
}

/// 形态检测结论：形态 + 可信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// 判定出的运行形态。
    pub form: RuntimeForm,
    /// 可信度（回落场景为 `Low`）。
    pub confidence: DetectionConfidence,
}

/// 检测当前进程的运行形态（纯读、零副作用、永不 panic）。
///
/// - Windows：`IOT_DAQ_SERVICE_MODE=1` → 服务；否则桌面（env 读取失败按桌面，
///   缺失本身即确定证据）。
/// - Linux：`/.dockerenv` 或 `/run/.containerenv` 存在 → Docker；两者均确认
///   不存在 → systemd（High）；探测失败（非 NotFound 的 IO 错误）→ 保守回落
///   systemd + `Low`。
/// - 其它平台：保守回落 LinuxSystemd + `Low`（当前产品矩阵不存在该形态，
///   仅为编译兜底）。
pub fn detect() -> Detection {
    detect_with_root(Path::new("/"))
}

/// [`detect`] 的根路径参数化版本（测试注入用；生产恒传 `/`）。
pub fn detect_with_root(root: &Path) -> Detection {
    #[cfg(windows)]
    {
        let _ = root;
        if service_mode_env_set() {
            Detection {
                form: RuntimeForm::WindowsService,
                confidence: DetectionConfidence::High,
            }
        } else {
            Detection {
                form: RuntimeForm::WindowsDesktop,
                confidence: DetectionConfidence::High,
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        docker_verdict(
            probe_exists(&root.join(DOCKERENV_MARKER)),
            probe_exists(&root.join(CONTAINERENV_MARKER)),
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = root;
        Detection {
            form: RuntimeForm::LinuxSystemd,
            confidence: DetectionConfidence::Low,
        }
    }
}

/// 探测路径是否存在。`None` = 探测失败（非 NotFound 的 IO 错误，证据不可得）。
/// （仅 Linux 编译路径使用；`test` 使参数化测试在任意宿主可用。）
#[cfg(any(target_os = "linux", test))]
fn probe_exists(path: &Path) -> Option<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// 由容器标记探测结果推导 Linux 形态（纯函数，参数化测试入口）。
///
/// 任一标记确认存在 → Docker（High）；两者均确认不存在 → systemd（High）；
/// 存在探测失败 → 保守回落 systemd + Low。
/// （仅 Linux 编译路径使用；`test` 使参数化测试在任意宿主可用。）
#[cfg(any(target_os = "linux", test))]
fn docker_verdict(dockerenv: Option<bool>, containerenv: Option<bool>) -> Detection {
    if dockerenv == Some(true) || containerenv == Some(true) {
        Detection {
            form: RuntimeForm::LinuxDocker,
            confidence: DetectionConfidence::High,
        }
    } else if dockerenv == Some(false) && containerenv == Some(false) {
        Detection {
            form: RuntimeForm::LinuxSystemd,
            confidence: DetectionConfidence::High,
        }
    } else {
        Detection {
            form: RuntimeForm::LinuxSystemd,
            confidence: DetectionConfidence::Low,
        }
    }
}

/// `IOT_DAQ_SERVICE_MODE` 是否为 `1`（纯函数；缺失 / 其它值均视为非服务模式）。
fn service_mode_env_set() -> bool {
    service_mode_from_env_value(std::env::var(SERVICE_MODE_ENV).ok().as_deref())
}

/// [`service_mode_env_set`] 的纯判定内核（测试参数化入口）。
fn service_mode_from_env_value(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

// ---------------------------------------------------------------------------
// 2. 平台路径约定
// ---------------------------------------------------------------------------

/// 各形态的平台路径约定（声明式，不做任何 IO）。
///
/// 目录创建统一走 [`ensure_dirs`]；授权锚点路径在 Windows 上为 `None`
/// （Windows 机器码走注册表 / WMI，见 `auth::machine_id`，与文件系统锚点无关）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformPaths {
    /// 数据目录：SQLite / 离线队列 / 试用标记冗余槽 / 租约状态。
    /// **容器形态 = 宿主持久卷的容器内挂载点**（见 [`paths`] 文档）。
    pub data_dir: PathBuf,
    /// 配置目录：gateway.toml 等用户可编辑资产。
    pub config_dir: PathBuf,
    /// 宿主 machine-id 的锚点路径（容器形态 = 只读挂载进来的宿主文件）。
    pub anchor_machine_id: Option<PathBuf>,
    /// DMI 锚点目录（`product_uuid` / `product_serial` 所在目录）。
    pub anchor_dmi: Option<PathBuf>,
    /// 宿主 MAC 注入文件（安装脚本 / compose 写入或只读挂载）。
    pub anchor_host_mac: Option<PathBuf>,
}

/// Linux 数据目录（原生与容器内一致的容器内路径）。
const LINUX_DATA_DIR: &str = "/var/lib/iot-daq";
/// Linux 配置目录（原生与容器内一致的容器内路径）。
const LINUX_CONFIG_DIR: &str = "/etc/iot-daq";
/// 宿主 machine-id 锚点。
const ANCHOR_MACHINE_ID: &str = "/etc/machine-id";
/// DMI 锚点目录。
const ANCHOR_DMI: &str = "/sys/class/dmi/id";
/// 宿主 MAC 注入文件（约定路径；安装脚本写入，compose 只读挂载）。
const ANCHOR_HOST_MAC: &str = "/run/iot-daq/host-mac";

/// 返回指定形态的平台路径约定。
///
/// # 各形态约定
/// - **LinuxSystemd**：数据 `/var/lib/iot-daq`、配置 `/etc/iot-daq`（systemd
///   单元模板见 [`systemd_unit_spec`]）。
/// - **LinuxDocker**：容器内路径与原生一致；**compose 必须把宿主持久卷挂到
///   `/var/lib/iot-daq`（rw）、配置卷挂到 `/etc/iot-daq`（rw），并只读挂载
///   `/etc/machine-id` 与 `/sys/class/dmi/id`（宿主锚点）、`/run/iot-daq`
///   （安装脚本注入的宿主 MAC 文件，`host-mac`）**。授权 / 试用 / 租约状态
///   只允许写 `data_dir`（持久卷），容器可写层不承载任何持久状态。
/// - **Windows**：`%ProgramData%\iot-daq` 下的 `data` / `config` 子目录；
///   文件锚点不适用（`None`）。
pub fn paths(form: RuntimeForm) -> PlatformPaths {
    match form {
        RuntimeForm::LinuxSystemd | RuntimeForm::LinuxDocker => PlatformPaths {
            data_dir: PathBuf::from(LINUX_DATA_DIR),
            config_dir: PathBuf::from(LINUX_CONFIG_DIR),
            anchor_machine_id: Some(PathBuf::from(ANCHOR_MACHINE_ID)),
            anchor_dmi: Some(PathBuf::from(ANCHOR_DMI)),
            anchor_host_mac: Some(PathBuf::from(ANCHOR_HOST_MAC)),
        },
        RuntimeForm::WindowsService | RuntimeForm::WindowsDesktop => {
            let root = windows_program_data_root();
            PlatformPaths {
                data_dir: root.join("data"),
                config_dir: root.join("config"),
                anchor_machine_id: None,
                anchor_dmi: None,
                anchor_host_mac: None,
            }
        }
    }
}

/// Windows 数据根：`%ProgramData%\iot-daq`；env 不可得时回落字面量
/// `C:\ProgramData\iot-daq`（ProgramData 在受支持的目标系统上恒为该路径）。
fn windows_program_data_root() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("iot-daq")
}

/// 创建 [`PlatformPaths`] 中的数据 / 配置目录（幂等；锚点路径只读不创建）。
///
/// # Errors
/// 任一目录创建失败 → [`DaemonError::StorageError`]（含路径与 IO 原因）。
pub fn ensure_dirs(paths: &PlatformPaths) -> DaemonResult<()> {
    for dir in [&paths.data_dir, &paths.config_dir] {
        std::fs::create_dir_all(dir).map_err(|e| {
            DaemonError::StorageError(format!("ensure dir {}: {e}", dir.display()))
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. 宿主锚点挂载校验（容器形态）
// ---------------------------------------------------------------------------

/// 单个锚点的挂载校验结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorMountStatus {
    /// 锚点可用：存在、内容可信、（容器内）符合只读约定。
    Ok,
    /// 锚点缺失 / 不可读 / 内容为占位（视为容器自生成，不可采信）。
    Missing,
    /// 锚点文件可写——不符合只读挂载约定，按容器自生成文件处理（不可采信）。
    ReadOnlyViolation,
}

/// 三类锚点的挂载校验报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorMountReport {
    /// 宿主 machine-id（`/etc/machine-id`）。
    pub machine_id: AnchorMountStatus,
    /// DMI 锚点（`/sys/class/dmi/id/product_uuid` 或 `product_serial`）。
    pub dmi: AnchorMountStatus,
    /// 宿主 MAC 注入文件（`/run/iot-daq/host-mac`）。
    pub mac: AnchorMountStatus,
}

impl AnchorMountReport {
    /// 三项锚点是否全部可用。
    pub fn all_ok(&self) -> bool {
        self.machine_id == AnchorMountStatus::Ok
            && self.dmi == AnchorMountStatus::Ok
            && self.mac == AnchorMountStatus::Ok
    }
}

/// 校验指定形态下的宿主锚点可用性（纯读、零副作用、永不 panic）。
///
/// - **原生形态**（systemd / Windows）：直接采本机锚点，恒三项 [`AnchorMountStatus::Ok`]；
/// - **容器形态**：逐项检查锚点文件的存在性 / 内容可信度 / 可写性，见下。
///
/// # machine-id 的「非容器自生成」判定（可测函数 + 误判风险）
/// 判定顺序：存在 → 内容为 32 个 hex 字符且非全零 → （unix）权限位不可写。
/// **误判风险（已知的诚实局限）**：
/// 1. 权限位来自 inode，不反映挂载点的 `ro` 标志——宿主文件若为 0644，即使
///    `:ro` bind 挂载，std 层面仍显示可写 → 可能**误报** `ReadOnlyViolation`。
///    缓解：compose 规范要求 `:ro` 挂载 + 宿主 machine-id 常规即为 0444；
/// 2. 容器自生成的 machine-id 同样是合法 32-hex，内容层面**无法区分**——
///    可写性是唯一线索；若容器以可写层伪造 0444 文件则**漏报**。
///    最终防线是 `auth::machine_id` 的 N-of-M 锚点配额，本函数只是前置体检，
///    不承担安全判决。
pub fn check_host_anchor_mounts(form: RuntimeForm, root: &Path) -> AnchorMountReport {
    if !form.is_container() {
        return AnchorMountReport {
            machine_id: AnchorMountStatus::Ok,
            dmi: AnchorMountStatus::Ok,
            mac: AnchorMountStatus::Ok,
        };
    }
    AnchorMountReport {
        machine_id: check_machine_id_anchor(root),
        dmi: check_dmi_anchor(root),
        mac: evaluate_anchor_file(&root.join("run/iot-daq/host-mac"), 1),
    }
}

/// machine-id 锚点判定：32-hex 非全零内容 + 不可写（判定语义见模块函数文档）。
fn check_machine_id_anchor(root: &Path) -> AnchorMountStatus {
    match std::fs::read(root.join("etc/machine-id")) {
        Err(_) => AnchorMountStatus::Missing,
        Ok(bytes) => {
            if !is_valid_machine_id(&bytes) {
                // 空 / 全零 / 非 32-hex：容器自生成占位的典型特征，按缺失处理。
                return AnchorMountStatus::Missing;
            }
            writable_anchor_violation(&root.join("etc/machine-id"))
        }
    }
}

/// machine-id 内容校验：恰好 32 个 hex 字符且非全零（systemd 语义）。
fn is_valid_machine_id(bytes: &[u8]) -> bool {
    bytes.len() == 32
        && bytes.iter().all(|b| b.is_ascii_hexdigit())
        && bytes.iter().any(|b| *b != b'0')
}

/// DMI 锚点判定：`product_uuid` 与 `product_serial` 任一可用即 Ok。
fn check_dmi_anchor(root: &Path) -> AnchorMountStatus {
    let uuid = root.join("sys/class/dmi/id/product_uuid");
    let serial = root.join("sys/class/dmi/id/product_serial");
    if evaluate_anchor_file(&uuid, 1) == AnchorMountStatus::Ok
        || evaluate_anchor_file(&serial, 1) == AnchorMountStatus::Ok
    {
        AnchorMountStatus::Ok
    } else if evaluate_anchor_file(&uuid, 1) == AnchorMountStatus::ReadOnlyViolation
        || evaluate_anchor_file(&serial, 1) == AnchorMountStatus::ReadOnlyViolation
    {
        AnchorMountStatus::ReadOnlyViolation
    } else {
        AnchorMountStatus::Missing
    }
}

/// 通用锚点文件判定：可读 + 非空（≥ `min_len` 字节有效内容）+ 不可写。
fn evaluate_anchor_file(path: &Path, min_len: usize) -> AnchorMountStatus {
    match std::fs::read(path) {
        Err(_) => AnchorMountStatus::Missing,
        Ok(bytes) => {
            let plausible = bytes.len() >= min_len && bytes.iter().any(|b| !b.is_ascii_whitespace());
            if !plausible {
                return AnchorMountStatus::Missing;
            }
            writable_anchor_violation(path)
        }
    }
}

/// unix 权限位可写检查：任一写位置位 → [`AnchorMountStatus::ReadOnlyViolation`]，
/// 否则 Ok。非 unix 平台无权限位语义，恒 Ok（局限见模块文档）。
fn writable_anchor_violation(path: &Path) -> AnchorMountStatus {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) if meta.permissions().mode() & 0o222 != 0 => {
                AnchorMountStatus::ReadOnlyViolation
            }
            Ok(_) => AnchorMountStatus::Ok,
            Err(_) => AnchorMountStatus::Missing,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        AnchorMountStatus::Ok
    }
}

// ---------------------------------------------------------------------------
// 4. Linux 原生部署模板（纯函数）
// ---------------------------------------------------------------------------

/// systemd 单元模板（`/etc/systemd/system/iot-daq.service`）。
///
/// 关键字段：`After=network-online.target`（等网络就绪再启动，授权握手依赖
/// NTP 校时与许可服务器连通）、`Restart=on-failure`（异常退出自动拉起）。
/// `Environment=IOT_DAQ_SERVICE_MODE=1` 与 [`SERVICE_MODE_ENV`] 约定呼应。
pub fn systemd_unit_spec() -> String {
    r#"[Unit]
Description=IoT-DAQ Gateway Daemon
Documentation=https://iot-daq.example.local/docs/daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=iot-daq
Group=iot-daq
Environment=IOT_DAQ_SERVICE_MODE=1
ExecStart=/usr/local/bin/iot-daq-daemon --config /etc/iot-daq/gateway.toml
WorkingDirectory=/var/lib/iot-daq
StateDirectory=iot-daq
Restart=on-failure
RestartSec=5
# 优雅停机：SIGTERM 触发 daemon 的 graceful shutdown（宽限期见 bootstrap）。
KillSignal=SIGTERM
TimeoutStopSec=35

[Install]
WantedBy=multi-user.target
"#
    .to_string()
}

/// udev 规则模板（`/etc/udev/rules.d/99-iot-daq-serial.rules`）。
///
/// 目标：让 `iot-daq` 组用户无需 root 即可访问串口 / USB-串口适配器；
/// `TAG+="systemd"` 便于设备单元与 daemon 依赖编排。
pub fn udev_rules_spec() -> String {
    r#"# iot-daq serial device access rules
# 串口设备（tty / USB-串口适配器）：iot-daq 组可读写。
SUBSYSTEM=="tty", MODE="0660", GROUP="iot-daq", TAG+="systemd"
SUBSYSTEM=="usb", ATTR{bInterfaceClass}=="ff", MODE="0660", GROUP="iot-daq", TAG+="systemd"
# 常见 USB-串口芯片厂商（FTDI / Prolific / WCH / Silicon Labs）。
SUBSYSTEM=="usb-serial", MODE="0660", GROUP="iot-daq", TAG+="systemd"
KERNEL=="ttyUSB*", MODE="0660", GROUP="iot-daq", TAG+="systemd"
KERNEL=="ttyACM*", MODE="0660", GROUP="iot-daq", TAG+="systemd"
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// 测试（task 57 切片 1 规格：8-14 项）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// T01 当前宿主可检测且自洽：Windows 上必为两 Windows 形态之一。
    #[test]
    fn detect_on_current_host_is_consistent() {
        let d = detect();
        #[cfg(windows)]
        assert!(
            d.form == RuntimeForm::WindowsService || d.form == RuntimeForm::WindowsDesktop,
            "windows host must resolve to a windows form, got {:?}",
            d.form
        );
        #[cfg(not(windows))]
        assert!(
            d.form == RuntimeForm::LinuxSystemd || d.form == RuntimeForm::LinuxDocker,
            "linux host must resolve to a linux form, got {:?}",
            d.form
        );
    }

    /// T02 容器标记判定参数化：任一存在 → Docker(High)；双确认不存在 →
    /// systemd(High)；探测失败 → systemd(Low)（回落保守值 + confidence）。
    #[test]
    fn docker_marker_verdicts_parametrized() {
        let docker = docker_verdict(Some(true), Some(false));
        assert_eq!(docker.form, RuntimeForm::LinuxDocker);
        assert_eq!(docker.confidence, DetectionConfidence::High);

        let docker2 = docker_verdict(None, Some(true));
        assert_eq!(docker2.form, RuntimeForm::LinuxDocker);
        assert_eq!(docker2.confidence, DetectionConfidence::High);

        let native = docker_verdict(Some(false), Some(false));
        assert_eq!(native.form, RuntimeForm::LinuxSystemd);
        assert_eq!(native.confidence, DetectionConfidence::High);

        let fallback = docker_verdict(None, None);
        assert_eq!(fallback.form, RuntimeForm::LinuxSystemd);
        assert_eq!(fallback.confidence, DetectionConfidence::Low);
    }

    /// T03 服务模式环境变量解析：仅 `1` 生效；缺失 / 其它值均非服务模式。
    #[test]
    fn service_mode_env_value_parsing() {
        assert!(service_mode_from_env_value(Some("1")));
        assert!(!service_mode_from_env_value(Some("0")));
        assert!(!service_mode_from_env_value(Some("")));
        assert!(!service_mode_from_env_value(Some("on")));
        assert!(!service_mode_from_env_value(None));
    }

    /// T04 Linux 原生路径约定：/var/lib/iot-daq + /etc/iot-daq + 文件锚点齐全。
    #[test]
    fn paths_linux_native_layout() {
        let p = paths(RuntimeForm::LinuxSystemd);
        assert_eq!(p.data_dir, PathBuf::from("/var/lib/iot-daq"));
        assert_eq!(p.config_dir, PathBuf::from("/etc/iot-daq"));
        assert_eq!(
            p.anchor_machine_id,
            Some(PathBuf::from("/etc/machine-id"))
        );
        assert_eq!(p.anchor_dmi, Some(PathBuf::from("/sys/class/dmi/id")));
        assert!(p.anchor_host_mac.is_some());
    }

    /// T05 Docker 路径与原生一致的容器内约定（宿主持久卷挂载点），
    /// 且 `is_container` 语义正确。
    #[test]
    fn paths_linux_docker_layout_matches_native_convention() {
        let p = paths(RuntimeForm::LinuxDocker);
        assert_eq!(p.data_dir, PathBuf::from("/var/lib/iot-daq"));
        assert_eq!(p.config_dir, PathBuf::from("/etc/iot-daq"));
        assert_eq!(
            p.anchor_machine_id,
            Some(PathBuf::from("/etc/machine-id"))
        );
        assert!(RuntimeForm::LinuxDocker.is_container());
        assert!(!RuntimeForm::LinuxSystemd.is_container());
    }

    /// T06 Windows 路径：ProgramData\iot-daq 下的 data / config；文件锚点为 None。
    #[cfg(windows)]
    #[test]
    fn paths_windows_program_data() {
        let p = paths(RuntimeForm::WindowsDesktop);
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        assert!(p.data_dir.starts_with(pd.clone()));
        assert!(p.config_dir.starts_with(pd));
        assert!(p.data_dir.ends_with("data"));
        assert!(p.config_dir.ends_with("config"));
        assert_eq!(p.anchor_machine_id, None);
        assert_eq!(p.anchor_dmi, None);
        assert_eq!(p.anchor_host_mac, None);
        // 服务形态与桌面形态共用同一路径约定。
        assert_eq!(paths(RuntimeForm::WindowsService), p);
    }

    /// T07 ensure_dirs 幂等创建 data / config 目录。
    #[test]
    fn ensure_dirs_creates_data_and_config() {
        let dir = TempDir::new().expect("tempdir");
        let p = PlatformPaths {
            data_dir: dir.path().join("data"),
            config_dir: dir.path().join("etc"),
            anchor_machine_id: None,
            anchor_dmi: None,
            anchor_host_mac: None,
        };
        ensure_dirs(&p).expect("ensure ok");
        assert!(p.data_dir.is_dir());
        assert!(p.config_dir.is_dir());
        // 幂等：重复调用仍 Ok。
        ensure_dirs(&p).expect("ensure idempotent");
    }

    /// T08 ensure_dirs IO 失败收敛为 StorageError（父路径是文件 → 创建必失败）。
    #[test]
    fn ensure_dirs_reports_io_error_as_storage_error() {
        let dir = TempDir::new().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let p = PlatformPaths {
            data_dir: blocker.join("data"),
            config_dir: dir.path().join("etc"),
            anchor_machine_id: None,
            anchor_dmi: None,
            anchor_host_mac: None,
        };
        let err = ensure_dirs(&p).expect_err("must fail");
        assert!(matches!(err, DaemonError::StorageError(_)));
        assert_eq!(err.error_code(), crate::error::ERR_STORAGE);
    }

    /// T09 原生形态：锚点报告恒三项 Ok（不依赖宿主文件系统）。
    #[test]
    fn anchor_report_native_form_all_ok() {
        for form in [
            RuntimeForm::LinuxSystemd,
            RuntimeForm::WindowsService,
            RuntimeForm::WindowsDesktop,
        ] {
            let r = check_host_anchor_mounts(form, Path::new("/definitely/not/a/root"));
            assert_eq!(r.machine_id, AnchorMountStatus::Ok, "form {form:?}");
            assert!(r.all_ok(), "native form {form:?} must be all Ok");
        }
    }

    /// T10 容器形态：machine-id 缺失 → Missing；DMI / MAC 缺失 → Missing。
    #[test]
    fn anchor_report_container_missing_when_root_empty() {
        let dir = TempDir::new().expect("tempdir");
        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.machine_id, AnchorMountStatus::Missing);
        assert_eq!(r.dmi, AnchorMountStatus::Missing);
        assert_eq!(r.mac, AnchorMountStatus::Missing);
        assert!(!r.all_ok());
    }

    /// T11 容器自生成占位 machine-id（全零 / 非 hex / 长度不对）→ Missing。
    #[test]
    fn anchor_machine_id_placeholder_content_is_missing() {
        let dir = TempDir::new().expect("tempdir");
        let etc = dir.path().join("etc");
        std::fs::create_dir_all(&etc).expect("mkdir etc");

        for bad in ["0".repeat(32), "x".repeat(32), "abcd".to_string()] {
            std::fs::write(etc.join("machine-id"), &bad).expect("write");
            let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
            assert_eq!(
                r.machine_id,
                AnchorMountStatus::Missing,
                "placeholder {bad:?} must not be trusted"
            );
        }
        // 合法 32-hex（非全零）在无权限位语义的平台上视为 Ok。
        std::fs::write(etc.join("machine-id"), "a".repeat(32)).expect("write");
        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.machine_id, AnchorMountStatus::Ok);
    }

    /// T12（unix）可写 machine-id → ReadOnlyViolation；0444 只读 → Ok。
    #[cfg(unix)]
    #[test]
    fn anchor_machine_id_writability_verdict() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let etc = dir.path().join("etc");
        std::fs::create_dir_all(&etc).expect("mkdir etc");
        let path = etc.join("machine-id");

        // 0644 可写 → 容器自生成特征 → ReadOnlyViolation。
        std::fs::write(&path, "b".repeat(32)).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.machine_id, AnchorMountStatus::ReadOnlyViolation);

        // 0444 只读 → 符合只读挂载约定 → Ok。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");
        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.machine_id, AnchorMountStatus::Ok);
    }

    /// T13 容器形态 MAC / DMI 注入文件存在且非空 → Ok。
    #[test]
    fn anchor_mac_and_dmi_present_ok() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("run/iot-daq")).expect("mkdir run");
        std::fs::create_dir_all(dir.path().join("sys/class/dmi/id")).expect("mkdir dmi");
        std::fs::write(dir.path().join("run/iot-daq/host-mac"), "aa:bb:cc:dd:ee:ff")
            .expect("write mac");
        std::fs::write(
            dir.path().join("sys/class/dmi/id/product_uuid"),
            "uuid-bbbb",
        )
        .expect("write uuid");

        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.mac, AnchorMountStatus::Ok);
        assert_eq!(r.dmi, AnchorMountStatus::Ok);

        // 空 MAC 文件 → Missing（占位不可信）。
        std::fs::write(dir.path().join("run/iot-daq/host-mac"), "  \n").expect("write blank");
        let r = check_host_anchor_mounts(RuntimeForm::LinuxDocker, dir.path());
        assert_eq!(r.mac, AnchorMountStatus::Missing);
    }

    /// T14 systemd 单元模板关键字段：After / Restart / ExecStart / Install /
    /// 服务模式环境变量约定。
    #[test]
    fn systemd_unit_spec_contains_required_fields() {
        let unit = systemd_unit_spec();
        for needle in [
            "After=network-online.target",
            "Wants=network-online.target",
            "Restart=on-failure",
            "ExecStart=",
            "WantedBy=multi-user.target",
            "Environment=IOT_DAQ_SERVICE_MODE=1",
            "StateDirectory=iot-daq",
        ] {
            assert!(unit.contains(needle), "unit missing {needle:?}");
        }
    }

    /// T15 udev 规则模板关键字段：SUBSYSTEM / MODE / GROUP / systemd tag。
    #[test]
    fn udev_rules_spec_contains_required_fields() {
        let rules = udev_rules_spec();
        for needle in [
            "SUBSYSTEM==\"tty\"",
            "MODE=\"0660\"",
            "GROUP=\"iot-daq\"",
            "TAG+=\"systemd\"",
            "ttyUSB*",
        ] {
            assert!(rules.contains(needle), "rules missing {needle:?}");
        }
    }
}
